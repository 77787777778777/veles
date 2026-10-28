//! Session mail between pool nodes (PLAN-sessions-and-phone.md S4,
//! round S-b): a session on the laptop mails the phone's session, and the
//! phone's answer comes back.
//!
//! The mail itself is `crate::mail` on the RECEIVING node — queued there,
//! nudged there, claimed and clamped there. This module is only the hop:
//! one A2A message framed like the other cluster protocols (marker +
//! base64 + sha256, see `remotetask.rs`), intercepted by the peer's
//! a2a_server before any session or turn, and answered inline.
//!
//! Three operations ride the one marker:
//!
//! - `send` — enqueue the text into a session there; with `wait_secs`,
//!   answer with the turn's reply.
//! - `check` — a mail's state and reply, for the node that sent it.
//! - `list` — the live sessions there (`mail::live`).
//!
//! Trust (USER decision D2): the receiver gates on `[cluster] task_peers`
//! — being a sync peer or holding a token does not buy turns — and the
//! sender's rights are capped at what the PEER holds here (`a2a:<peer>`
//! in the ACL, User by default). The principal the ask claims can only
//! LOWER that, never raise it: the turn runs with the lowest of the
//! claimed sender, the peer, and the target session. An owner session on
//! the laptop mailing the phone therefore gets a User-level turn there
//! unless the owner named the peer higher.
//!
//! The exchange's chain id and hop travel with the ask, so the loop guard
//! (`mail::MAX_HOPS`) holds across nodes as it does inside one.

use crate::acl::{Cap, Principal};
use crate::lockutil::LockOk;
use crate::mail::{self, Delivery, Sender, Waited};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The marker every session-mail ask starts with.
pub const MARKER: &str = "[veles session mail]";
/// The reply marker.
pub const REPLY_MARKER: &str = "[veles session mail reply]";

/// How long one `list` or `check` may take.
const QUICK: Duration = Duration::from_secs(10);

pub fn is_ask(raw: &str) -> bool {
    raw.trim_start().starts_with(MARKER) && !is_reply(raw)
}

pub fn is_reply(raw: &str) -> bool {
    raw.trim_start().starts_with(REPLY_MARKER)
}

const FENCE: &str = "```b64";

fn b64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s.trim())?)
}

fn sha_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn fence(marker: &str, json: &str) -> String {
    format!(
        "{marker}\n{FENCE}\nsha256:{}\n{}\n```",
        sha_hex(json.as_bytes()),
        b64_encode(json.as_bytes())
    )
}

/// The JSON inside a fenced message, its hash checked.
pub(crate) fn unfence(raw: &str) -> Result<String> {
    let mut in_fence = false;
    let mut sha = String::new();
    let mut b64 = String::new();
    for l in raw.trim().lines() {
        let t = l.trim();
        if !in_fence {
            in_fence = t == FENCE;
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
        bail!("no {FENCE} fence with sha256 + payload found");
    }
    let bytes = b64_decode(&b64)?;
    if sha_hex(&bytes) != sha {
        bail!("payload hash does not match");
    }
    Ok(String::from_utf8(bytes)?)
}

/// The ask, as it travels.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ask {
    /// `send` | `check` | `list`
    pub op: String,
    /// send: the session id on the receiving node.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_title: Option<String>,
    /// The sender's principal on ITS node, as JSON; `None` = an owner
    /// surface there. It can only lower what the peer holds here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<String>,
    #[serde(default)]
    pub hop: i64,
    #[serde(default)]
    pub wait_secs: u64,
    /// check: the mail id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<String>,
}

pub fn encode_ask(ask: &Ask) -> Result<String> {
    Ok(fence(MARKER, &serde_json::to_string(ask)?))
}

pub fn decode_ask(raw: &str) -> Result<Ask> {
    serde_json::from_str(&unfence(raw)?).context("payload is not a session-mail ask")
}

pub fn encode_reply(v: &Value) -> Result<String> {
    Ok(fence(REPLY_MARKER, &serde_json::to_string(v)?))
}

pub fn decode_reply(raw: &str) -> Result<Value> {
    serde_json::from_str(&unfence(raw)?).context("payload is not a session-mail reply")
}

