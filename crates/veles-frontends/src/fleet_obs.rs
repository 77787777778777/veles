//! The command center's per-node observation (0.2.0, owner ask 287976.1,
//! FLEET-6 remainder): a node's own log tail and machine metrics, the
//! version, revision and config drift across the registry, and an
//! explicit gossip exchange.
//!
//! THE CALL. A registry row is identity only (fleet_ops.rs, "THE
//! RULES"): every remote ask dials the operator's `[a2a_agents]` entry
//! named like the node, with its bearer, never the registry's
//! self-reported endpoint. It is the JSON-RPC method [`METHOD`] on the
//! node's A2A server — not a `message/send` text, because a peer that
//! does not know a text marker runs the text as an LLM turn, while every
//! veles A2A server answers an unknown METHOD with "method not found"
//! (which is how a 0.1.x node is told apart from an unreachable one).
//!
//! WHO MAY ASK. The asking side is owner-only: the desktop API (owner
//! token) and `/fleet` from an owner. The answering node answers only an
//! authenticated peer named in its `[cluster] memsync_peers` — the list
//! of peers that talk cluster business with it (gossip, config, vault and
//! memory sync all use it), read live — and nothing during a lockdown.
//!
//! WHAT LEAVES THE NODE. Logs: at most the in-memory ring of the
//! gateway's own log (`logbuf`, [`MAX_LINES`] lines), every line through
//! the same two redactors the outbound bus uses (registered secret
//! values, then credential shapes). Metrics: figures about the machine
//! and counts of running work, no labels or prompts. Config drift: per
//! replicated key a hash of the value salted with the asker's nonce, the
//! key names, and the config log's per-author high-water marks — never a
//! value.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use veles_core::config::VelesConfig;
use veles_core::lockutil::LockOk;
use veles_core::store::{NodeRow, Store};
use veles_core::storesync::OperationError;

/// The node-to-node method: `{op: logs|metrics|fingerprint, …}`.
pub const METHOD: &str = "veles/fleet";
/// The most log lines one ask returns: the whole ring.
pub const MAX_LINES: usize = veles_core::logbuf::CAP;
/// What an ask without `lines` gets.
pub const DEFAULT_LINES: usize = 100;
/// How long a remote ask may take.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);

/// The source revision this binary was built from (build.rs).
pub fn revision() -> &'static str {
    option_env!("VELES_SOURCE_REVISION").unwrap_or("unreported-development-build")
}

fn started() -> &'static OnceLock<Instant> {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    &STARTED
}

/// Note when the gateway came up, for `gateway_uptime_secs`. The first
/// call wins; the gateway makes it at boot.
pub fn mark_started() {
    let _ = started().set(Instant::now());
}

// ── what this node answers ───────────────────────────────────────────

/// Answer one fleet request for THIS node: `op` is `logs`, `metrics` or
/// `fingerprint`. Runs blocking reads (the store, /proc, config.toml);
/// call it off the async runtime.
pub fn answer_local(store: &Arc<Mutex<Store>>, params: &Value) -> Result<Value, String> {
    let op = params["op"].as_str().unwrap_or("");
    let mut v = match op {
        "logs" => local_logs(
            params["lines"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_LINES),
            params["level"].as_str().unwrap_or("all"),
        ),
        "metrics" => local_metrics(store),
        "fingerprint" => local_fingerprint(store, params["nonce"].as_str().unwrap_or("")),
        other => return Err(format!("unknown fleet request `{other}`")),
    };
    v["op"] = json!(op);
    v["node"] = self_node_json();
    Ok(v)
}

fn self_node_json() -> Value {
    let id = veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
        .ok()
        .flatten();
    let name = veles_core::config::load()
        .ok()
        .and_then(|c| c.node.name)
        .or_else(|| id.as_ref().map(|i| i.name.clone()));
    json!({"id": id.map(|i| i.id), "name": name})
}

fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "TRACE" => 0,
        "DEBUG" => 1,
        "INFO" => 2,
        "WARN" | "WARNING" => 3,
        "ERROR" => 4,
        _ => 0,
    }
}

/// The last `lines` lines of this gateway's log ring at or above
/// `level` (`all`, `info`, `warn`, `error`), redacted.
pub fn local_logs(lines: usize, level: &str) -> Value {
    let lines = lines.clamp(1, MAX_LINES);
    let floor = if level == "all" { 0 } else { level_rank(level) };
    let secrets = veles_core::secrets::scrubbable();
    let clean =
        |s: &str| veles_core::a2a::redact_outbound(&veles_core::secrets::scrub(s, &secrets));
    let all = veles_core::logbuf::snapshot(MAX_LINES);
    let held = all.len();
    let mut kept: Vec<Value> = all
        .into_iter()
        .filter(|l| level_rank(&l.level) >= floor)
        .map(|l| {
            json!({
                "time": l.time,
                "level": l.level,
                "target": clean(&l.target),
                "msg": clean(&l.msg),
            })
        })
        .collect();
    let skip = kept.len().saturating_sub(lines);
    let kept = kept.split_off(skip);
    json!({
        "lines": kept,
        "held": held,
        "capacity": MAX_LINES,
        "level": level,
        "redacted": true,
    })
}

/// This node's machine and work, sampled now.
pub fn local_metrics(store: &Arc<Mutex<Store>>) -> Value {
    let mut v = veles_core::hostmetrics::sample(&veles_core::config::veles_home());
    v["version"] = json!(env!("CARGO_PKG_VERSION"));
    v["revision"] = json!(revision());
    v["profile"] = json!(veles_core::profiles::active_profile());
    v["os"] = json!(std::env::consts::OS);
    v["arch"] = json!(std::env::consts::ARCH);
    v["gateway_uptime_secs"] = json!(started().get().map(|t| t.elapsed().as_secs()));
    v["in_flight"] = in_flight(store);
    v["sampled_at"] = json!(chrono::Utc::now().to_rfc3339());
    v
}

/// Work running in THIS gateway process: turns (by surface, from the
/// turn checkpoints it owns) and background tasks (by kind). Counts
/// only — a label or a prompt is not the command center's business.
pub(crate) fn in_flight(store: &Arc<Mutex<Store>>) -> Value {
    let mut turns = serde_json::Map::new();
    {
        let s = veles_core::store::lock_timed(store, "fleet_obs::in_flight");
        if let Ok(mut stmt) = s.conn().prepare(
            "SELECT surface, COUNT(*) FROM turn_checkpoints
              WHERE state = 'running' AND owner_pid = ?1 GROUP BY surface",
        ) {
            if let Ok(rows) = stmt.query_map([std::process::id() as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            }) {
                for (surface, n) in rows.flatten() {
                    turns.insert(surface, json!(n));
                }
            }
        };
        // (The `;` ends the statement, so the statement's temporaries go
        // before the guard they borrow from.)
    }
    let mut tasks = BTreeMap::<&str, usize>::new();
    for t in veles_core::tasks::live() {
        *tasks.entry(t.kind.as_str()).or_default() += 1;
    }
    let total_turns: i64 = turns.values().filter_map(Value::as_i64).sum();
    json!({
        "turns": turns,
        "turns_total": total_turns,
        "tasks": tasks,
        "tasks_total": tasks.values().sum::<usize>(),
    })
}

