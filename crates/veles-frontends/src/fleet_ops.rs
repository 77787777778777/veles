//! The fleet command center's node and sync operations — what the
//! desktop's `/api/fleet*` routes and the `/fleet` chat command share.
//!
//! WHAT IS HERE (FLEET-5/FLEET-6, review of 2026-11-06). The owner asked
//! for "all kinds of sync" and "everything else a command center needs";
//! the first cut had store push/apply only and threw the sync state it
//! fetched away. This module is the honest increment:
//!
//! - status for every sync kind the gateway runs — store (`storesync`),
//!   memory (`memsync`), config (`configsync`) and vault replication —
//!   per peer: configured, automatic or not, cursor vs. this node's high
//!   water mark, the automatic loop's backoff (failures, next retry) and
//!   the last result this process saw (explicit or automatic);
//! - an explicit push or inbox apply per kind, through the SAME core
//!   calls and meta cursors the automatic loops use, with the same
//!   backoff keys, so an explicit pass and the timer agree;
//! - per-node health: heartbeat staleness AND reachability (a failing
//!   sync kind, or an explicit probe of the configured A2A card) — a
//!   stale node and an unreachable one are different problems;
//! - forget for a stale registry row.
//!
//! THE RULES. A node's registry row supplies IDENTITY only. Every dial
//! uses the operator's `[a2a_agents]` URL and bearer for the configured
//! peer name; the registry's self-reported endpoint is never dialed. An
//! operation that changes this node's or a peer's config, vault or
//! memory needs `confirmed` — the desktop asks before it sends it.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use veles_core::config::VelesConfig;
use veles_core::lockutil::LockOk;
use veles_core::store::{NodeRow, Store};
use veles_core::storesync::OperationError;

/// The sync kinds, in display order.
pub const KINDS: [&str; 4] = ["store", "memory", "config", "vault"];

/// Is `kind` one of [`KINDS`]?
pub fn valid_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// The configured peer name `node` answers to for `kind`. Store sync
/// trusts `[cluster] storesync_peers`; memory, config and vault ride
/// `memsync_peers` (the gateway's `configsync_push`/`vaultsync_push`
/// walk that list too).
pub fn peer_for<'a>(cfg: &'a VelesConfig, kind: &str, node: &NodeRow) -> Option<&'a str> {
    let list = if kind == "store" {
        &cfg.cluster.storesync_peers
    } else {
        &cfg.cluster.memsync_peers
    };
    list.iter()
        .find(|p| p.as_str() == node.name || p.as_str() == node.id)
        .map(String::as_str)
}

/// Does the automatic loop run `kind`?
fn auto_for(cfg: &VelesConfig, kind: &str) -> bool {
    match kind {
        "store" => cfg.cluster.storesync_auto,
        "memory" => cfg.cluster.memsync_auto,
        _ => cfg.cluster.configsync_auto,
    }
}

/// The automatic loops' backoff key for (kind, peer) — gateway.rs uses
/// these spellings.
pub fn backoff_key(kind: &str, peer: &str) -> String {
    let purpose = match kind {
        "store" => "storesync",
        "memory" => "memsync",
        "config" => "configsync",
        "vault" => "vaultsync",
        other => other,
    };
    format!("{purpose}:{peer}")
}

/// Do these operations change data a person may want to look at first?
/// A store push lands in the peer's inbox and a store apply writes only
/// per-sender replicas — neither touches a primary. The others merge
/// into (or send something that merges into) memory, config.toml or a
/// vault.
pub fn needs_confirmation(kind: &str, operation: &str) -> bool {
    !matches!((kind, operation), ("store", _) | ("memory", "push"))
}

// ── last results ─────────────────────────────────────────────────────

type LastMap = BTreeMap<String, (String, bool, String)>;

fn last_map() -> &'static Mutex<LastMap> {
    static LAST: std::sync::OnceLock<Mutex<LastMap>> = std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
}

/// Record the outcome of a pass at (kind, peer) — explicit or automatic.
/// Process-local, like the backoff: a restart forgets it. The line is a
/// status sentence from the sync code, never a payload.
pub fn record(kind: &str, peer: &str, outcome: Result<&str, &str>) {
    let (ok, line) = match outcome {
        Ok(l) => (true, l),
        Err(e) => (false, e),
    };
    let line: String = line
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(240)
        .collect();
    last_map().lock_ok().insert(
        format!("{kind}:{peer}"),
        (chrono::Utc::now().to_rfc3339(), ok, line),
    );
}

fn last(kind: &str, peer: &str) -> Value {
    match last_map().lock_ok().get(&format!("{kind}:{peer}")) {
        Some((at, ok, line)) => json!({"at": at, "ok": ok, "line": line}),
        None => Value::Null,
    }
}

// ── status ───────────────────────────────────────────────────────────

/// This node's high-water marks, read once per inventory.
pub struct Local {
    store_seq: Option<i64>,
    memory_rev: Option<i64>,
    /// This node's id while config sync is on: "behind" a config peer
    /// is counted from its own rows, per peer (see `node_sync_status`).
    config_node: Option<String>,
    vault_digest: Option<String>,
}

impl Local {
    pub fn read(store: &Store, cfg: &VelesConfig) -> Self {
        let sync_memory = !cfg.cluster.memsync_peers.is_empty();
        Self {
            store_seq: (!cfg.cluster.storesync_peers.is_empty())
                .then(|| veles_core::storesync::max_seq(store).ok())
                .flatten(),
            memory_rev: sync_memory
                .then(|| veles_core::memory::MemoryStore::new().latest_rev_id().ok())
                .flatten(),
            config_node: sync_memory
                .then(|| {
                    veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
                        .ok()
                        .flatten()
                        .map(|identity| identity.id)
                })
                .flatten(),
            vault_digest: sync_memory
                .then(|| {
                    veles_core::vault::Vault::open_local_if_any()
                        .ok()
                        .flatten()
                        .filter(|v| v.is_initialized())
                        .map(|v| veles_core::vault::rows_digest(&v.file))
                })
                .flatten(),
        }
    }
}