/// The lower of two principals: the lower role, and the capabilities
/// both hold. The subject is the lower one's.
pub fn lowest(a: &Principal, b: &Principal) -> Principal {
    let (lo, hi) = if a.role <= b.role { (a, b) } else { (b, a) };
    let mut out = lo.clone();
    out.caps = if hi.has(Cap::All) {
        lo.caps.clone()
    } else if lo.has(Cap::All) {
        hi.caps.clone()
    } else {
        lo.caps.intersection(&hi.caps).copied().collect()
    };
    out
}

/// Answer one ask from `peer`, whose own standing here is
/// `peer_principal`. Called by the a2a_server before any session or turn.
pub async fn receive(
    store: &Arc<Mutex<Store>>,
    task_peers: &[String],
    peer: &str,
    peer_principal: &Principal,
    ask: Ask,
) -> Value {
    if !task_peers.iter().any(|p| p == peer) {
        return json!({
            "status": "refused",
            "error": format!("'{peer}' is not in [cluster] task_peers here"),
        });
    }
    match ask.op.as_str() {
        "list" => {
            let sessions: Vec<Value> = mail::live(store)
                .into_iter()
                .map(|l| {
                    json!({
                        "session_id": l.session_id, "title": l.title, "surface": l.surface,
                        "running": l.running, "held": l.held,
                    })
                })
                .collect();
            json!({"status": "ok", "sessions": sessions})
        }
        "check" => {
            let Some(id) = ask.mail.as_deref() else {
                return json!({"status": "refused", "error": "check needs a mail id"});
            };
            match store.lock_ok().mail_get(id) {
                Ok(Some(row)) if row.via_peer.as_deref() == Some(peer) => json!({
                    "status": "ok", "id": row.id, "to": row.to_session,
                    "state": row.state, "reply": row.reply,
                }),
                Ok(_) => {
                    json!({"status": "refused", "error": format!("no mail {id} from {peer} here")})
                }
                Err(e) => json!({"status": "error", "error": format!("{e:#}")}),
            }
        }
        "send" => send_here(store, peer, peer_principal, ask).await,
        other => json!({"status": "refused", "error": format!("unknown op {other:?}")}),
    }
}

async fn send_here(
    store: &Arc<Mutex<Store>>,
    peer: &str,
    peer_principal: &Principal,
    ask: Ask,
) -> Value {
    // The rights this mail carries: what the peer holds here, lowered by
    // whatever the sender held on its own node. A claim that does not
    // parse is a GUEST, never the peer's full standing.
    let claimed = ask
        .principal
        .as_deref()
        .map(|j| serde_json::from_str::<Principal>(j).unwrap_or_else(|_| Principal::guest()));
    let rights = match &claimed {
        Some(c) => lowest(c, peer_principal),
        None => peer_principal.clone(),
    };
    let from = Sender {
        session: ask.from_session.clone(),
        title: ask.from_title.clone(),
        principal: Some(rights),
    };
    let relay = mail::Relay {
        peer: peer.to_string(),
        chain_id: ask.chain_id.clone(),
        hop: ask.hop,
    };
    let sent = match mail::send_relayed(store, &from, &relay, &ask.to, &ask.text) {
        Ok(s) => s,
        Err(e) => return json!({"status": "refused", "error": format!("{e:#}")}),
    };
    let delivery = match sent.delivery {
        Delivery::Here | Delivery::Process(_) => "held",
        Delivery::Queued => "queued",
    };
    let mut out = json!({
        "status": "ok", "id": sent.id, "delivery": delivery, "state": "queued",
    });
    if ask.wait_secs > 0 && sent.delivery != Delivery::Queued {
        let secs = ask.wait_secs.min(crate::sessions_tool::MAX_WAIT_SECS);
        match mail::wait(store, &sent.id, Duration::from_secs(secs)).await {
            Ok(Waited::Answered(r)) => {
                out["state"] = json!("answered");
                out["reply"] = json!(r);
            }
            Ok(Waited::Failed(e)) => {
                out["state"] = json!("failed");
                out["reply"] = json!(e);
            }
            Ok(Waited::Pending(state)) => out["state"] = json!(state),
            Err(e) => {
                out["state"] = json!("unknown");
                out["reply"] = json!(format!("{e:#}"));
            }
        }
    }
    out
}