/// A short salted hash: equal values hash equal under the same nonce,
/// and the value does not leave the node.
fn salted(nonce: &str, key: &str, value: &str) -> String {
    let h = veles_core::memory::MemoryStore::content_sha(&format!("{nonce}\0{key}\0{value}"));
    h.chars().take(16).collect()
}

/// A nonce the asker sent, or a fresh one: 16..=128 visible characters.
fn usable_nonce(nonce: &str) -> String {
    let ok = (16..=128).contains(&nonce.len()) && nonce.chars().all(|c| c.is_ascii_graphic());
    if ok {
        nonce.to_string()
    } else {
        uuid::Uuid::new_v4().simple().to_string()
    }
}

/// What drift is compared on: version, revision, and the replicated
/// config — the config log's per-author high-water marks, a hash of the
/// whole log, and per replicated key of config.toml a salted hash.
pub fn local_fingerprint(store: &Arc<Mutex<Store>>, nonce: &str) -> Value {
    let nonce = usable_nonce(nonce);
    let mut marks = serde_json::Map::new();
    let mut rows = 0i64;
    let mut log_hash = String::new();
    {
        let s = veles_core::store::lock_timed(store, "fleet_obs::fingerprint");
        if let Ok(mut stmt) = s.conn().prepare(
            "SELECT node, MAX(node_seq), COUNT(*) FROM config_log GROUP BY node ORDER BY node",
        ) {
            if let Ok(it) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            }) {
                for (author, max, n) in it.flatten() {
                    marks.insert(author, json!(max));
                    rows += n;
                }
            }
        }
        if let Ok(mut stmt) = s
            .conn()
            .prepare("SELECT node, node_seq, key, value FROM config_log ORDER BY node, node_seq")
        {
            if let Ok(it) = stmt.query_map([], |r| {
                Ok(format!(
                    "{}\t{}\t{}\t{}\n",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?
                ))
            }) {
                let text: String = it.flatten().collect();
                log_hash = salted(&nonce, "config_log", &text);
            }
        };
    }
    let mut keys = serde_json::Map::new();
    let mut keys_error = Value::Null;
    match std::fs::read_to_string(veles_core::configsync::config_path()) {
        Ok(raw) => match veles_core::configsync::scan_config_toml(&raw) {
            Ok(snapshot) => {
                for (k, v) in snapshot {
                    let h = salted(&nonce, &k, &v);
                    keys.insert(k, json!(h));
                }
            }
            Err(e) => keys_error = json!(format!("{e:#}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => keys_error = json!(format!("config.toml: {e}")),
    }
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "revision": revision(),
        "nonce": nonce,
        "config": {
            "log_marks": marks,
            "log_rows": rows,
            "log_hash": log_hash,
            "keys": keys,
            "keys_error": keys_error,
        },
        "inventory": local_inventory(),
    })
}

// ── deployment consistency ───────────────────────────────────────────

/// What this node RUNS, for deployment consistency (0.2.2): the
/// version of each loaded in-process module and process plugin, each
/// provider driver's source hash, and every user plugin that shadows a
/// bundled one, with the hash of each side. 0.2.0 was "deployed to all
/// nodes" with three builds on four nodes and a stale user copy of the
/// Anthropic driver shadowing the bundled one, and nothing showed it.
/// Versions and hashes only: no path, no config.
pub fn local_inventory() -> Value {
    let mut items = BTreeMap::<String, String>::new();
    if let Some(h) = veles_core::modulehost::global() {
        for s in h.status() {
            if s.state != "refused" {
                items.insert(format!("module:{}", s.plugin), s.version.clone());
            }
        }
    }
    if let Some(h) = veles_core::pluginhost::global() {
        for s in h.status() {
            if s.state != "refused" {
                items.insert(format!("plugin:{}", s.plugin), s.version.clone());
            }
        }
    }
    for d in veles_core::providers::drivers() {
        let version = d.provenance.version.clone().unwrap_or_default();
        let hash = d
            .provenance
            .hash
            .as_deref()
            .map(veles_core::plugins::short_digest)
            .unwrap_or("")
            .to_string();
        items.insert(
            format!("driver:{}", d.qualified_name()),
            format!("{version} {hash}").trim().to_string(),
        );
    }
    let mut shadows = Vec::new();
    for p in veles_core::plugins::discover_all().unwrap_or_default() {
        if let Some(b) = veles_core::plugins::shadowed_bundled(&p) {
            shadows.push(json!({
                "name": p.manifest.name,
                "user_sha256": veles_core::plugins::short_digest(&p.digest),
                "bundled_sha256": veles_core::plugins::short_digest(&b.digest),
                "same_content": p.digest == b.digest,
            }));
        }
    }
    json!({"items": items, "shadows": shadows})
}

/// One node's deployment, as [`consistency`] compares it.
pub struct Deployed<'a> {
    pub id: &'a str,
    pub name: &'a str,
    /// The node's fingerprint; `None` when it could not be asked.
    pub fingerprint: Option<&'a Value>,
    /// Why it could not be asked.
    pub error: Option<String>,
}

/// The most common value, and whether it is shared by more than one
/// node (a tie has no majority).
fn majority<'v>(values: &[&'v str]) -> Option<&'v str> {
    let mut counts = BTreeMap::<&str, usize>::new();
    for v in values {
        *counts.entry(v).or_default() += 1;
    }
    let best = counts.values().copied().max()?;
    let top: Vec<&str> = counts
        .iter()
        .filter(|(_, n)| **n == best)
        .map(|(v, _)| *v)
        .collect();
    (top.len() == 1).then(|| top[0])
}