fn meta_i64(store: &Store, key: &str) -> i64 {
    store
        .meta_get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// One node's sync status: a map kind → status for every kind it takes
/// part in. Kinds it is not a peer for are absent.
pub fn node_sync_status(
    store: &Store,
    cfg: &VelesConfig,
    local: &Local,
    node: &NodeRow,
    now: Instant,
) -> Value {
    let mut out = serde_json::Map::new();
    for kind in KINDS {
        let Some(peer) = peer_for(cfg, kind, node) else {
            continue;
        };
        let backoff = veles_core::backoff::pool()
            .status(&backoff_key(kind, peer), now)
            .map(|(failures, wait)| json!({"failures": failures, "retry_in_secs": wait.as_secs()}))
            .unwrap_or(Value::Null);
        let mut row = json!({
            "peer": peer,
            "auto": auto_for(cfg, kind),
            "dialable": cfg.a2a_agents.contains_key(peer),
            "backoff": backoff,
            "last": last(kind, peer),
        });
        match kind {
            "store" => {
                let cursor = veles_core::storesync::peer_cursor(store, peer);
                row["cursor"] = json!(cursor);
                row["acked"] = json!(meta_i64(
                    store,
                    &format!("{}{peer}", veles_core::storesync::PEER_ACKED_PREFIX)
                ));
                row["local"] = json!(local.store_seq);
                row["behind"] = json!(local.store_seq.map(|h| (h - cursor).max(0)));
            }
            "memory" => {
                let cursor = meta_i64(store, &veles_core::memsync::cursor_key(peer));
                row["cursor"] = json!(cursor);
                row["local"] = json!(local.memory_rev);
                row["behind"] = json!(local.memory_rev.map(|h| (h - cursor).max(0)));
            }
            "config" => {
                let cursor = meta_i64(store, &veles_core::configsync::cursor_key(peer));
                // Behind is what the next push would carry: this node's
                // own offered rows past the cursor. The log also holds
                // every row received from the other peers, at higher
                // rowids; those are their origin's to push, and counting
                // them kept a peer "10 behind" forever (SMOKE-7).
                let pending = local
                    .config_node
                    .as_deref()
                    .and_then(|me| veles_core::configsync::pending_offer(store, me, cursor).ok());
                row["cursor"] = json!(cursor);
                row["local"] = json!(pending.map(|(_, head)| head.unwrap_or(cursor).max(cursor)));
                row["behind"] = json!(pending.map(|(count, _)| count));
            }
            _ => {
                let has = store.meta_get(&veles_core::vault::peer_has_key(peer));
                row["has_vault"] = json!(local.vault_digest.is_some());
                row["current"] = json!(
                    local.vault_digest.is_some() && has.as_deref() == local.vault_digest.as_deref()
                );
            }
        }
        out.insert(kind.to_string(), row);
    }
    Value::Object(out)
}

/// Heartbeat age in seconds; None when the timestamp does not parse.
fn heartbeat_age(heartbeat_at: &str) -> Option<i64> {
    let ts = chrono::NaiveDateTime::parse_from_str(heartbeat_at, "%Y-%m-%d %H:%M:%S").ok()?;
    Some((chrono::Utc::now().naive_utc() - ts).num_seconds().max(0))
}

/// Per-node health: `stale` (no heartbeat within `stale_after_secs`),
/// else `failing` (a sync kind is in backoff, or the last probe could
/// not reach it), else `fresh`. The self row is never `failing`.
///
/// A failed probe stands only until the next good exchange with the
/// node: a gossip exchange (`gossip`, the last one with this node's
/// gossip peer) or a sync pass of any kind that succeeded after it. It
/// used to stand until someone probed again, so one failed Probe kept a
/// node red, and its "failing" alert was never followed by a recovery
/// (FLEET-3).
pub fn node_health(
    node: &NodeRow,
    stale_after_secs: u64,
    sync: &Value,
    gossip: &Value,
    is_self: bool,
) -> Value {
    let stale = veles_core::node::is_stale(&node.heartbeat_at, stale_after_secs as i64);
    let failing: Vec<&str> = KINDS
        .iter()
        .copied()
        .filter(|k| {
            sync[*k]["backoff"]["failures"].as_u64().unwrap_or(0) > 0
                || sync[*k]["last"]["ok"] == json!(false)
        })
        .collect();
    let peer = cfg_peer_name(node);
    let probe = last("probe", &peer);
    let superseded = KINDS
        .iter()
        .map(|k| &sync[*k]["last"])
        .chain(std::iter::once(gossip))
        .any(|pass| pass["ok"] == json!(true) && later(&pass["at"], &probe["at"]));
    let unreachable = probe["ok"] == json!(false) && !superseded;
    let state = if is_self {
        "self"
    } else if stale {
        "stale"
    } else if !failing.is_empty() || unreachable {
        "failing"
    } else {
        "fresh"
    };
    json!({
        "state": state,
        "heartbeat_age_secs": heartbeat_age(&node.heartbeat_at),
        "failing": failing,
        "probe": probe,
        "unreachable": unreachable,
    })
}

/// Is the RFC 3339 time `a` later than `b`? A missing or unreadable
/// time is never later.
fn later(a: &Value, b: &Value) -> bool {
    let t = |v: &Value| {
        v.as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    };
    matches!((t(a), t(b)), (Some(a), Some(b)) if a > b)
}

/// The probe record's key: the node id (a probe resolves its own peer).
fn cfg_peer_name(node: &NodeRow) -> String {
    node.id.clone()
}

/// The last gossip exchange with the configured gossip peer that names
/// `node`, or null.
fn gossip_last(cfg: &VelesConfig, node: &NodeRow) -> Value {
    cfg.cluster
        .gossip_peers
        .iter()
        .find(|p| p.as_str() == node.name || p.as_str() == node.id)
        .map(|p| last("gossip", p))
        .unwrap_or(Value::Null)
}

/// [`node_health`] without this node's high-water marks: what the alert
/// watcher (`fleet_watch`) reads every gossip tick. Health needs only
/// the backoff and the last pass per kind, so it skips [`Local::read`]
/// (the memory store, the vault digest) that the inventory pays for.
pub fn node_health_light(cfg: &VelesConfig, node: &NodeRow, is_self: bool, now: Instant) -> Value {
    let mut sync = serde_json::Map::new();
    for kind in KINDS {
        let Some(peer) = peer_for(cfg, kind, node) else {
            continue;
        };
        let backoff = veles_core::backoff::pool()
            .status(&backoff_key(kind, peer), now)
            .map(|(failures, wait)| json!({"failures": failures, "retry_in_secs": wait.as_secs()}))
            .unwrap_or(Value::Null);
        sync.insert(
            kind.to_string(),
            json!({"peer": peer, "backoff": backoff, "last": last(kind, peer)}),
        );
    }
    let sync = Value::Object(sync);
    let mut health = node_health(
        node,
        cfg.cluster.stale_after_secs,
        &sync,
        &gossip_last(cfg, node),
        is_self,
    );
    health["sync"] = sync;
    health
}

/// The last explicit or automatic gossip exchange with each configured
/// gossip peer, for `/api/fleet`'s `sync.gossip`.
pub fn gossip_status(cfg: &VelesConfig) -> Value {
    Value::Array(
        cfg.cluster
            .gossip_peers
            .iter()
            .map(|peer| {
                json!({
                    "peer": peer,
                    "dialable": cfg.a2a_agents.contains_key(peer),
                    "last": last("gossip", peer),
                })
            })
            .collect(),
    )
}

fn pending(dir: Result<Vec<std::path::PathBuf>, anyhow::Error>) -> usize {
    dir.map(|v| v.len()).unwrap_or(0)
}

/// What sits in this node's sync inboxes, per kind.
pub fn inbox_counts() -> Value {
    json!({
        "store": pending(veles_core::storesync::inbox_pending(&veles_core::storesync::inbox_dir())),
        "memory": pending(veles_core::memsync::inbox_pending(&veles_core::memsync::inbox_dir())),
        "config": pending(veles_core::configsync::inbox_pending(&veles_core::configsync::inbox_dir())),
        "vault": pending(veles_core::vault::inbox_pending(&veles_core::vault::inbox_dir())),
    })
}

/// Add the command-center fields to a `/api/fleet` body built by
/// `fleet_inventory_json`: per node `sync_status`, `health` and
/// `sync_kinds`; `sync.inbox` and `sync.config_auto_effective`.
pub fn annotate(body: &mut Value, nodes: &[NodeRow], store: &Store, cfg: &VelesConfig) {
    let local = Local::read(store, cfg);
    let now = Instant::now();
    let stale_after = cfg.cluster.stale_after_secs;
    if let Some(rows) = body["nodes"].as_array_mut() {
        for row in rows.iter_mut() {
            let Some(node) = nodes
                .iter()
                .find(|n| Some(n.id.as_str()) == row["id"].as_str())
            else {
                continue;
            };
            let sync = node_sync_status(store, cfg, &local, node, now);
            let is_self = row["self"] == json!(true);
            row["health"] = node_health(node, stale_after, &sync, &gossip_last(cfg, node), is_self);
            row["sync_kinds"] = json!(sync
                .as_object()
                .map(|m| m.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default());
            row["sync_status"] = sync;
        }
    }
    body["sync"]["inbox"] = inbox_counts();
    body["sync"]["gossip"] = gossip_status(cfg);
    // The nodes forgotten fleet-wide that this gateway still refuses,
    // the registry's version spread, and the recent health alerts.
    body["forgotten"] = json!(store
        .node_tombstones()
        .unwrap_or_default()
        .iter()
        .map(|t| json!({"id": t.id, "forgotten_at": t.forgotten_at}))
        .collect::<Vec<_>>());
    body["versions"] = crate::fleet_obs::version_spread(nodes, stale_after);
    // Deployment consistency (version, revision, modules, plugins,
    // shadowing user plugins) as the last Drift check found it: it
    // needs a dial to every node, which this 15 s poll does not make.
    body["consistency"] = crate::fleet_obs::consistency_last();
    body["alerts"] = json!({
        "enabled": cfg.notify.fleet_alerts,
        "debounce_secs": cfg.notify.fleet_alert_debounce_secs,
        "recent": crate::fleet_watch::recent(),
    });
}

/// `/fleet list`: the `/api/fleet` body (`desktop_api::fleet_body`) as
/// chat text. Any caller with the status capability gets each node's
/// health state. `full` (the owner) adds what the Instances tab shows:
/// why a node is stale or failing, its status per sync kind, what waits
/// in this node's inboxes, a version drift, the nodes forgotten
/// fleet-wide, and the recent health alerts with where they go; and it
/// says so when the answering process is not the gateway, whose view
/// of passes, backoff and alerts is only its own.
/// `/fleet list` used to print `fresh` or `stale` and a `[sync peer]`
/// tag for store sync only (FLEET-9).
pub fn list_text(body: &Value, full: bool) -> String {
    let rows = body["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if rows.is_empty() {
        return "no registered nodes; enable the cluster registry to populate /fleet".into();
    }
    let secrets = veles_core::secrets::scrubbable();
    let clean =
        |s: &str| veles_core::a2a::redact_outbound(&veles_core::secrets::scrub(s, &secrets));
    let text = |v: &Value| v.as_str().unwrap_or("?").to_string();
    let mut out = "registered fleet nodes:\n".to_string();
    for row in rows {
        // The self row's health state is `self`; say fresh or stale and
        // mark it, as every other row reads.
        let state = match row["health"]["state"].as_str() {
            Some(state) if state != "self" => state,
            _ if row["stale"] == json!(true) => "stale",
            _ => "fresh",
        };
        out.push_str(&format!(
            "- {} ({}) {} — {state}",
            text(&row["id"]),
            text(&row["name"]),
            text(&row["host"])
        ));
        if row["self"] == json!(true) {
            out.push_str(" (this node)");
        }
        if full {
            let mut health = row["health"].clone();
            health["sync"] = row["sync_status"].clone();
            let why = crate::fleet_watch::detail(&health);
            if !why.is_empty() {
                out.push_str(&format!(": {why}"));
            }
        }
        out.push('\n');
        if full {
            for kind in KINDS {
                if row["sync_status"][kind].is_object() {
                    out.push_str(&format!(
                        "    {}\n",
                        kind_line(kind, &row["sync_status"][kind], &clean)
                    ));
                }
            }
        }
    }
    if !full {
        return out;
    }
    if body["gateway"] == json!(false) {
        out.push_str(
            "this process runs no gateway: the last passes, backoff and alerts here are only its own\n",
        );
    }
    let inbox = &body["sync"]["inbox"];
    if inbox.is_object() {
        out.push_str(&format!(
            "received, waiting to apply: store {} · memory {} · config {} · vault {}\n",
            inbox["store"], inbox["memory"], inbox["config"], inbox["vault"]
        ));
    }
    if body["versions"]["drift"] == json!(true) {
        let groups: Vec<String> = body["versions"]["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| {
                let names: Vec<String> = g["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|n| {
                        if n["stale"] == json!(true) {
                            format!("{} (stale)", text(&n["name"]))
                        } else {
                            text(&n["name"])
                        }
                    })
                    .collect();
                format!("{} ({})", text(&g["version"]), names.join(", "))
            })
            .collect();
        out.push_str(&format!(
            "version drift in the registry: {}\n",
            groups.join("; ")
        ));
    }
    if body["consistency"].is_object() {
        out.push_str(&crate::fleet_obs::consistency_text(&body["consistency"]));
    }
    let forgotten: Vec<String> = body["forgotten"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| format!("{} (at {} UTC)", text(&t["id"]), text(&t["forgotten_at"])))
        .collect();
    if !forgotten.is_empty() {
        out.push_str(&format!("forgotten fleet-wide: {}\n", forgotten.join(", ")));
    }
    let alerts = &body["alerts"];
    let delivery = if alerts["enabled"] != json!(true) {
        "listed here only; [notify] fleet_alerts is off".to_string()
    } else {
        match alerts["delivery"]["sends"].as_str() {
            Some("here") => "sent to the owner's home chat from this gateway".to_string(),
            Some("elsewhere") => "sent to the owner's home chat by the gateway that runs its \
                                  Telegram adapter, not this one"
                .to_string(),
            _ => format!("not sent: {}", text(&alerts["delivery"]["reason"])),
        }
    };
    let recent: Vec<String> = alerts["recent"]
        .as_array()
        .into_iter()
        .flatten()
        .take(5)
        .map(|a| {
            format!(
                "    {}: {} → {} {} ({})\n",
                text(&a["at"]),
                text(&a["from"]),
                text(&a["to"]),
                text(&a["name"]),
                if a["sent"] == json!(true) {
                    "sent".to_string()
                } else {
                    text(&a["note"])
                }
            )
        })
        .collect();
    if recent.is_empty() {
        out.push_str(&format!(
            "health alerts ({delivery}): none since this process started\n"
        ));
    } else {
        out.push_str(&format!("health alerts ({delivery}):\n{}", recent.concat()));
    }
    out
}