/// Where a node's sessions are reached: its `[a2a_agents]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The name this node knows it by (the `[a2a_agents]` key).
    pub name: String,
    pub url: String,
    pub bearer: Option<String>,
}

/// Resolve a node reference — an `[a2a_agents]` name, or a registry node
/// by name, id or id prefix — to how it is dialled.
pub fn remote(store: &Store, cfg: &crate::config::VelesConfig, node: &str) -> Option<Remote> {
    let bearer_of = |a: &crate::config::A2APeer| {
        let t = a.auth.token.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    if let Some(a) = cfg.a2a_agents.get(node) {
        return Some(Remote {
            name: node.to_string(),
            url: a.url.clone(),
            bearer: bearer_of(a),
        });
    }
    let row = match store.node_resolve(node).ok()? {
        crate::store::NodeMatch::One(r) => *r,
        _ => store
            .nodes_list()
            .ok()?
            .into_iter()
            .find(|n| n.name == node)?,
    };
    for (name, a) in cfg.a2a_agents.iter() {
        let by_endpoint = row
            .endpoint
            .as_deref()
            .is_some_and(|ep| a.url.trim_end_matches('/') == ep.trim_end_matches('/'));
        if by_endpoint || name == &row.name {
            return Some(Remote {
                name: name.clone(),
                url: a.url.clone(),
                bearer: bearer_of(a),
            });
        }
    }
    None
}

/// Split `node/session` (the cross-node address). `None` when the
/// reference has no node part.
pub fn split_address(reference: &str) -> Option<(&str, &str)> {
    let (node, sid) = reference.trim().split_once('/')?;
    (!node.is_empty() && !sid.is_empty() && !sid.contains('/')).then_some((node, sid))
}

async fn call(remote: &Remote, ask: &Ask, timeout: Duration) -> Result<Value> {
    let text = encode_ask(ask)?;
    let client = crate::a2a::A2AClient::with_timeout(timeout);
    let ctx = crate::a2a::new_context_id();
    let result = client
        .call(&remote.url, remote.bearer.as_deref(), &text, &ctx)
        .await
        .with_context(|| format!("node {} did not answer", remote.name))?;
    let reply = crate::a2a::extract_text(&result);
    if !is_reply(&reply) {
        let state = result["status"]["state"].as_str().unwrap_or("?");
        bail!(
            "node {} did not answer session mail ({state}): {reply}",
            remote.name
        );
    }
    let v = decode_reply(&reply)?;
    if v["status"] != "ok" {
        bail!(
            "node {}: {}",
            remote.name,
            v["error"].as_str().unwrap_or("refused")
        );
    }
    Ok(v)
}

/// What a remote send came back with.
#[derive(Debug, Clone)]
pub struct RemoteSent {
    pub id: String,
    /// The receiving node holds the session (it runs now or after its
    /// current turn), or nobody does and it waits there.
    pub held: bool,
    /// queued | running | answered | failed — as of the answer.
    pub state: String,
    pub reply: Option<String>,
}

/// Send `text` from `from` into session `to` on `remote`. The exchange
/// continues the sending turn's chain when it is a mail turn.
pub async fn send(
    remote: &Remote,
    from: &Sender,
    to: &str,
    text: &str,
    wait_secs: u64,
) -> Result<RemoteSent> {
    let (chain_id, hop) = match from.session.as_deref().and_then(mail::current) {
        Some(c) => (Some(c.chain_id), c.hop + 1),
        None => (None, 0),
    };
    if hop > mail::MAX_HOPS {
        bail!(
            "this mail would be hop {hop} of one chain of messages between sessions, and a \
             chain stops at {}. Answer in your own turn instead of sending more mail.",
            mail::MAX_HOPS
        );
    }
    let ask = Ask {
        op: "send".into(),
        to: to.to_string(),
        text: text.to_string(),
        from_session: from.session.clone(),
        from_title: from.title.clone(),
        principal: match &from.principal {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        },
        chain_id,
        hop,
        wait_secs,
        mail: None,
    };
    let v = call(remote, &ask, Duration::from_secs(wait_secs) + QUICK).await?;
    Ok(RemoteSent {
        id: v["id"].as_str().unwrap_or_default().to_string(),
        held: v["delivery"] == "held",
        state: v["state"].as_str().unwrap_or("queued").to_string(),
        reply: v["reply"].as_str().map(str::to_string),
    })
}

/// A mail this node sent to `remote`: its state there, and its reply.
pub async fn check(remote: &Remote, id: &str) -> Result<(String, Option<String>)> {
    let ask = Ask {
        op: "check".into(),
        mail: Some(id.to_string()),
        ..Default::default()
    };
    let v = call(remote, &ask, QUICK).await?;
    Ok((
        v["state"].as_str().unwrap_or("?").to_string(),
        v["reply"].as_str().map(str::to_string),
    ))
}

/// The live sessions on `remote`.
pub async fn list(remote: &Remote) -> Result<Vec<mail::LiveSession>> {
    let ask = Ask {
        op: "list".into(),
        ..Default::default()
    };
    let v = call(remote, &ask, QUICK).await?;
    Ok(v["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| mail::LiveSession {
            session_id: s["session_id"].as_str().unwrap_or_default().to_string(),
            title: s["title"].as_str().unwrap_or_default().to_string(),
            surface: s["surface"].as_str().unwrap_or_default().to_string(),
            pid: None,
            running: s["running"].as_bool().unwrap_or(false),
            held: s["held"].as_bool().unwrap_or(false),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::Role;

    fn store() -> (Arc<Mutex<Store>>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("state.db")).unwrap();
        (Arc::new(Mutex::new(st)), dir)
    }

    #[test]
    fn an_ask_round_trips_and_a_tampered_one_is_refused() {
        let ask = Ask {
            op: "send".into(),
            to: "20261011_101010_abcdef".into(),
            text: "hello\nthere".into(),
            from_session: Some("s1".into()),
            hop: 2,
            ..Default::default()
        };
        let wire = encode_ask(&ask).unwrap();
        assert!(is_ask(&wire) && !is_reply(&wire));
        let back = decode_ask(&wire).unwrap();
        assert_eq!(
            (back.to.as_str(), back.text.as_str(), back.hop),
            (ask.to.as_str(), ask.text.as_str(), 2)
        );
        // Flip one payload character: the hash no longer matches.
        let b64_line = wire.lines().nth(3).unwrap().to_string();
        let mut chars: Vec<char> = b64_line.chars().collect();
        chars[4] = if chars[4] == 'A' { 'B' } else { 'A' };
        let bad = wire.replace(&b64_line, &chars.into_iter().collect::<String>());
        assert!(decode_ask(&bad).is_err());
        let reply = encode_reply(&json!({"status": "ok"})).unwrap();
        assert!(is_reply(&reply) && !is_ask(&reply));
    }

    #[test]
    fn the_lowest_of_two_principals_never_rises() {
        let owner = Principal::owner();
        let user = Principal::known("a2a:laptop", Role::User);
        let admin = Principal::known("telegram:1", Role::Admin);
        assert_eq!(lowest(&owner, &user).role, Role::User);
        assert_eq!(lowest(&user, &owner).role, Role::User);
        assert!(
            !lowest(&owner, &user).has(Cap::All),
            "never the owner's All"
        );
        assert_eq!(lowest(&admin, &user).role, Role::User);
        let no_web = Principal::known("a2a:laptop", Role::User).without(Cap::Web);
        assert!(!lowest(&user, &no_web).has(Cap::Web));
    }

    #[test]
    fn addresses_split_on_one_slash() {
        assert_eq!(split_address("laptop/2026_x"), Some(("laptop", "2026_x")));
        assert_eq!(split_address("2026_x"), None);
        assert_eq!(split_address("/2026_x"), None);
        assert_eq!(split_address("a/b/c"), None);
    }

    #[tokio::test]
    async fn a_peer_outside_task_peers_is_refused_and_a_listed_one_is_capped() {
        // Delivery knocks on the data dir's sockets: never a real one.
        let _home = crate::testenv::scratch_home();
        let (st, _d) = store();
        st.lock_ok().create_session("net-target").unwrap();
        let peer_user = Principal::known("a2a:laptop", Role::User);
        let ask = Ask {
            op: "send".into(),
            to: "net-target".into(),
            text: "hi from the laptop".into(),
            from_session: Some("laptop-s".into()),
            ..Default::default()
        };
        let v = receive(&st, &["phone".into()], "laptop", &peer_user, ask.clone()).await;
        assert_eq!(v["status"], "refused", "{v}");
        assert!(st.lock_ok().queue_items("net-target").unwrap().is_empty());

        // Listed: queued here, and the sender's claim of OWNER (None)
        // still runs at what the PEER holds — a User.
        let v = receive(&st, &["laptop".into()], "laptop", &peer_user, ask).await;
        assert_eq!(v["status"], "ok", "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        let row = st.lock_ok().mail_get(&id).unwrap().unwrap();
        assert_eq!(row.via_peer.as_deref(), Some("laptop"));
        let p: Principal = serde_json::from_str(row.from_principal.as_deref().unwrap()).unwrap();
        assert_eq!(p.role, Role::User);
        assert!(row.from_pid.is_none(), "no local process waits on it");

        // `check` answers the peer that sent it, and nobody else.
        let check = |peer: &'static str| {
            let st = st.clone();
            let id = id.clone();
            async move {
                receive(
                    &st,
                    &["laptop".into(), "other".into()],
                    peer,
                    &Principal::known("a2a:x", Role::User),
                    Ask {
                        op: "check".into(),
                        mail: Some(id),
                        ..Default::default()
                    },
                )
                .await
            }
        };
        assert_eq!(check("laptop").await["state"], "queued");
        assert_eq!(check("other").await["status"], "refused");
    }

    #[tokio::test]
    async fn a_claimed_principal_only_lowers_and_garbage_is_a_guest() {
        // Delivery knocks on the data dir's sockets: never a real one.
        let _home = crate::testenv::scratch_home();
        let (st, _d) = store();
        st.lock_ok().create_session("net-claim").unwrap();
        let send = |peer: Principal, principal: Option<String>| {
            let st = st.clone();
            async move {
                let v = receive(
                    &st,
                    &["laptop".into()],
                    "laptop",
                    &peer,
                    Ask {
                        op: "send".into(),
                        to: "net-claim".into(),
                        text: "x".into(),
                        principal,
                        ..Default::default()
                    },
                )
                .await;
                let id = v["id"].as_str().unwrap().to_string();
                let row = st.lock_ok().mail_get(&id).unwrap().unwrap();
                serde_json::from_str::<Principal>(row.from_principal.as_deref().unwrap())
                    .unwrap()
                    .role
            }
        };
        let owner = Principal::owner();
        let peer_user = Principal::known("a2a:laptop", Role::User);
        let trusted =
            serde_json::to_string(&Principal::known("telegram:5", Role::Trusted)).unwrap();
        assert_eq!(
            send(owner.clone(), Some(trusted)).await,
            Role::Trusted,
            "a lower claim lowers"
        );
        // A claim ABOVE what the peer holds here buys nothing: a peer
        // that says its sender is the owner is still a User here.
        let claims_owner = serde_json::to_string(&Principal::owner()).unwrap();
        assert_eq!(
            send(peer_user, Some(claims_owner)).await,
            Role::User,
            "a higher claim never raises the peer"
        );
        assert_eq!(send(owner, Some("{garbage".into())).await, Role::Guest);
    }

    #[tokio::test]
    async fn the_hop_cap_holds_across_nodes() {
        // Delivery knocks on the data dir's sockets: never a real one.
        let _home = crate::testenv::scratch_home();
        let (st, _d) = store();
        st.lock_ok().create_session("net-hop").unwrap();
        let v = receive(
            &st,
            &["laptop".into()],
            "laptop",
            &Principal::known("a2a:laptop", Role::User),
            Ask {
                op: "send".into(),
                to: "net-hop".into(),
                text: "deep".into(),
                chain_id: Some("mabcdefabcdefabcd".into()),
                hop: mail::MAX_HOPS + 1,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(v["status"], "refused", "{v}");
    }
}