/// Deployment consistency across nodes (pure): every node's version,
/// revision, module, plugin and driver versions, and user plugins that
/// shadow a bundled one with other content. A node that differs from
/// the others is flagged with what differs; without a majority, every
/// node that disagrees is.
///
/// An item (module, process plugin, driver) is compared among the nodes
/// that have it, and one that is not on every compared node is listed
/// in `missing` by name with the nodes that lack it: a Windows node has
/// no Racket or Lisp module and a phone has its own set, so a lacking
/// item is not drift, but it is never left unsaid either.
///
/// `verdict`: `not_consistent` when a node is flagged; `unknown` when a
/// node could not be compared; `consistent_with_differences` when every
/// node agrees on its build and on every item two or more of them share
/// and some item is missing somewhere; `consistent` (and `consistent`
/// true) only when nothing is flagged, unknown or missing.
pub fn consistency(nodes: &[Deployed]) -> Value {
    let mut unknown = Vec::new();
    let mut compared: Vec<(&Deployed, &Value)> = Vec::new();
    for n in nodes {
        match n.fingerprint {
            Some(f) if f.get("inventory").is_some_and(|i| i.is_object()) => compared.push((n, f)),
            Some(f) => unknown.push(json!({"id": n.id, "name": n.name,
                "why": format!("runs veles {}, which does not report its modules and plugins",
                               f["version"].as_str().unwrap_or("?"))})),
            None => unknown.push(json!({"id": n.id, "name": n.name,
                "why": n.error.clone().unwrap_or_else(|| "not asked".into())})),
        }
    }
    let mut why: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    // Version and revision: every node reports them.
    for field in ["version", "revision"] {
        let values: Vec<&str> = compared
            .iter()
            .map(|(_, f)| f[field].as_str().unwrap_or("?"))
            .collect();
        let maj = majority(&values);
        for ((n, _), v) in compared.iter().zip(&values) {
            if maj != Some(*v) && values.iter().any(|o| o != v) {
                why.entry(n.id).or_default().push(format!("{field} {v}"));
            }
        }
    }
    // Each module, plugin and driver, among the nodes that load it.
    let mut keys = BTreeSet::new();
    for (_, f) in &compared {
        if let Some(m) = f["inventory"]["items"].as_object() {
            keys.extend(m.keys().cloned());
        }
    }
    let mut missing = Vec::new();
    for key in &keys {
        let have: Vec<(&Deployed, &str)> = compared
            .iter()
            .filter_map(|(n, f)| f["inventory"]["items"][key].as_str().map(|v| (*n, v)))
            .collect();
        if have.len() < compared.len() {
            let lacking: Vec<Value> = compared
                .iter()
                .filter(|(n, _)| !have.iter().any(|(h, _)| h.id == n.id))
                .map(|(n, _)| json!({"id": n.id, "name": n.name}))
                .collect();
            let on: Vec<&str> = have.iter().map(|(n, _)| n.name).collect();
            missing.push(json!({"item": key, "on": on, "lacking": lacking}));
        }
        let values: Vec<&str> = have.iter().map(|(_, v)| *v).collect();
        let maj = majority(&values);
        for (n, v) in &have {
            if maj != Some(*v) && values.iter().any(|o| o != v) {
                why.entry(n.id).or_default().push(format!("{key} {v}"));
            }
        }
    }
    // A user copy with other content than the bundled one it hides.
    let mut shadows = Vec::new();
    for (n, f) in &compared {
        for s in f["inventory"]["shadows"].as_array().into_iter().flatten() {
            let same = s["same_content"] == json!(true);
            shadows.push(json!({"id": n.id, "name": n.name, "plugin": s["name"],
                "user_sha256": s["user_sha256"], "bundled_sha256": s["bundled_sha256"],
                "same_content": same}));
            if !same {
                why.entry(n.id).or_default().push(format!(
                    "user plugin {} shadows the bundled one (user {}, bundled {})",
                    s["name"].as_str().unwrap_or("?"),
                    s["user_sha256"].as_str().unwrap_or("?"),
                    s["bundled_sha256"].as_str().unwrap_or("?")
                ));
            }
        }
    }
    let flagged: Vec<Value> = compared
        .iter()
        .filter_map(|(n, _)| {
            why.get(n.id)
                .map(|w| json!({"id": n.id, "name": n.name, "differs": w}))
        })
        .collect();
    let (verdict, statement) = if !flagged.is_empty() {
        (
            "not_consistent",
            format!(
                "The fleet is NOT consistent: {} node(s) differ.",
                flagged.len()
            ),
        )
    } else if !unknown.is_empty() || compared.is_empty() {
        (
            "unknown",
            format!(
                "Consistency unknown: {} node(s) could not be compared.",
                unknown.len()
            ),
        )
    } else if !missing.is_empty() {
        (
            "consistent_with_differences",
            format!(
                "The fleet is consistent with differences: {} node(s) run the same build and \
                 agree on everything they share; {} item(s) are not on every node.",
                compared.len(),
                missing.len()
            ),
        )
    } else {
        (
            "consistent",
            format!(
                "The fleet is consistent: {} node(s) run the same build, modules and plugins.",
                compared.len()
            ),
        )
    };
    json!({
        "consistent": verdict == "consistent",
        "verdict": verdict,
        "compared": compared.len(),
        "flagged": flagged,
        "unknown": unknown,
        "missing": missing,
        "shadows": shadows,
        "statement": statement,
    })
}

fn last_consistency() -> &'static Mutex<Value> {
    static LAST: OnceLock<Mutex<Value>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(Value::Null))
}

/// A consistency verdict as text: the statement, then each flagged
/// node with what differs, the nodes that could not be compared, each
/// item not on every node with the nodes that lack it, and the
/// shadowing user plugins.
pub fn consistency_text(c: &Value) -> String {
    let mut out = String::new();
    if let Some(s) = c["statement"].as_str() {
        out.push_str(s);
        out.push('\n');
    }
    for f in c["flagged"].as_array().into_iter().flatten() {
        let what: Vec<&str> = f["differs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        out.push_str(&format!(
            "  {} differs: {}\n",
            f["name"].as_str().unwrap_or("?"),
            what.join("; ")
        ));
    }
    for u in c["unknown"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} not compared: {}\n",
            u["name"].as_str().unwrap_or("?"),
            u["why"].as_str().unwrap_or("")
        ));
    }
    for m in c["missing"].as_array().into_iter().flatten() {
        let lacking: Vec<&str> = m["lacking"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n["name"].as_str())
            .collect();
        out.push_str(&format!(
            "  {} is not on: {}\n",
            m["item"].as_str().unwrap_or("?"),
            lacking.join(", ")
        ));
    }
    for s in c["shadows"].as_array().into_iter().flatten() {
        if s["same_content"] == json!(true) {
            out.push_str(&format!(
                "  {}: user plugin {} shadows the bundled one with the same content ({})\n",
                s["name"].as_str().unwrap_or("?"),
                s["plugin"].as_str().unwrap_or("?"),
                s["user_sha256"].as_str().unwrap_or("?")
            ));
        }
    }
    out
}

/// The last consistency check this process made (`/api/fleet`
/// `consistency`), or a note that none was made yet.
pub fn consistency_last() -> Value {
    let v = last_consistency().lock_ok().clone();
    if v.is_null() {
        json!({"checked": false, "consistent": false,
               "statement": "Consistency not checked yet: run Drift… (/fleet drift)."})
    } else {
        v
    }
}

// ── asking a node ────────────────────────────────────────────────────

pub(crate) fn me() -> Option<String> {
    veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
}

/// The `[a2a_agents]` entry named like the node (its name or id), as
/// the probe resolves it: (peer name, url, bearer).
pub(crate) fn dial_target(
    cfg: &VelesConfig,
    node: &NodeRow,
) -> Option<(String, String, Option<String>)> {
    cfg.a2a_agents
        .iter()
        .find(|(name, _)| name.as_str() == node.name || name.as_str() == node.id)
        .map(|(name, peer)| (name.clone(), peer.url.clone(), peer.bearer()))
}

/// Run `f` on a blocking thread with the caller's directory override.
pub(crate) async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> Result<R, OperationError> {
    let dirs = veles_core::config::dirs();
    veles_core::rt::spawn_blocking(move || {
        let _dirs = veles_core::config::set_dirs(dirs);
        f()
    })
    .await
    .map_err(|e| OperationError::Failed(anyhow::anyhow!("fleet task failed: {e}")))
}