/// One sync kind of one node, as the tab's detail pane words it.
fn kind_line(kind: &str, s: &Value, clean: &dyn Fn(&str) -> String) -> String {
    let count = |v: &Value| v.as_i64().map_or("?".to_string(), |n| n.to_string());
    let mut parts = Vec::new();
    if s["dialable"] == json!(false) {
        parts.push("no [a2a_agents] entry, this peer cannot be dialed".to_string());
    }
    if kind == "vault" {
        parts.push(
            if s["has_vault"] == json!(false) {
                "this node has no vault"
            } else if s["current"] == json!(true) {
                "the peer holds this vault"
            } else {
                "the peer has not been sent this vault yet"
            }
            .to_string(),
        );
    } else {
        parts.push(format!(
            "cursor {} of {} ({} behind)",
            count(&s["cursor"]),
            count(&s["local"]),
            count(&s["behind"])
        ));
    }
    if let Some(ok) = s["last"]["ok"].as_bool() {
        parts.push(format!(
            "last pass {} at {}: {}",
            if ok { "ok" } else { "failed" },
            s["last"]["at"].as_str().unwrap_or("?"),
            clean(s["last"]["line"].as_str().unwrap_or(""))
        ));
    }
    if let Some(failures) = s["backoff"]["failures"].as_u64() {
        parts.push(format!(
            "backing off: {failures} failure(s), next automatic try in {} s",
            count(&s["backoff"]["retry_in_secs"])
        ));
    }
    format!(
        "{kind} sync with {} ({}): {}",
        s["peer"].as_str().unwrap_or("?"),
        if s["auto"] == json!(true) {
            "automatic"
        } else {
            "manual only"
        },
        parts.join("; ")
    )
}