/// Ask registry node `requested` (id or name) for `params` (`op` and
/// its arguments) — answered here for this node itself, else over the
/// node's configured A2A link.
pub async fn ask(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
    params: Value,
) -> Result<Value, OperationError> {
    let op = params["op"].as_str().unwrap_or("");
    if !matches!(op, "logs" | "metrics" | "fingerprint") {
        return Err(OperationError::InvalidOperation(format!(
            "a fleet request is logs, metrics or fingerprint, not {op:?}"
        )));
    }
    let node = crate::fleet_ops::find_node(store, requested)?;
    if me().as_deref() == Some(node.id.as_str()) {
        let s = store.clone();
        let mut v = blocking(move || answer_local(&s, &params))
            .await?
            .map_err(OperationError::InvalidOperation)?;
        v["via"] = json!("local");
        return Ok(v);
    }
    let (peer, url, bearer) = dial_target(cfg, &node).ok_or_else(|| {
        OperationError::NotConfigured(format!(
            "no [a2a_agents] entry names {}; its registry endpoint is not dialed",
            node.name
        ))
    })?;
    let client = veles_core::a2a::A2AClient::with_timeout(ASK_TIMEOUT);
    match client.rpc(&url, bearer.as_deref(), METHOD, params).await {
        Ok(mut v) => {
            v["via"] = json!(peer);
            Ok(v)
        }
        Err(e) => Err(rpc_error(&node, &format!("{e:#}"))),
    }
}

/// A failed remote ask, said in the owner's terms.
fn rpc_error(node: &NodeRow, text: &str) -> OperationError {
    if text.contains("-32601") {
        OperationError::Refused(format!(
            "{} does not answer fleet requests: it runs veles {}, and 0.2.0 or later does",
            node.name,
            if node.version.is_empty() {
                "?"
            } else {
                node.version.as_str()
            }
        ))
    } else if text.starts_with("peer RPC error") {
        OperationError::Refused(format!("{}: {text}", node.name))
    } else {
        OperationError::Failed(anyhow::anyhow!("{}: {text}", node.name))
    }
}

// ── drift ────────────────────────────────────────────────────────────

/// The registry's version spread: each version with the nodes that run
/// it (stale ones marked), most common first. `drift` is true when the
/// nodes that are not stale run more than one version.
pub fn version_spread(nodes: &[NodeRow], stale_after_secs: u64) -> Value {
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut live = BTreeSet::new();
    for n in nodes {
        let stale = veles_core::node::is_stale(&n.heartbeat_at, stale_after_secs as i64);
        if !stale {
            live.insert(n.version.clone());
        }
        by.entry(n.version.clone())
            .or_default()
            .push(json!({"id": n.id, "name": n.name, "stale": stale}));
    }
    let mut groups: Vec<(String, Vec<Value>)> = by.into_iter().collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    json!({
        "drift": live.len() > 1,
        "groups": groups
            .into_iter()
            .map(|(version, nodes)| json!({"version": version, "nodes": nodes}))
            .collect::<Vec<_>>(),
    })
}

fn int_map(v: &Value) -> BTreeMap<String, i64> {
    v.as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_i64().map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default()
}

fn str_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Compare THIS node's fingerprint with another's (same nonce): what
/// differs, by name. Pure.
pub fn compare(here: &Value, there: &Value) -> Value {
    let hm = int_map(&here["config"]["log_marks"]);
    let tm = int_map(&there["config"]["log_marks"]);
    let mut here_ahead = Vec::new();
    let mut there_ahead = Vec::new();
    let authors: BTreeSet<&String> = hm.keys().chain(tm.keys()).collect();
    for a in authors {
        let (h, t) = (
            hm.get(a).copied().unwrap_or(0),
            tm.get(a).copied().unwrap_or(0),
        );
        if h > t {
            here_ahead.push(json!({"author": a, "here": h, "there": t}));
        } else if t > h {
            there_ahead.push(json!({"author": a, "here": h, "there": t}));
        }
    }
    let hk = str_map(&here["config"]["keys"]);
    let tk = str_map(&there["config"]["keys"]);
    let mut differ = Vec::new();
    let mut only_here = Vec::new();
    let mut only_there = Vec::new();
    for (k, h) in &hk {
        match tk.get(k) {
            Some(t) if t != h => differ.push(k.clone()),
            Some(_) => {}
            None => only_here.push(k.clone()),
        }
    }
    for k in tk.keys() {
        if !hk.contains_key(k) {
            only_there.push(k.clone());
        }
    }
    let nonce_same = here["nonce"] == there["nonce"];
    let log_same = here["config"]["log_hash"] == there["config"]["log_hash"]
        && here_ahead.is_empty()
        && there_ahead.is_empty();
    let keys_same = differ.is_empty() && only_here.is_empty() && only_there.is_empty();
    json!({
        "version_differs": here["version"] != there["version"],
        "revision_differs": here["revision"] != there["revision"],
        "version": there["version"],
        "revision": there["revision"],
        "inventory": there["inventory"],
        "config": {
            "comparable": nonce_same,
            "same": nonce_same && log_same && keys_same,
            "log_same": log_same,
            "log": {"here_ahead": here_ahead, "there_ahead": there_ahead},
            "keys": {"differ": differ, "only_here": only_here, "only_there": only_there},
            "keys_error": there["config"]["keys_error"],
        },
    })
}

/// Drift across the registry: every node that is not stale and has an
/// `[a2a_agents]` entry is asked for its fingerprint (concurrently,
/// bounded by the ask timeout) and compared with this node's.
pub async fn drift(store: &Arc<Mutex<Store>>, cfg: &VelesConfig) -> Result<Value, OperationError> {
    let nodes = store
        .lock_ok()
        .nodes_list()
        .map_err(OperationError::Failed)?;
    let me = me();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let here = {
        let (s, n) = (store.clone(), nonce.clone());
        blocking(move || local_fingerprint(&s, &n)).await?
    };
    let names: BTreeMap<String, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.name.clone()))
        .collect();
    let stale_after = cfg.cluster.stale_after_secs as i64;
    let mut asks = Vec::new();
    let mut rows = Vec::new();
    #[allow(clippy::type_complexity)]
    let mut fingerprints: Vec<(String, String, Option<Value>, Option<String>)> = Vec::new();
    for n in &nodes {
        if me.as_deref() == Some(n.id.as_str()) {
            continue;
        }
        let base = json!({
            "id": n.id,
            "name": n.name,
            "registry_version": n.version,
            "syncs_config": cfg.cluster.memsync_peers.iter().any(|p| p == &n.name || p == &n.id),
        });
        if veles_core::node::is_stale(&n.heartbeat_at, stale_after) {
            let mut b = base;
            b["asked"] = json!(false);
            b["error"] = json!("stale: not asked");
            fingerprints.push((
                n.id.clone(),
                n.name.clone(),
                None,
                Some("stale: not asked".into()),
            ));
            rows.push(b);
            continue;
        }
        let (store, cfg, id, nonce) = (store.clone(), cfg.clone(), n.id.clone(), nonce.clone());
        asks.push(async move {
            let r = ask(
                &store,
                &cfg,
                &id,
                json!({"op": "fingerprint", "nonce": nonce}),
            )
            .await;
            (base, r)
        });
    }
    for (mut b, r) in futures::future::join_all(asks).await {
        b["asked"] = json!(true);
        let (id, name) = (
            b["id"].as_str().unwrap_or("").to_string(),
            b["name"].as_str().unwrap_or("").to_string(),
        );
        match r {
            Ok(there) => {
                let c = compare(&here, &there);
                if let Some(obj) = c.as_object() {
                    for (k, v) in obj {
                        b[k] = v.clone();
                    }
                }
                b["error"] = Value::Null;
                fingerprints.push((id, name, Some(there), None));
            }
            Err(e) => {
                b["error"] = json!(e.to_string());
                fingerprints.push((id, name, None, Some(e.to_string())));
            }
        }
        rows.push(b);
    }
    // Deployment consistency over every node, this one included.
    let self_name = me
        .as_deref()
        .and_then(|id| names.get(id))
        .cloned()
        .unwrap_or_else(|| "this node".into());
    let self_id = me.clone().unwrap_or_default();
    let mut deployed = vec![Deployed {
        id: &self_id,
        name: &self_name,
        fingerprint: Some(&here),
        error: None,
    }];
    for (id, name, fp, err) in &fingerprints {
        deployed.push(Deployed {
            id,
            name,
            fingerprint: fp.as_ref(),
            error: err.clone(),
        });
    }
    let mut consistency = consistency(&deployed);
    consistency["checked"] = json!(true);
    consistency["checked_at"] = json!(chrono::Utc::now().to_rfc3339());
    *last_consistency().lock_ok() = consistency.clone();
    // Author ids are node ids: name them where the registry can.
    for r in rows.iter_mut() {
        for side in ["here_ahead", "there_ahead"] {
            // `pointer_mut`, not indexing: indexing a missing key inserts
            // it, and a row that was never compared would grow a config.
            if let Some(list) = r
                .pointer_mut(&format!("/config/log/{side}"))
                .and_then(Value::as_array_mut)
            {
                for item in list {
                    if let Some(name) = item["author"].as_str().and_then(|a| names.get(a)) {
                        item["author_name"] = json!(name);
                    }
                }
            }
        }
    }
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let differing = rows
        .iter()
        .filter(|r| {
            r["version_differs"] == json!(true)
                || r["revision_differs"] == json!(true)
                || r["config"]["same"] == json!(false)
        })
        .count();
    Ok(json!({
        "self": {
            "id": me,
            "version": here["version"],
            "revision": here["revision"],
            "config_rows": here["config"]["log_rows"],
            "config_keys": here["config"]["keys"].as_object().map_or(0, |m| m.len()),
        },
        "versions": version_spread(&nodes, cfg.cluster.stale_after_secs),
        "nodes": rows,
        "differing": differing,
        "consistency": consistency,
        "self_inventory": here["inventory"],
        "checked_at": chrono::Utc::now().to_rfc3339(),
    }))
}

// ── gossip now ───────────────────────────────────────────────────────

/// Exchange registry gossip now — with every `[cluster] gossip_peers`
/// entry, or with the one `peer` names (a gossip peer name, or a
/// registry node id or name that one of them is). The same exchange
/// and the same result record the cadence loop makes.
pub async fn gossip_now(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    peer: Option<&str>,
) -> Result<Value, OperationError> {
    let peers: Vec<String> = match peer.map(str::trim).filter(|p| !p.is_empty()) {
        None => cfg.cluster.gossip_peers.clone(),
        Some(p) => {
            let node = store
                .lock_ok()
                .nodes_list()
                .map_err(OperationError::Failed)?
                .into_iter()
                .find(|n| n.id == p || n.name == p);
            let wanted: Vec<String> = cfg
                .cluster
                .gossip_peers
                .iter()
                .filter(|g| {
                    g.as_str() == p
                        || node
                            .as_ref()
                            .is_some_and(|n| g.as_str() == n.name || g.as_str() == n.id)
                })
                .cloned()
                .collect();
            if wanted.is_empty() {
                return Err(OperationError::NotConfigured(format!(
                    "{p} is not in [cluster] gossip_peers"
                )));
            }
            wanted
        }
    };
    if peers.is_empty() {
        return Err(OperationError::NotConfigured(
            "no [cluster] gossip_peers are configured; gossip is off".into(),
        ));
    }
    let mut results = Vec::new();
    for p in peers {
        let Some(target) = cfg.a2a_agents.get(&p) else {
            let why = format!("{p} has no [a2a_agents] entry");
            crate::fleet_ops::record("gossip", &p, Err(&why));
            results.push(json!({"peer": p, "ok": false, "line": why}));
            continue;
        };
        let Some(_flight) = veles_core::storesync::PushInFlight::claim(&format!("gossip:{p}"))
        else {
            results.push(json!({"peer": p, "ok": false,
                "line": format!("a gossip exchange with {p} is already running")}));
            continue;
        };
        let (url, bearer) = (target.url.clone(), target.bearer());
        match veles_core::gossip::exchange_with_peer(store, &p, &url, bearer.as_deref()).await {
            Ok(line) => {
                crate::fleet_ops::record("gossip", &p, Ok(&line));
                results.push(json!({"peer": p, "ok": true, "line": line}));
            }
            Err(e) => {
                let why = format!("{e:#}");
                crate::fleet_ops::record("gossip", &p, Err(&why));
                results.push(json!({"peer": p, "ok": false, "line": why}));
            }
        }
    }
    let ok = results.iter().filter(|r| r["ok"] == json!(true)).count();
    Ok(json!({
        "results": results,
        "ok": ok,
        "result": format!("gossip: {ok} of {} peer(s) answered", results.len()),
    }))
}

// ── chat-surface text (`/fleet logs|metrics|drift|gossip`) ───────────

fn bytes(v: &Value) -> String {
    match v.as_u64() {
        Some(b) if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        Some(b) if b >= 1 << 20 => format!("{:.0} MiB", b as f64 / (1u64 << 20) as f64),
        Some(b) => format!("{b} B"),
        None => "?".into(),
    }
}

/// A log answer as chat lines.
pub fn logs_text(v: &Value) -> String {
    let lines = v["lines"].as_array().cloned().unwrap_or_default();
    let mut out = format!(
        "{} — last {} line(s) of {} held (redacted):\n",
        v["node"]["name"].as_str().unwrap_or("?"),
        lines.len(),
        v["held"]
    );
    for l in lines {
        out.push_str(&format!(
            "{} {} {}: {}\n",
            l["time"].as_str().unwrap_or(""),
            l["level"].as_str().unwrap_or(""),
            l["target"].as_str().unwrap_or(""),
            l["msg"].as_str().unwrap_or("")
        ));
    }
    out
}