/// `/fleet probe <node>`: a [`probe`] answer as one line.
pub fn probe_text(v: &Value) -> String {
    let node = v["node"].as_str().unwrap_or("?");
    let peer = v["peer"].as_str().unwrap_or("?");
    let ms = v["latency_ms"].as_u64().unwrap_or(0);
    if v["reachable"] == json!(true) {
        return format!("node {node} answered in {ms} ms ([a2a_agents] {peer})");
    }
    let secrets = veles_core::secrets::scrubbable();
    format!(
        "node {node} did not answer ([a2a_agents] {peer}, {ms} ms): {}",
        veles_core::a2a::redact_outbound(&veles_core::secrets::scrub(
            v["error"].as_str().unwrap_or(""),
            &secrets
        ))
    )
}

// ── operations ───────────────────────────────────────────────────────

pub(crate) fn find_node(
    store: &Arc<Mutex<Store>>,
    requested: &str,
) -> Result<NodeRow, OperationError> {
    let requested = requested.trim();
    store
        .lock_ok()
        .nodes_list()
        .map_err(OperationError::Failed)?
        .into_iter()
        .find(|n| !requested.is_empty() && (n.id == requested || n.name == requested))
        .ok_or(OperationError::UnknownNode)
}

/// Run `f` on a blocking thread with the caller's directory override.
async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> Result<R, OperationError> {
    let dirs = veles_core::config::dirs();
    veles_core::rt::spawn_blocking(move || {
        let _dirs = veles_core::config::set_dirs(dirs);
        f()
    })
    .await
    .map_err(|e| OperationError::Failed(anyhow::anyhow!("sync task failed: {e}")))
}