/// A metrics answer as a few chat lines.
pub fn metrics_text(v: &Value) -> String {
    let mut out = format!(
        "{} — veles {} ({}), {} {}\n",
        v["node"]["name"].as_str().unwrap_or("?"),
        v["version"].as_str().unwrap_or("?"),
        v["revision"].as_str().unwrap_or("?"),
        v["os"].as_str().unwrap_or(""),
        v["arch"].as_str().unwrap_or("")
    );
    let cpu = &v["cpu"];
    if cpu["load1"].is_null() && cpu["busy_pct"].is_number() {
        // Windows keeps no load average: the node's CPU busy share since
        // its previous sample (hostmetrics::cpu_busy, FLEET-7).
        out.push_str(&format!(
            "cpu: {} core(s), {}% busy over the last {} s\n",
            cpu["cores"], cpu["busy_pct"], cpu["busy_window_secs"]
        ));
    } else {
        out.push_str(&format!(
            "cpu: {} core(s), load {} / {} / {}\n",
            cpu["cores"], cpu["load1"], cpu["load5"], cpu["load15"]
        ));
    }
    if v["memory"].is_object() {
        out.push_str(&format!(
            "memory: {} available of {} ({}% used)\n",
            bytes(&v["memory"]["available_bytes"]),
            bytes(&v["memory"]["total_bytes"]),
            v["memory"]["used_pct"]
        ));
    }
    if v["disk"].is_object() {
        out.push_str(&format!(
            "disk: {} free of {} at {}\n",
            bytes(&v["disk"]["free_bytes"]),
            bytes(&v["disk"]["total_bytes"]),
            v["disk"]["path"].as_str().unwrap_or("?")
        ));
    }
    if v["io_pressure"].is_object() {
        out.push_str(&format!(
            "io pressure: some {}% / {}% / {}% (10 s / 1 min / 5 min)\n",
            v["io_pressure"]["some_avg10"],
            v["io_pressure"]["some_avg60"],
            v["io_pressure"]["some_avg300"]
        ));
    }
    out.push_str(&format!(
        "uptime: host {} s, gateway {} s\n",
        v["host_uptime_secs"], v["gateway_uptime_secs"]
    ));
    out.push_str(&format!(
        "in flight: {} turn(s) {}, {} task(s) {}\n",
        v["in_flight"]["turns_total"],
        v["in_flight"]["turns"],
        v["in_flight"]["tasks_total"],
        v["in_flight"]["tasks"]
    ));
    for u in v["unavailable"].as_array().into_iter().flatten() {
        out.push_str(&format!("unavailable: {}\n", u.as_str().unwrap_or("")));
    }
    out
}

/// A drift answer as chat lines.
pub fn drift_text(v: &Value) -> String {
    let mut out = consistency_text(&v["consistency"]);
    for g in v["versions"]["groups"].as_array().into_iter().flatten() {
        let names: Vec<&str> = g["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n["name"].as_str())
            .collect();
        out.push_str(&format!(
            "version {}: {}\n",
            g["version"].as_str().unwrap_or("?"),
            names.join(", ")
        ));
    }
    for n in v["nodes"].as_array().into_iter().flatten() {
        let name = n["name"].as_str().unwrap_or("?");
        if let Some(e) = n["error"].as_str() {
            out.push_str(&format!("{name}: {e}\n"));
            continue;
        }
        let mut what = Vec::new();
        if n["version_differs"] == json!(true) {
            what.push(format!("version {}", n["version"].as_str().unwrap_or("?")));
        }
        if n["revision_differs"] == json!(true) {
            what.push(format!(
                "revision {}",
                n["revision"].as_str().unwrap_or("?")
            ));
        }
        let keys = &n["config"]["keys"];
        for (label, field) in [
            ("differs", "differ"),
            ("only here", "only_here"),
            ("only there", "only_there"),
        ] {
            let list: Vec<&str> = keys[field]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !list.is_empty() {
                what.push(format!("config {label}: {}", list.join(", ")));
            }
        }
        for (label, side) in [("ahead", "here_ahead"), ("behind", "there_ahead")] {
            for a in n["config"]["log"][side].as_array().into_iter().flatten() {
                what.push(format!(
                    "config log {label} for {} ({} here, {} there)",
                    a["author_name"]
                        .as_str()
                        .or(a["author"].as_str())
                        .unwrap_or("?"),
                    a["here"],
                    a["there"]
                ));
            }
        }
        if what.is_empty() {
            out.push_str(&format!("{name}: no drift\n"));
        } else {
            out.push_str(&format!("{name}: {}\n", what.join("; ")));
        }
    }
    if v["nodes"].as_array().is_none_or(|n| n.is_empty()) {
        out.push_str("no other registered node\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FLEET-7: `/fleet metrics` for a Windows node shows its CPU busy
    /// share, not a load average of nulls.
    #[test]
    fn a_windows_nodes_metrics_text_shows_its_busy_share() {
        let v = json!({
            "node": {"name": "winbox"}, "version": "0.2.2", "revision": "r",
            "os": "windows", "arch": "x86_64",
            "cpu": {"cores": 8, "busy_pct": 37.3, "busy_window_secs": 30.0},
            "unavailable": [],
        });
        let text = metrics_text(&v);
        assert!(
            text.contains("cpu: 8 core(s), 37.3% busy over the last 30.0 s"),
            "{text}"
        );
        assert!(!text.contains("load null"), "{text}");
    }

    fn registered(store: &Store, id: &str, name: &str, version: &str) {
        store
            .node_register(
                id,
                name,
                "host",
                version,
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
    }

    /// The log tail: bounded, level-filtered, and every line through both
    /// redactors — a registered secret value and a credential shape that
    /// was never registered alike.
    #[test]
    fn the_log_tail_is_bounded_filtered_and_redacted() {
        // scratch_home holds the environment lock for the whole test (do
        // not take `testenv::guard` as well: it is the same lock).
        let (_home, _env) = veles_core::testenv::scratch_home();
        let secret = "fleet-test-secret-value-77";
        std::fs::write(
            veles_core::config::config_dir().join("secrets.env"),
            format!("FLEET_TEST_SECRET={secret}\n"),
        )
        .unwrap();
        veles_core::logbuf::clear();
        for i in 0..5 {
            veles_core::logbuf::push(veles_core::logbuf::LogLine {
                time: format!("00:00:0{i}"),
                level: "INFO".into(),
                target: "veles".into(),
                msg: format!("line {i}"),
            });
        }
        veles_core::logbuf::push(veles_core::logbuf::LogLine {
            time: "00:00:09".into(),
            level: "WARN".into(),
            target: "veles".into(),
            msg: format!("leaked {secret} and sk-ant-abcdefghijklmnopqrstuvwxyz0123"),
        });
        let v = local_logs(3, "all");
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 3, "{v}");
        assert_eq!(lines[0]["msg"], "line 3", "the LAST lines, oldest first");
        let warn = lines[2]["msg"].as_str().unwrap();
        assert!(!warn.contains(secret), "{warn}");
        assert!(warn.contains("FLEET_TEST_SECRET=<REDACTED>"), "{warn}");
        assert!(!warn.contains("abcdefghijklmnop"), "{warn}");
        let only_warn = local_logs(50, "warn");
        assert_eq!(
            only_warn["lines"].as_array().unwrap().len(),
            1,
            "{only_warn}"
        );
        assert_eq!(local_logs(0, "all")["lines"].as_array().unwrap().len(), 1);
        assert!(
            local_logs(100_000, "all")["lines"]
                .as_array()
                .unwrap()
                .len()
                <= MAX_LINES
        );
        veles_core::logbuf::clear();
    }

    /// The metrics answer names the build and counts this process's work.
    #[test]
    fn metrics_carry_version_revision_and_running_work() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            Store::open(&veles_core::config::veles_home().join("state.db")).unwrap(),
        ));
        store
            .lock_ok()
            .turn_checkpoint_start("s-1", "turn-1", "desktop", 0, "host")
            .unwrap();
        let v = answer_local(&store, &json!({"op": "metrics"})).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["revision"], revision());
        assert_eq!(v["in_flight"]["turns"]["desktop"], 1, "{v}");
        assert!(v["cpu"]["cores"].as_u64().unwrap() >= 1, "{v}");
        assert_eq!(v["op"], "metrics");
        assert!(answer_local(&store, &json!({"op": "shell"})).is_err());
    }

    /// Drift compares by name and hash: the keys that differ, the authors
    /// one side has more config rows from, and the build — never a value.
    #[test]
    fn fingerprints_compare_by_key_and_author_without_values() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            Store::open(&veles_core::config::veles_home().join("state.db")).unwrap(),
        ));
        std::fs::write(
            veles_core::configsync::config_path(),
            "[model]\ndefault = \"secret-model-name\"\n[agent]\nmax_turns = 9\n",
        )
        .unwrap();
        let nonce = "0123456789abcdef0123";
        let here = local_fingerprint(&store, nonce);
        assert!(
            !here.to_string().contains("secret-model-name"),
            "a value never leaves the node: {here}"
        );
        assert_eq!(here["nonce"], nonce);
        let mut there = here.clone();
        assert_eq!(compare(&here, &there)["config"]["same"], true);
        there["config"]["keys"]["model.default"] = json!("ffffffffffffffff");
        there["config"]["keys"]
            .as_object_mut()
            .unwrap()
            .remove("agent.max_turns");
        there["config"]["keys"]["web.backends"] = json!("0000000000000000");
        there["config"]["log_marks"] = json!({"laptop-id": 4});
        there["revision"] = json!("other");
        let c = compare(&here, &there);
        assert_eq!(c["config"]["same"], false, "{c}");
        assert_eq!(c["config"]["keys"]["differ"], json!(["model.default"]));
        assert_eq!(c["config"]["keys"]["only_here"], json!(["agent.max_turns"]));
        assert_eq!(c["config"]["keys"]["only_there"], json!(["web.backends"]));
        assert_eq!(c["config"]["log"]["there_ahead"][0]["author"], "laptop-id");
        assert_eq!(c["revision_differs"], true);
        assert_eq!(c["version_differs"], false);
        // A short or absent nonce is replaced, so the hashes are salted.
        let unsalted = local_fingerprint(&store, "");
        assert_eq!(unsalted["nonce"].as_str().unwrap().len(), 32);
        assert_eq!(
            compare(&here, &unsalted)["config"]["comparable"],
            false,
            "different nonces cannot be compared"
        );
    }

    #[test]
    fn version_spread_flags_drift_among_live_nodes_only() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, "a", "laptop", "0.2.0");
        registered(&store, "b", "vps", "0.2.0");
        registered(&store, "c", "phone", "0.1.6");
        let nodes = store.nodes_list().unwrap();
        let v = version_spread(&nodes, 300);
        assert_eq!(v["drift"], true, "{v}");
        assert_eq!(v["groups"][0]["version"], "0.2.0", "most common first");
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = '2000-01-01 00:00:00' WHERE id = 'c'",
                [],
            )
            .unwrap();
        let v = version_spread(&store.nodes_list().unwrap(), 300);
        assert_eq!(
            v["drift"], false,
            "a stale node's old version is no drift: {v}"
        );
    }

    /// The asking side refuses before any dial: an unknown op, an unknown
    /// node, a node with no `[a2a_agents]` entry (the registry endpoint is
    /// never a fallback); and answers for this node itself locally.
    #[tokio::test]
    async fn asking_refuses_unknown_and_undialable_nodes_and_answers_self_locally() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let me = veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())
            .unwrap();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        registered(&store, &me.id, "me", "0.2.0");
        registered(&store, "id-vps", "vps", "0.2.0");
        let store = Arc::new(Mutex::new(store));
        let cfg = VelesConfig::default();
        let e = ask(&store, &cfg, "vps", json!({"op": "shell"}))
            .await
            .unwrap_err();
        assert!(matches!(e, OperationError::InvalidOperation(_)), "{e}");
        let e = ask(&store, &cfg, "nobody", json!({"op": "metrics"}))
            .await
            .unwrap_err();
        assert!(matches!(e, OperationError::UnknownNode), "{e}");
        let e = ask(&store, &cfg, "vps", json!({"op": "logs"}))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("[a2a_agents]"), "{e}");
        let v = ask(&store, &cfg, &me.id, json!({"op": "metrics"}))
            .await
            .unwrap();
        assert_eq!(v["via"], "local", "{v}");
        let node = store.lock_ok().node_get("id-vps").unwrap().unwrap();
        assert!(matches!(
            rpc_error(&node, "peer RPC error -32601: method not found: veles/fleet"),
            OperationError::Refused(m) if m.contains("0.2.0 or later")
        ));
    }

    #[tokio::test]
    async fn gossip_now_needs_a_configured_peer() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            Store::open(&veles_core::config::veles_home().join("state.db")).unwrap(),
        ));
        let mut cfg = VelesConfig::default();
        let e = gossip_now(&store, &cfg, None).await.unwrap_err();
        assert!(e.to_string().contains("gossip_peers"), "{e}");
        cfg.cluster.gossip_peers = vec!["vps".into()];
        let e = gossip_now(&store, &cfg, Some("phone")).await.unwrap_err();
        assert!(matches!(e, OperationError::NotConfigured(_)), "{e}");
        // A named peer without an [a2a_agents] entry is a failed pass,
        // recorded for the Instances tab like an automatic one.
        let v = gossip_now(&store, &cfg, Some("vps")).await.unwrap();
        assert_eq!(v["results"][0]["ok"], false, "{v}");
        assert_eq!(
            crate::fleet_ops::gossip_status(&cfg)[0]["last"]["ok"],
            false
        );
    }

    fn deployed(version: &str, revision: &str, items: Value, shadows: Value) -> Value {
        json!({"version": version, "revision": revision,
               "inventory": {"items": items, "shadows": shadows}})
    }

    /// Deployment consistency (0.2.2): the 0.2.0 fleet — three builds on
    /// four nodes and a stale user copy of the Anthropic driver — is
    /// flagged node by node with what differs, an older node or one that
    /// could not be asked makes the verdict unknown, and "consistent" is
    /// said only when every node agrees on everything. (A node that
    /// lacks an item: consistency_names_what_a_node_lacks.)
    #[test]
    fn consistency_flags_the_node_that_differs_and_claims_nothing_it_did_not_see() {
        let same = json!({"module:telegram": "0.2.2", "driver:anthropic": "1.0 aaaa"});
        let a = deployed("0.2.2", "r1", same.clone(), json!([]));
        let b = deployed("0.2.2", "r1", same.clone(), json!([]));
        let c = deployed("0.2.2", "r1", same.clone(), json!([]));
        let node = |id: &'static str, f: &'static Value| Deployed {
            id,
            name: id,
            fingerprint: Some(f),
            error: None,
        };
        let (a, b, c) = (
            Box::leak(Box::new(a)),
            Box::leak(Box::new(b)),
            Box::leak(Box::new(c)),
        );
        let ok = consistency(&[node("laptop", a), node("vps", b), node("phone", c)]);
        assert_eq!(ok["consistent"], true, "{ok}");
        assert!(ok["statement"].as_str().unwrap().contains("is consistent"));

        // 0.2.0 as it was: another build on the phone, another revision
        // on the vps, and a user copy of the driver on the laptop.
        let stale_driver = Box::leak(Box::new(deployed(
            "0.2.2",
            "r1",
            json!({"module:telegram": "0.2.2", "driver:anthropic": "0.9 bbbb"}),
            json!([{"name": "anthropic", "user_sha256": "bbbb", "bundled_sha256": "aaaa",
                    "same_content": false}]),
        )));
        let other_rev = Box::leak(Box::new(deployed("0.2.2", "r0", same.clone(), json!([]))));
        let other_build = Box::leak(Box::new(deployed("0.2.1", "r1", same.clone(), json!([]))));
        let fourth = Box::leak(Box::new(deployed("0.2.2", "r1", same.clone(), json!([]))));
        let bad = consistency(&[
            node("laptop", stale_driver),
            node("vps", other_rev),
            node("phone", other_build),
            node("winbox", fourth),
        ]);
        assert_eq!(bad["consistent"], false, "{bad}");
        let flagged = |id: &str| -> Vec<String> {
            bad["flagged"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["id"] == id)
                .map(|f| {
                    f["differs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|d| d.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(flagged("laptop")
            .iter()
            .any(|d| d.contains("driver:anthropic 0.9 bbbb")));
        assert!(flagged("laptop")
            .iter()
            .any(|d| d.contains("shadows the bundled one (user bbbb, bundled aaaa)")));
        assert_eq!(flagged("vps"), vec!["revision r0".to_string()]);
        assert_eq!(flagged("phone"), vec!["version 0.2.1".to_string()]);
        assert!(
            flagged("winbox").is_empty(),
            "the node that agrees is not flagged"
        );
        assert!(bad["statement"]
            .as_str()
            .unwrap()
            .contains("NOT consistent"));

        // An older node (no inventory) and one that was not asked: the
        // verdict is unknown, never "consistent".
        let old = Box::leak(Box::new(json!({"version": "0.2.1", "revision": "r1"})));
        let unknown = consistency(&[
            node("laptop", a),
            node("vps", old),
            Deployed {
                id: "phone",
                name: "phone",
                fingerprint: None,
                error: Some("stale: not asked".into()),
            },
        ]);
        assert_eq!(unknown["consistent"], false);
        assert_eq!(unknown["unknown"].as_array().unwrap().len(), 2, "{unknown}");
        assert!(consistency_text(&unknown).contains("phone not compared: stale"));
        // A shadow with the bundled content is listed, not flagged.
        let same_shadow = Box::leak(Box::new(deployed(
            "0.2.2",
            "r1",
            same.clone(),
            json!([{"name": "anthropic", "user_sha256": "aaaa", "bundled_sha256": "aaaa",
                    "same_content": true}]),
        )));
        let listed = consistency(&[node("laptop", same_shadow), node("vps", c)]);
        assert_eq!(listed["consistent"], true, "{listed}");
        assert_eq!(listed["shadows"].as_array().unwrap().len(), 1);
        // This node reports what it runs: version, revision, inventory.
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            Store::open(&veles_core::config::veles_home().join("state.db")).unwrap(),
        ));
        let fp = local_fingerprint(&store, "n");
        assert!(fp["inventory"]["items"].is_object(), "{fp}");
        assert!(fp["inventory"]["shadows"].is_array(), "{fp}");
    }

    /// The decision for an item not every node has: version, revision
    /// and every item two or more nodes share must agree for
    /// "consistent", and an item missing from a node is always named,
    /// with the nodes that lack it — a Windows node has no Racket or Lisp
    /// module, a phone its own set — and the verdict is then "consistent
    /// with differences", never plain "consistent". consistency() used
    /// to compare an item only among the nodes that report it, so a node
    /// that lacked a module the others ran was never mentioned.
    #[test]
    fn consistency_names_what_a_node_lacks() {
        let node = |id: &'static str, f: Value| Deployed {
            id,
            name: id,
            fingerprint: Some(Box::leak(Box::new(f))),
            error: None,
        };
        let full = json!({"module:telegram": "0.2.2", "module:racket": "0.2.2",
                          "driver:anthropic": "1.0 aaaa"});
        let text = |c: &Value| consistency_text(c);

        // All agree.
        let all = consistency(&[
            node("laptop", deployed("0.2.2", "r1", full.clone(), json!([]))),
            node("vps", deployed("0.2.2", "r1", full.clone(), json!([]))),
        ]);
        assert_eq!(all["consistent"], true, "{all}");
        assert_eq!(all["verdict"], "consistent", "{all}");
        assert!(all["missing"].as_array().unwrap().is_empty(), "{all}");

        // One node on another revision: not consistent, that node named.
        let rev = consistency(&[
            node("laptop", deployed("0.2.2", "r1", full.clone(), json!([]))),
            node("vps", deployed("0.2.2", "r1", full.clone(), json!([]))),
            node("phone", deployed("0.2.2", "r0", full.clone(), json!([]))),
        ]);
        assert_eq!(rev["consistent"], false, "{rev}");
        assert_eq!(rev["verdict"], "not_consistent", "{rev}");
        assert!(
            text(&rev).contains("phone differs: revision r0"),
            "{}",
            text(&rev)
        );

        // One node lacking a module: named with the node, never plain
        // "consistent".
        let mut no_racket = full.clone();
        no_racket.as_object_mut().unwrap().remove("module:racket");
        let lacks = consistency(&[
            node("laptop", deployed("0.2.2", "r1", full.clone(), json!([]))),
            node("vps", deployed("0.2.2", "r1", full.clone(), json!([]))),
            node("winbox", deployed("0.2.2", "r1", no_racket, json!([]))),
        ]);
        assert_eq!(lacks["consistent"], false, "{lacks}");
        assert_eq!(lacks["verdict"], "consistent_with_differences", "{lacks}");
        let statement = lacks["statement"].as_str().unwrap();
        assert!(
            statement.contains("consistent with differences"),
            "{statement}"
        );
        assert!(
            !statement.starts_with("The fleet is consistent:"),
            "{statement}"
        );
        let missing = lacks["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 1, "{lacks}");
        assert_eq!(missing[0]["item"], "module:racket");
        assert_eq!(missing[0]["lacking"][0]["name"], "winbox");
        assert!(
            text(&lacks).contains("module:racket is not on: winbox"),
            "{}",
            text(&lacks)
        );

        // The real case: a stale user copy of the Anthropic driver on one
        // node (the VPS ran a 2026-10-29 copy through two releases).
        // Through this node's own inventory, as a fingerprint carries it.
        let (_cfg, _data, _guard) = veles_core::testenv::scratch_xdg();
        let bundled = tempfile::tempdir().unwrap();
        let plant = |root: &std::path::Path, body: &str| {
            let dir = root.join("anthropic");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.toml"), "name = \"anthropic\"\n").unwrap();
            std::fs::write(dir.join("plugin.tcl"), body).unwrap();
        };
        plant(bundled.path(), "# the driver as 0.2.2 ships it\n");
        std::env::set_var(veles_core::plugins::BUNDLED_ENV, bundled.path());
        plant(
            &veles_core::plugins::plugins_dir(),
            "# a copy from 2026-10-29\n",
        );
        let inv = local_inventory();
        let vps = json!({"version": "0.2.2", "revision": "r1", "inventory": {
            "items": full.clone(), "shadows": inv["shadows"].clone()}});
        let stale = consistency(&[
            node("laptop", deployed("0.2.2", "r1", full.clone(), json!([]))),
            node("vps", vps),
        ]);
        assert_eq!(stale["consistent"], false, "{stale}");
        assert_eq!(stale["verdict"], "not_consistent", "{stale}");
        assert!(
            text(&stale).contains("vps differs: user plugin anthropic shadows the bundled one"),
            "{}",
            text(&stale)
        );
    }
}