/// One explicit sync operation: `kind` ∈ [`KINDS`], `operation` ∈
/// push|apply. `requested` names the registry node for a push and is
/// ignored by an apply (which drains THIS node's inbox).
pub async fn operate(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    kind: &str,
    requested: &str,
    operation: &str,
    confirmed: bool,
) -> Result<Value, OperationError> {
    if !valid_kind(kind) {
        return Err(OperationError::InvalidOperation(format!(
            "kind must be one of {}, not {kind}",
            KINDS.join(", ")
        )));
    }
    if operation != "push" && operation != "apply" {
        return Err(OperationError::InvalidOperation(format!(
            "operation must be push or apply, not {operation}"
        )));
    }
    if kind == "store" {
        let result = veles_core::storesync::operate(store, cfg, requested, operation).await;
        if operation == "push" {
            if let Ok(node) = find_node(store, requested) {
                if let Some(peer) = peer_for(cfg, "store", &node) {
                    match &result {
                        Ok(v) => record("store", peer, Ok(v["result"].as_str().unwrap_or("ok"))),
                        Err(OperationError::Failed(e)) => {
                            record("store", peer, Err(&format!("{e:#}")))
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        return result.map(|mut v| {
            v["kind"] = json!("store");
            if operation == "apply" {
                let refused = v["refusals"].as_array().map_or(0, Vec::len);
                v["result"] = json!(format!(
                    "applied {} offer(s), {refused} refused",
                    v["applied"].as_u64().unwrap_or(0)
                ));
            }
            v
        });
    }
    if needs_confirmation(kind, operation) && !confirmed {
        return Err(OperationError::NeedsConfirmation(format!(
            "{kind} {operation} changes {} — confirm it first",
            if operation == "apply" {
                "this node's data"
            } else {
                "the peer's data"
            }
        )));
    }
    if operation == "apply" {
        let line = match kind {
            "memory" => crate::a2a_server::apply_memsync_inbox().await,
            "config" => {
                let s = store.clone();
                blocking(move || {
                    crate::a2a_server::apply_configsync_inbox(&veles_core::store::lock_timed(
                        &s,
                        "fleet::configsync_apply",
                    ))
                })
                .await?
            }
            _ => blocking(crate::a2a_server::apply_vault_inbox).await?,
        };
        return Ok(json!({"operation": "apply", "kind": kind, "result": line}));
    }
    let node = find_node(store, requested)?;
    let peer = peer_for(cfg, kind, &node)
        .ok_or_else(|| {
            OperationError::NotConfigured(format!(
                "node is not a configured {kind} sync peer; add it to [cluster] {} first",
                if kind == "store" {
                    "storesync_peers"
                } else {
                    "memsync_peers"
                }
            ))
        })?
        .to_string();
    let target = cfg.a2a_agents.get(&peer).ok_or_else(|| {
        OperationError::NotConfigured("sync peer has no [a2a_agents] endpoint configured".into())
    })?;
    let (url, bearer) = (target.url.clone(), target.bearer());
    // Explicit passes of one kind to one peer do not overlap. (The store
    // guard is shared with the automatic loop; these kinds' loops run
    // inline on the sync tick and their receivers are idempotent.)
    let _flight = veles_core::storesync::PushInFlight::claim(&format!("{kind}:{peer}"))
        .ok_or_else(|| {
            OperationError::AlreadyRunning(format!(
                "a {kind} push to {peer} is already running; wait for it to finish"
            ))
        })?;
    let me = veles_core::node::resolve_self(cfg).map_err(OperationError::Failed)?;
    let result = match kind {
        "memory" => push_memory(store, cfg, &me, &peer, &url, bearer.as_deref()).await,
        "config" => push_config(store, &me, &peer, &url, bearer.as_deref()).await,
        _ => push_vault(store, &peer, &url, bearer.as_deref()).await,
    };
    let key = backoff_key(kind, &peer);
    let pool = veles_core::backoff::pool();
    match result {
        Ok(line) => {
            pool.succeeded(&key);
            record(kind, &peer, Ok(&line));
            Ok(json!({"operation": "push", "kind": kind, "node": node.id, "result": line}))
        }
        Err(OperationError::Failed(e)) => {
            let base = Duration::from_secs(cfg.cluster.gossip_interval_secs.max(1));
            pool.failed(&key, base, Instant::now());
            record(kind, &peer, Err(&format!("{e:#}")));
            Err(OperationError::Failed(e))
        }
        Err(other) => Err(other),
    }
}

async fn push_memory(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    me: &veles_core::node::SelfNode,
    peer: &str,
    url: &str,
    bearer: Option<&str>,
) -> Result<String, OperationError> {
    let mem = veles_core::memory::MemoryStore::new();
    let high = mem.latest_rev_id().map_err(OperationError::Failed)?;
    let key = veles_core::memsync::cursor_key(peer);
    let cursor = meta_i64(&store.lock_ok(), &key);
    if high <= cursor {
        return Ok(format!(
            "nothing to push: {peer} has revisions up to {cursor}"
        ));
    }
    let line = veles_core::memsync::push_to_peer_scoped(
        &mem,
        &me.identity.id,
        &me.name,
        url,
        bearer,
        cursor,
        &cfg.cluster.local_memory_globs,
    )
    .await
    .map_err(OperationError::Failed)?;
    store
        .lock_ok()
        .meta_set(&key, &high.to_string())
        .map_err(OperationError::Failed)?;
    Ok(format!(
        "pushed revisions {cursor}..{high}: {}",
        line.lines().next().unwrap_or_default()
    ))
}

async fn push_config(
    store: &Arc<Mutex<Store>>,
    me: &veles_core::node::SelfNode,
    peer: &str,
    url: &str,
    bearer: Option<&str>,
) -> Result<String, OperationError> {
    // Log this node's own unscanned edits first — what the automatic
    // pass does when config.toml changed — then offer past the cursor.
    let (s, id, name, peer_owned) = (
        store.clone(),
        me.identity.id.clone(),
        me.name.clone(),
        peer.to_string(),
    );
    let built = blocking(move || -> anyhow::Result<_> {
        let s = veles_core::store::lock_timed(&s, "fleet::configsync_push");
        veles_core::configsync::scan_file(&s, &id, &veles_core::configsync::config_path())?;
        let cursor: i64 = s
            .meta_get(&veles_core::configsync::cursor_key(&peer_owned))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let high = s.config_log_max_seq()?;
        let (offer, withheld) = veles_core::configsync::build_offer(&s, &id, &name, cursor)?;
        Ok((offer, withheld, high))
    })
    .await?
    .map_err(OperationError::Failed)?;
    let (offer, withheld, high) = built;
    if offer.rows.is_empty() {
        return Ok(format!("nothing to push to {peer}"));
    }
    let reply = veles_core::configsync::send_offer(url, bearer, &offer)
        .await
        .map_err(OperationError::Failed)?;
    store
        .lock_ok()
        .meta_set(&veles_core::configsync::cursor_key(peer), &high.to_string())
        .map_err(OperationError::Failed)?;
    let said = reply
        .split("\n\n")
        .nth(1)
        .unwrap_or(&reply)
        .trim()
        .to_string();
    Ok(format!(
        "pushed {} row(s){}: {said}",
        offer.rows.len(),
        if withheld.is_empty() {
            String::new()
        } else {
            format!(" ({} withheld)", withheld.len())
        }
    ))
}

async fn push_vault(
    store: &Arc<Mutex<Store>>,
    peer: &str,
    url: &str,
    bearer: Option<&str>,
) -> Result<String, OperationError> {
    let v = blocking(veles_core::vault::Vault::open_local_if_any)
        .await?
        .map_err(OperationError::Failed)?
        .filter(|v| v.is_initialized())
        .ok_or_else(|| OperationError::NotConfigured("this node has no vault to send".into()))?;
    let digest = veles_core::vault::rows_digest(&v.file);
    let key = veles_core::vault::peer_has_key(peer);
    if store.lock_ok().meta_get(&key).as_deref() == Some(digest.as_str()) {
        return Ok(format!("{peer} already holds this vault"));
    }
    let msg = veles_core::vault::Message::Offer {
        from_node: v.keys.node.clone(),
        from_name: v.keys.name.clone(),
        file: v.file.clone(),
    };
    let reply = veles_core::vault::push_to_peer(&msg, url, bearer)
        .await
        .map_err(OperationError::Failed)?;
    store
        .lock_ok()
        .meta_set(&key, &digest)
        .map_err(OperationError::Failed)?;
    Ok(reply
        .split("\n\n")
        .nth(1)
        .unwrap_or(&reply)
        .trim()
        .to_string())
}

/// Forget a STALE registry row, fleet-wide: the row goes and a tombstone
/// stays, which gossip carries to every peer (`gossip.rs`, "Forgetting a
/// node fleet-wide"), so a peer that still lists the node cannot merge
/// it back here and drops it itself. Refuses this node itself and a node
/// still heartbeating (it would re-register on its next tick, and
/// forgetting a live node is how one loses track of it): the guards
/// `veles nodes forget` shares (`store::forget_refusal`, FLEET-6).
pub fn forget(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
    me: Option<&str>,
    confirmed: bool,
) -> Result<Value, OperationError> {
    use veles_core::store::{forget_refusal, ForgetRefusal};
    let node = find_node(store, requested)?;
    match forget_refusal(&node, me, cfg.cluster.stale_after_secs as i64) {
        Some(ForgetRefusal::ThisNode) => {
            return Err(OperationError::Refused(
                "this is the gateway's own node; it re-registers itself".into(),
            ))
        }
        Some(ForgetRefusal::Live { heartbeat_at }) => {
            return Err(OperationError::Refused(format!(
                "{} is still heartbeating (last {heartbeat_at}); only a stale node can be forgotten",
                node.name
            )))
        }
        None => {}
    }
    if !confirmed {
        return Err(OperationError::NeedsConfirmation(format!(
            "forgetting {} removes it from the registry — confirm it first",
            node.name
        )));
    }
    let forgot = store
        .lock_ok()
        .node_forget_everywhere(&node.id)
        .map_err(OperationError::Failed)?;
    Ok(json!({
        "forgot": forgot,
        "node": node.id,
        "name": node.name,
        // Said, because it is the difference from 0.1.x: gossip carries it.
        "fleet_wide": true,
        "gossip_peers": cfg.cluster.gossip_peers.len(),
    }))
}

/// Reach the node's configured A2A card (`[a2a_agents]` entry named
/// like the node). Never the registry endpoint, never a bearer: the
/// card is public. Reachability is the answer, so an unreachable node
/// is `Ok` with `reachable: false`.
pub async fn probe(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
) -> Result<Value, OperationError> {
    let node = find_node(store, requested)?;
    let (peer, target) = cfg
        .a2a_agents
        .iter()
        .find(|(name, _)| name.as_str() == node.name || name.as_str() == node.id)
        .ok_or_else(|| {
            OperationError::NotConfigured(
                "no [a2a_agents] entry names this node; its registry endpoint is not dialed".into(),
            )
        })?;
    let started = Instant::now();
    let client = veles_core::a2a::A2AClient::with_timeout(Duration::from_secs(5));
    let result = client.discover(&target.url).await;
    let ms = started.elapsed().as_millis() as u64;
    let key = cfg_peer_name(&node);
    Ok(match result {
        Ok(card) => {
            record("probe", &key, Ok(&format!("reachable in {ms} ms")));
            json!({"node": node.id, "peer": peer, "reachable": true, "latency_ms": ms,
                   "card_name": card["name"]})
        }
        Err(e) => {
            let why = format!("{e:#}");
            record("probe", &key, Err(&why));
            json!({"node": node.id, "peer": peer, "reachable": false, "latency_ms": ms,
                   "error": why})
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(store: &Store, id: &str, name: &str) {
        store
            .node_register(
                id,
                name,
                "host",
                "0.2.0",
                "always-on",
                None,
                Some("http://127.0.0.1:9"),
                None,
                None,
            )
            .unwrap();
    }

    fn age(store: &Store, id: &str, secs: i64) {
        let at = (chrono::Utc::now() - chrono::Duration::seconds(secs))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = ?1 WHERE id = ?2",
                [at.as_str(), id],
            )
            .unwrap();
    }

    #[test]
    fn status_covers_every_kind_a_node_is_a_peer_for() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "id-vps", "vps");
        registered(&store, "id-phone", "phone");
        let mut cfg = VelesConfig::default();
        cfg.cluster.storesync_peers = vec!["vps".into()];
        cfg.cluster.memsync_peers = vec!["vps".into(), "phone".into()];
        cfg.cluster.memsync_auto = true;
        store
            .meta_set(&veles_core::memsync::cursor_key("vps"), "7")
            .unwrap();
        veles_core::backoff::pool().failed(
            "configsync:phone",
            Duration::from_secs(30),
            Instant::now(),
        );
        let nodes = store.nodes_list().unwrap();
        let local = Local::read(&store, &cfg);
        let vps = nodes.iter().find(|n| n.id == "id-vps").unwrap();
        let phone = nodes.iter().find(|n| n.id == "id-phone").unwrap();
        let s = node_sync_status(&store, &cfg, &local, vps, Instant::now());
        assert_eq!(
            s.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["config", "memory", "store", "vault"],
            "{s}"
        );
        assert_eq!(s["memory"]["cursor"], 7);
        assert_eq!(s["memory"]["auto"], true);
        assert_eq!(s["store"]["dialable"], false, "no [a2a_agents] entry");
        let p = node_sync_status(&store, &cfg, &local, phone, Instant::now());
        assert!(p.get("store").is_none(), "phone is no store peer: {p}");
        assert_eq!(p["config"]["backoff"]["failures"], 1, "{p}");
        let h = node_health(phone, 300, &p, &Value::Null, false);
        assert_eq!(h["state"], "failing", "{h}");
        assert_eq!(h["failing"], json!(["config"]));
        age(&store, "id-phone", 900);
        let phone = store
            .nodes_list()
            .unwrap()
            .into_iter()
            .find(|n| n.id == "id-phone")
            .unwrap();
        assert_eq!(
            node_health(&phone, 300, &p, &Value::Null, false)["state"],
            "stale"
        );
        veles_core::backoff::pool().succeeded("configsync:phone");
    }

    /// SMOKE-7: in a cluster of three, `config_log` holds this node's
    /// rows AND the rows received from both peers, but a push only ever
    /// offers this node's own rows. Counting "behind" against the
    /// highest rowid of every origin left the vps row at "cursor 5 of 15
    /// (10 behind)" forever while every Sync now said "nothing to push".
    /// Behind is what the next push would carry, nothing else.
    #[test]
    fn config_behind_counts_only_what_a_push_would_send() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "id-vps", "vps");
        let mut cfg = VelesConfig::default();
        cfg.cluster.memsync_peers = vec!["vps".into()];
        let me = veles_core::node::resolve_self(&cfg).unwrap().identity.id;
        let row =
            |node: &str, n: i64, key: &str, value: &str| veles_core::configsync::ConfigLogRow {
                node: node.into(),
                node_seq: n,
                at: format!("2026-11-01T00:00:{n:02}.000000000Z"),
                key: key.into(),
                value: value.into(),
            };
        for n in 1..=5 {
            store
                .config_log_append(&row(&me, n, "model.default", &format!("\"m{n}\"")))
                .unwrap();
        }
        // The last push carried those five; then both peers' rows arrived.
        store
            .meta_set(&veles_core::configsync::cursor_key("vps"), "5")
            .unwrap();
        for origin in ["id-phone", "id-vps"] {
            for n in 1..=5 {
                store
                    .config_log_append(&row(
                        origin,
                        n,
                        "model.default",
                        &format!("\"{origin}{n}\""),
                    ))
                    .unwrap();
            }
        }
        let status = |store: &Store| {
            let nodes = store.nodes_list().unwrap();
            let vps = nodes.iter().find(|n| n.id == "id-vps").unwrap();
            node_sync_status(store, &cfg, &Local::read(store, &cfg), vps, Instant::now())["config"]
                .clone()
        };
        let c = status(&store);
        assert_eq!(c["behind"], 0, "peers' rows are not ours to push: {c}");
        assert_eq!(
            (c["cursor"].clone(), c["local"].clone()),
            (json!(5), json!(5)),
            "{c}"
        );
        // A row a push withholds (credential-shaped) is not behind either.
        store
            .config_log_append(&row(
                &me,
                6,
                "web.search_api",
                "\"sk-ant-abcdefghijklmnopqrstuvwxyz0123456789\"",
            ))
            .unwrap();
        assert_eq!(status(&store)["behind"], 0, "{}", status(&store));
        // A new edit here is one row behind, at its own rowid.
        store
            .config_log_append(&row(&me, 7, "model.default", "\"m7\""))
            .unwrap();
        let c = status(&store);
        assert_eq!(
            (c["behind"].clone(), c["local"].clone()),
            (json!(1), json!(17)),
            "{c}"
        );
    }

    #[test]
    fn forget_refuses_self_and_live_nodes_and_needs_confirmation() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "id-me", "me");
        registered(&store, "id-live", "live");
        registered(&store, "id-gone", "gone");
        age(&store, "id-gone", 3600);
        let store = Arc::new(Mutex::new(store));
        let cfg = VelesConfig::default();
        assert!(matches!(
            forget(&store, &cfg, "id-me", Some("id-me"), true),
            Err(OperationError::Refused(_))
        ));
        assert!(matches!(
            forget(&store, &cfg, "live", Some("id-me"), true),
            Err(OperationError::Refused(_))
        ));
        assert!(matches!(
            forget(&store, &cfg, "nobody", Some("id-me"), true),
            Err(OperationError::UnknownNode)
        ));
        assert!(matches!(
            forget(&store, &cfg, "gone", Some("id-me"), false),
            Err(OperationError::NeedsConfirmation(_))
        ));
        assert_eq!(
            store.lock_ok().nodes_list().unwrap().len(),
            3,
            "nothing gone yet"
        );
        let done = forget(&store, &cfg, "gone", Some("id-me"), true).unwrap();
        assert_eq!(done["forgot"], true);
        let left: Vec<String> = store
            .lock_ok()
            .nodes_list()
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(left.len(), 2);
        assert!(!left.contains(&"id-gone".to_string()));
    }

    #[tokio::test]
    async fn changing_operations_need_confirmation_and_unknown_kinds_are_refused() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "id-vps", "vps");
        let store = Arc::new(Mutex::new(store));
        let mut cfg = VelesConfig::default();
        cfg.cluster.memsync_peers = vec!["vps".into()];
        for (kind, op) in [
            ("config", "apply"),
            ("vault", "apply"),
            ("memory", "apply"),
            ("config", "push"),
            ("vault", "push"),
        ] {
            let e = operate(&store, &cfg, kind, "vps", op, false)
                .await
                .unwrap_err();
            assert!(
                matches!(e, OperationError::NeedsConfirmation(_)),
                "{kind} {op}: {e}"
            );
        }
        let e = operate(&store, &cfg, "gossip", "vps", "push", true)
            .await
            .unwrap_err();
        assert!(matches!(e, OperationError::InvalidOperation(_)), "{e}");
        // A memory push is not destructive, so it gets as far as the dial
        // config: the registry endpoint is never used as a fallback.
        let e = operate(&store, &cfg, "memory", "vps", "push", false)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("[a2a_agents]"), "{e}");
        let e = operate(&store, &cfg, "memory", "nobody", "push", false)
            .await
            .unwrap_err();
        assert!(matches!(e, OperationError::UnknownNode), "{e}");
        let e = operate(&store, &cfg, "memory", "", "push", false)
            .await
            .unwrap_err();
        assert!(
            matches!(e, OperationError::UnknownNode),
            "an empty node name matches nothing"
        );
        // Applying is a local drain: an empty inbox reports, it does not fail.
        let v = operate(&store, &cfg, "config", "", "apply", true)
            .await
            .unwrap();
        assert_eq!(v["kind"], "config", "{v}");
    }

    #[tokio::test]
    async fn probe_dials_only_the_configured_card_and_reports_unreachable() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "id-vps", "vps");
        let store = Arc::new(Mutex::new(store));
        let mut cfg = VelesConfig::default();
        let e = probe(&store, &cfg, "vps").await.unwrap_err();
        assert!(matches!(e, OperationError::NotConfigured(_)), "{e}");
        // A port nothing listens on: unreachable is an answer, not an error.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        cfg.a2a_agents.insert(
            "vps".into(),
            veles_core::config::A2APeer {
                url,
                ..Default::default()
            },
        );
        let v = probe(&store, &cfg, "id-vps").await.unwrap();
        assert_eq!(v["reachable"], false, "{v}");
        let node = store.lock_ok().nodes_list().unwrap().remove(0);
        let h = node_health(&node, 300, &json!({}), &Value::Null, false);
        assert_eq!(h["state"], "failing", "a failed probe marks the node: {h}");
    }

    /// FLEET-3: one failed manual Probe used to keep the node "failing"
    /// (a red row, and an alert) until someone probed again: nothing
    /// else cleared the verdict. The next good exchange with the node — a
    /// gossip exchange or a sync pass — supersedes it; a probe that fails
    /// again after that is news again.
    #[test]
    fn a_failed_probe_does_not_pin_the_node_failing() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "id-probed", "probed");
        registered(&store, "id-probed-sync", "probed-sync");
        let mut cfg = VelesConfig::default();
        cfg.cluster.gossip_peers = vec!["probed".into()];
        cfg.cluster.memsync_peers = vec!["probed-sync".into()];
        let health = |id: &str| {
            let node = store
                .nodes_list()
                .unwrap()
                .into_iter()
                .find(|n| n.id == id)
                .unwrap();
            node_health_light(&cfg, &node, false, Instant::now())
        };
        let tick = || std::thread::sleep(Duration::from_millis(5));
        record("probe", "id-probed", Err("connection refused"));
        assert_eq!(health("id-probed")["state"], "failing");
        tick();
        record("gossip", "probed", Ok("probed: merged 3 row(s)"));
        let h = health("id-probed");
        assert_eq!(h["state"], "fresh", "a good gossip exchange after it: {h}");
        tick();
        record("probe", "id-probed", Err("connection refused"));
        assert_eq!(
            health("id-probed")["state"],
            "failing",
            "a probe failing after that is news again"
        );
        // A sync pass is an exchange as well.
        record("probe", "id-probed-sync", Err("connection refused"));
        assert_eq!(health("id-probed-sync")["state"], "failing");
        tick();
        record("config", "probed-sync", Ok("stored 1 row(s)"));
        let h = health("id-probed-sync");
        assert_eq!(h["state"], "fresh", "a good sync pass after it: {h}");
    }

    /// FLEET-9: `/fleet list` words the `/api/fleet` body as the tab's
    /// detail pane does. The self row is marked, a stale node says for
    /// how long and a failing one why, each sync kind has its line, and
    /// the alerts say where they go. Without `full` only the states are
    /// left.
    #[test]
    fn the_fleet_listing_words_every_part_of_the_inventory() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let body = json!({
            "nodes": [
                {"id": "id-me", "name": "me", "host": "h0", "self": true, "stale": false,
                 "health": {"state": "self"}, "sync_status": {}},
                {"id": "id-a", "name": "a", "host": "h1", "self": false, "stale": false,
                 "health": {"state": "failing", "failing": ["memory"], "unreachable": true,
                            "probe": {"ok": false, "line": "connection refused"}},
                 "sync_status": {
                     "memory": {"peer": "a", "auto": true, "dialable": true,
                                "cursor": 4, "local": 9, "behind": 5,
                                "last": {"ok": false, "at": "2026-11-10T01:00:00+00:00",
                                         "line": "peer answered 503"},
                                "backoff": {"failures": 2, "retry_in_secs": 40}},
                     "vault": {"peer": "a", "auto": false, "dialable": false,
                               "has_vault": true, "current": false,
                               "last": null, "backoff": null},
                 }},
                {"id": "id-b", "name": "b", "host": "h2", "self": false, "stale": true,
                 "health": {"state": "stale", "heartbeat_age_secs": 1260}, "sync_status": {}},
            ],
            "sync": {"inbox": {"store": 1, "memory": 0, "config": 2, "vault": 0}},
            "versions": {"drift": true, "groups": [
                {"version": "0.2.2", "nodes": [{"name": "me", "stale": false},
                                               {"name": "a", "stale": false}]},
                {"version": "0.2.0", "nodes": [{"name": "b", "stale": true}]},
            ]},
            "forgotten": [{"id": "id-gone", "forgotten_at": "2026-11-01 10:00:00"}],
            "alerts": {
                "enabled": true,
                "delivery": {"sends": "elsewhere",
                             "reason": "the telegram adapter runs on another node"},
                "recent": [{"at": "2026-11-10T01:00:05+00:00", "name": "a",
                            "from": "fresh", "to": "failing",
                            "sent": false, "note": "not sent from this node"}],
            },
        });
        let full = list_text(&body, true);
        for wanted in [
            "- id-me (me) h0 — fresh (this node)\n",
            "- id-a (a) h1 — failing: memory sync: peer answered 503; probe: connection refused\n",
            "    memory sync with a (automatic): cursor 4 of 9 (5 behind); \
             last pass failed at 2026-11-10T01:00:00+00:00: peer answered 503; \
             backing off: 2 failure(s), next automatic try in 40 s\n",
            "    vault sync with a (manual only): no [a2a_agents] entry, this peer \
             cannot be dialed; the peer has not been sent this vault yet\n",
            "- id-b (b) h2 — stale: ",
            "received, waiting to apply: store 1 · memory 0 · config 2 · vault 0\n",
            "version drift in the registry: 0.2.2 (me, a); 0.2.0 (b (stale))\n",
            "forgotten fleet-wide: id-gone (at 2026-11-01 10:00:00 UTC)\n",
            "health alerts (sent to the owner's home chat by the gateway that runs its \
             Telegram adapter, not this one):\n    \
             2026-11-10T01:00:05+00:00: fresh → failing a (not sent from this node)\n",
        ] {
            assert!(full.contains(wanted), "missing {wanted:?} in:\n{full}");
        }
        // Where the alerts go, in each case /api/fleet reports.
        let mut quiet = body.clone();
        quiet["alerts"]["recent"] = json!([]);
        for (delivery, enabled, wanted) in [
            (
                json!({"sends": "no", "reason": "[notify] ask_relay is off"}),
                true,
                "health alerts (not sent: [notify] ask_relay is off): none since this process started\n",
            ),
            (
                json!({"sends": "here", "reason": ""}),
                true,
                "health alerts (sent to the owner's home chat from this gateway): none since",
            ),
            (
                json!({"sends": "no", "reason": "[notify] fleet_alerts is off"}),
                false,
                "health alerts (listed here only; [notify] fleet_alerts is off): none since",
            ),
        ] {
            quiet["alerts"]["delivery"] = delivery;
            quiet["alerts"]["enabled"] = json!(enabled);
            let text = list_text(&quiet, true);
            assert!(text.contains(wanted), "missing {wanted:?} in:\n{text}");
        }
        // A listing from a process that is not the gateway says so: what
        // it shows of passes, backoff and alerts is only its own.
        assert!(!full.contains("runs no gateway"), "{full}");
        quiet["gateway"] = json!(false);
        assert!(
            list_text(&quiet, true).contains(
                "this process runs no gateway: the last passes, backoff and alerts here are only its own\n"
            ),
            "{quiet}"
        );
        assert!(!list_text(&quiet, false).contains("runs no gateway"));
        // No drift and nothing forgotten: no such lines.
        quiet["versions"]["drift"] = json!(false);
        quiet["forgotten"] = json!([]);
        let text = list_text(&quiet, true);
        assert!(
            !text.contains("version drift") && !text.contains("forgotten"),
            "{text}"
        );
        assert_eq!(
            list_text(&body, false),
            "registered fleet nodes:\n\
             - id-me (me) h0 — fresh (this node)\n\
             - id-a (a) h1 — failing\n\
             - id-b (b) h2 — stale\n"
        );
        assert_eq!(
            list_text(&json!({"nodes": []}), true),
            "no registered nodes; enable the cluster registry to populate /fleet"
        );
        assert_eq!(
            probe_text(&json!({"node": "id-a", "peer": "a", "reachable": true, "latency_ms": 12})),
            "node id-a answered in 12 ms ([a2a_agents] a)"
        );
        assert_eq!(
            probe_text(&json!({"node": "id-a", "peer": "a", "reachable": false,
                               "latency_ms": 3, "error": "connection refused"})),
            "node id-a did not answer ([a2a_agents] a, 3 ms): connection refused"
        );
    }
}
