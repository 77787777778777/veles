//! The command center's node management (0.2.2, owner ask 287976: "We
//! should also be able to deploy/redeploy/manage the nodes themselves
//! and all of kinds of sync in it"): restart a node's gateway, read and
//! write one node's config key, bulk probe and sync, and the audit
//! trail of every command-center action on the asking gateway.
//!
//! THE CALL. Like `veles/fleet` (fleet_obs.rs), a registry row is
//! identity only: a remote node is dialed on the operator's
//! `[a2a_agents]` entry named like it, with its bearer. The requests
//! travel as their OWN JSON-RPC method, [`METHOD`] — not a new op of
//! `veles/fleet`, so that the node that answers them gates them apart,
//! and not a `message/send` text (HANDOFF pitfall 108): a 0.2.1 or
//! older node answers `-32601 method not found`, which is said as "it
//! runs veles X".
//!
//! WHO MAY ASK. The asking side is owner-only: the desktop API (owner
//! token) and `/fleet` from a caller holding `all`. The answering node
//! answers only an authenticated peer named in its `[cluster]
//! memsync_peers` — the list config sync already uses for "who may push
//! config to me" — never during a lockdown, and logs every request in
//! its A2A audit log (`a2a_server::fleet_manage_request`). There is no
//! second trust model.
//!
//! WHAT A CHANGE MUST CARRY. A restart or a config write is refused
//! unless it is FRESH: a `request_id` this node has not seen and an
//! `at` within [`WINDOW_SECS`] of its clock. Each also names the state
//! it was decided on, so a replay outside the remembered window still
//! fails: a restart names the node's boot (`expect_boot`, which the
//! restart changes), a config write the hash of the value it replaces
//! (`expect`).
//!
//! CONFIG. Exactly the keys config sync carries, under its rules: a
//! node-local key (`[node]`, `[cluster]`, `[a2a_agents]`, `[tui]`, any
//! section config sync does not know) is never read or written from
//! another node; a key in a held section (`[acl]`, approvals, sandbox,
//! hooks, plugins, MCP servers) is not applied from a peer — it is
//! edited on a node and pushed, and waits on the receiving node for its
//! owner (`/configsync apply`), as config sync does; a credential (a
//! secret-shaped value, or a key named like one) is never sent either
//! way. A write lands in the node's own config.toml like an edit made
//! there, so config sync then carries it to the nodes that sync config
//! with it, exactly as it would a local edit.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use veles_core::config::VelesConfig;
use veles_core::lockutil::LockOk;
use veles_core::store::{NodeRow, Store};
use veles_core::storesync::OperationError;
use veles_core::supervisor::Supervisor;

/// The node-to-node method: `{op: status|restart|config_get|config_set, …}`.
pub const METHOD: &str = "veles/fleet-manage";
/// How far a change request's `at` may be from the answering node's
/// clock, and how long its `request_id` is remembered.
pub const WINDOW_SECS: i64 = 120;
/// How long a remote ask may take.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the asker waits for a restarted node to come back.
pub const BACK_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a node waits between answering a restart and stopping, so
/// the answer leaves first.
#[cfg_attr(test, allow(dead_code))]
const RESTART_DELAY: Duration = Duration::from_millis(1500);

// ── the audit trail on the asking gateway ────────────────────────────

/// `fleet_audit.jsonl` in the veles home: one line per command-center
/// action this gateway asked for (who, what, which node, the result).
pub fn audit_path() -> PathBuf {
    veles_core::config::veles_home().join("fleet_audit.jsonl")
}

/// The most records the file keeps; past twice this it is cut back.
const AUDIT_KEEP: usize = 2000;

fn audit_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Append one record. `result` is a short summary; it passes through
/// the outbound redactor, and nothing that carries a value or a token
/// is ever handed to it.
pub fn audit(who: &str, action: &str, node: &str, ok: bool, result: &str) {
    let mut summary = veles_core::a2a::redact_outbound(result.trim());
    if summary.chars().count() > 300 {
        summary = summary.chars().take(300).collect::<String>() + "…";
    }
    let line = json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "who": who,
        "action": action,
        "node": node,
        "ok": ok,
        "result": summary,
    })
    .to_string();
    let _guard = audit_lock().lock_ok();
    let path = audit_path();
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()));
    if let Err(e) = appended {
        tracing::warn!("fleet audit write failed: {e}");
        return;
    }
    // Bounded: cut back to the newest AUDIT_KEEP records once the file
    // holds twice that.
    if let Ok(text) = std::fs::read_to_string(&path) {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > 2 * AUDIT_KEEP {
            let kept = lines[lines.len() - AUDIT_KEEP..].join("\n") + "\n";
            let tmp = path.with_extension("jsonl.tmp");
            if std::fs::write(&tmp, kept).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// Audit an operation's outcome and hand it back unchanged.
pub fn audited(
    who: &str,
    action: &str,
    node: &str,
    r: Result<Value, OperationError>,
) -> Result<Value, OperationError> {
    match &r {
        Ok(v) => audit(who, action, node, true, &summary_of(v)),
        Err(e) => audit(who, action, node, false, &e.to_string()),
    }
    r
}

/// A one-line summary of an operation's answer, for the audit trail.
fn summary_of(v: &Value) -> String {
    if v["op"] == json!("config_get") {
        return if v["withheld"] == json!(true) {
            "read: a credential, withheld".into()
        } else if v["set"] == json!(true) {
            "read: set".into()
        } else {
            "read: not set".into()
        };
    }
    for k in ["result", "note", "line"] {
        if let Some(s) = v[k].as_str().filter(|s| !s.is_empty()) {
            return s.to_string();
        }
    }
    if let Some(r) = v["reachable"].as_bool() {
        return if r {
            format!("reachable in {} ms", v["latency_ms"])
        } else {
            format!("unreachable: {}", v["error"].as_str().unwrap_or(""))
        };
    }
    if v["confirm_required"] == json!(true) {
        return format!(
            "listed {} target(s), not confirmed",
            v["targets"].as_array().map_or(0, Vec::len)
        );
    }
    "done".into()
}

/// The newest `limit` records, newest first.
pub fn audit_recent(limit: usize) -> Vec<Value> {
    let _guard = audit_lock().lock_ok();
    let text = std::fs::read_to_string(audit_path()).unwrap_or_default();
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .take(limit.clamp(1, AUDIT_KEEP))
        .collect()
}

/// The audit trail as chat text.
pub fn audit_text(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "No command-center action is recorded on this gateway yet.\n".into();
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "{} {} {} {} [{}] {}\n",
            r["at"].as_str().unwrap_or("?"),
            r["who"].as_str().unwrap_or("?"),
            r["action"].as_str().unwrap_or("?"),
            r["node"].as_str().filter(|n| !n.is_empty()).unwrap_or("-"),
            if r["ok"] == json!(true) {
                "ok"
            } else {
                "failed"
            },
            r["result"].as_str().unwrap_or("")
        ));
    }
    out
}

// ── what this node answers ───────────────────────────────────────────

fn seen() -> &'static Mutex<HashMap<String, i64>> {
    static SEEN: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A change request is fresh: an unseen `request_id` (16 to 64
/// visible characters) and an `at` (unix seconds) within
/// [`WINDOW_SECS`] of this node's clock. The id is remembered.
fn fresh(params: &Value) -> Result<(), String> {
    let id = params["request_id"].as_str().unwrap_or("");
    if !(16..=64).contains(&id.len()) || !id.chars().all(|c| c.is_ascii_graphic()) {
        return Err("a change request needs a request_id (16 to 64 characters)".into());
    }
    let now = chrono::Utc::now().timestamp();
    let Some(at) = params["at"].as_i64() else {
        return Err("a change request needs its time (`at`, unix seconds)".into());
    };
    if (now - at).abs() > WINDOW_SECS {
        return Err(format!(
            "this request was made {}s from this node's clock, more than {WINDOW_SECS}s: \
             refused as stale or replayed (check both clocks)",
            now - at
        ));
    }
    let mut seen = seen().lock_ok();
    seen.retain(|_, t| now - *t <= 2 * WINDOW_SECS);
    if seen.contains_key(id) {
        return Err("this request was already received: refused as a replay".into());
    }
    seen.insert(id.to_string(), now);
    Ok(())
}

/// A key named like a credential: config sync withholds secret-shaped
/// VALUES; the command center also keeps away from keys that are
/// credentials by name, whatever their value looks like.
fn secret_named(segment: &str) -> bool {
    let s = segment.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "api_key",
        "apikey",
        "bearer",
        "credential",
        "private_key",
        "auth",
        "cookie",
    ]
    .iter()
    .any(|w| s.contains(w))
}

/// Whether `value` (under `key`) carries a credential anywhere: a key
/// segment named like one, a nested key named like one, or a string
/// shaped like one.
fn carries_secret(key: &str, value: Option<&Value>) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::String(s) => veles_core::a2a::secret_shaped(s),
            Value::Array(a) => a.iter().any(walk),
            Value::Object(m) => m.iter().any(|(k, v)| secret_named(k) || walk(v)),
            _ => false,
        }
    }
    key.split('.').any(secret_named) || value.is_some_and(walk)
}

/// The hash a config write names as the value it replaces.
pub fn value_hash(v: Option<&Value>) -> String {
    let text = match v {
        None => "unset".to_string(),
        Some(v) => format!("set\0{v}"),
    };
    veles_core::memory::MemoryStore::content_sha(&text)
        .chars()
        .take(16)
        .collect()
}

/// Whether the command center may touch `key` at all: the keys config
/// sync carries, minus the ones it holds for a node's owner.
pub fn key_verdict(key: &str, writing: bool) -> Result<(), String> {
    let Ok(segs) = veles_core::configsync::split_key(key) else {
        return Err(format!("`{key}` is not a config key path"));
    };
    // A table that holds a node-local key is node-local too. Config sync
    // carries only the leaves under it and leaves the node's own out;
    // written whole, the table would replace them (`platforms.a2a.extra
    // = {host = "0.0.0.0"}` rebinds that node's A2A server to every
    // interface), and read whole it would show them. Said before the
    // general answer below (which `is_replicated_key` also gives such a
    // table), because it names the key to use instead.
    if let Some(inner) = veles_core::configsync::NODE_KEYS.iter().find(|k| {
        let inner: Vec<&str> = k.split('.').collect();
        inner.len() > segs.len() && segs.iter().zip(&inner).all(|(a, b)| a == b)
    }) {
        return Err(format!(
            "`{key}` holds the node-local key `{inner}`: config sync never carries that from \
             one node to another, so the command center neither reads nor writes `{key}` \
             whole; name a key under it"
        ));
    }
    if !veles_core::configsync::is_replicated_key(key) {
        return Err(format!(
            "`{key}` is node-local: config sync never carries it from one node to another, \
             so the command center neither reads nor writes it there; edit it on that node"
        ));
    }
    if writing && veles_core::configsync::is_held_key(key) {
        let top = key.split('.').next().unwrap_or(key);
        return Err(format!(
            "[{top}] is a held section: config sync never applies a change to it from another \
             node without the receiving node's owner. Edit it on this gateway and push config \
             (Sync now, config): the receiving node holds it until its owner applies it there \
             (/configsync apply)"
        ));
    }
    Ok(())
}

/// Answer one management request for THIS node. `sup` is what
/// supervises this process; `from_peer` is a request another node sent
/// (veles/fleet-manage), not this gateway's own command center.
/// Blocking (config.toml, the store); call it off the async runtime.
/// The caller has checked who asks.
pub fn answer_local(
    store: &Arc<Mutex<Store>>,
    params: &Value,
    sup: &Supervisor,
    from_peer: bool,
) -> Result<Value, String> {
    let op = params["op"].as_str().unwrap_or("");
    let changes = matches!(op, "restart" | "config_set");
    if changes && veles_core::lockdown::current(Some(store)).is_some() {
        return Err(
            "this node is in lockdown: it is not restarted or reconfigured from \
                    the command center until the lockdown is released"
                .into(),
        );
    }
    let mut v = match op {
        "status" => status_local(store, sup),
        "restart" => restart_local(store, params, sup)?,
        "config_get" => config_get_local(params["key"].as_str().unwrap_or("").trim())?,
        "config_set" => config_set_local(params, from_peer)?,
        other => return Err(format!("unknown fleet management request `{other}`")),
    };
    v["op"] = json!(op);
    Ok(v)
}

fn status_local(store: &Arc<Mutex<Store>>, sup: &Supervisor) -> Value {
    json!({
        "boot": veles_core::supervisor::boot_id(),
        "supervisor": sup.json(),
        "in_flight": crate::fleet_obs::in_flight(store),
        "version": env!("CARGO_PKG_VERSION"),
        "revision": crate::fleet_obs::revision(),
    })
}

fn restart_local(
    store: &Arc<Mutex<Store>>,
    params: &Value,
    sup: &Supervisor,
) -> Result<Value, String> {
    fresh(params)?;
    let boot = veles_core::supervisor::boot_id();
    if params["expect_boot"].as_str() != Some(boot) {
        return Err(format!(
            "this restart was decided for boot {}, and this node runs boot {boot}: it has \
             restarted since (a stale or replayed request)",
            params["expect_boot"].as_str().unwrap_or("(none)")
        ));
    }
    if !sup.can_restart() {
        return Err(format!(
            "this node cannot restart itself: {}. It stays up; restart it where it runs",
            sup.detail()
        ));
    }
    let work = crate::fleet_obs::in_flight(store);
    let turns = work["turns_total"].as_i64().unwrap_or(0);
    let force = params["force"].as_bool().unwrap_or(false);
    if turns > 0 && !force {
        return Err(format!(
            "{turns} turn(s) are running here ({}): a restart now cuts them off. Desktop turns \
             continue from their saved history after the restart; connector, A2A, mail, cron \
             and subagent turns do not. Ask again with force to restart anyway",
            work["turns"]
        ));
    }
    schedule_restart();
    Ok(json!({
        "restarting": true,
        "boot": boot,
        "supervisor": sup.json(),
        "turns_running": turns,
        "result": format!("restarting: {}", sup.detail()),
    }))
}

#[cfg(test)]
pub(crate) static RESTARTS_SCHEDULED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Stop this gateway shortly (after the answer has left) and exit with
/// the restart code. Unit tests count instead.
fn schedule_restart() {
    #[cfg(test)]
    {
        RESTARTS_SCHEDULED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    #[cfg(not(test))]
    {
        tracing::warn!("fleet: restart requested by the command center");
        std::thread::spawn(|| {
            std::thread::sleep(RESTART_DELAY);
            veles_core::supervisor::request_restart();
        });
    }
}

fn config_get_local(key: &str) -> Result<Value, String> {
    key_verdict(key, false)?;
    let doc = veles_core::configedit::read_doc().map_err(|e| format!("{e:#}"))?;
    let segs: Vec<&str> = key.split('.').collect();
    let value = veles_core::configedit::get_json(&doc, &segs);
    let withheld = carries_secret(key, value.as_ref());
    let effect = veles_core::configedit::effect_for(key);
    let cfg = veles_core::config::load().unwrap_or_default();
    Ok(json!({
        "key": key,
        "set": value.is_some(),
        "value": if withheld { Value::Null } else { value.clone().unwrap_or(Value::Null) },
        "withheld": withheld,
        "hash": if withheld { Value::Null } else { json!(value_hash(value.as_ref())) },
        "held": veles_core::configsync::is_held_key(key),
        "effect": effect.as_str(),
        "note": effect.note(),
        "configsync_auto": cfg.cluster.configsync_auto,
    }))
}

fn config_set_local(params: &Value, from_peer: bool) -> Result<Value, String> {
    let key = params["key"].as_str().unwrap_or("").trim().to_string();
    key_verdict(&key, true)?;
    // `configsync_auto = false` is this node's owner deciding every
    // config change another node sends (a push waits in the inbox): a
    // write from another node's command center is one, so it is not
    // taken here either. Unreadable config fails closed.
    if from_peer
        && !veles_core::config::load()
            .map(|c| c.cluster.configsync_auto)
            .unwrap_or(false)
    {
        return Err(format!(
            "this node's owner decides every config change another node sends ([cluster] \
             configsync_auto = false: a config push waits in its inbox), so `{key}` is not \
             written from the command center; push config and its owner applies it there \
             (/configsync apply)"
        ));
    }
    fresh(params)?;
    // The new value: JSON, or `raw` text read as a TOML literal (a
    // string when it is not one), as the settings page sends it.
    let new_toml = match (
        params.get("raw").and_then(Value::as_str),
        params.get("value"),
    ) {
        (Some(raw), _) => Some(veles_core::configedit::literal_or_string(raw)),
        (None, None | Some(Value::Null)) => None,
        (None, Some(v)) => {
            Some(veles_core::configedit::json_to_toml(v).map_err(|e| format!("{e:#}"))?)
        }
    };
    let new_json: Option<Value> = new_toml.as_ref().map(|t| {
        let mut doc = toml_edit::DocumentMut::new();
        let _ = veles_core::configedit::set_value(&mut doc, &["v"], t.clone());
        veles_core::configedit::get_json(&doc, &["v"]).unwrap_or(Value::Null)
    });
    if carries_secret(&key, new_json.as_ref()) {
        return Err(format!(
            "`{key}` would carry a credential: secrets stay on their node and are never \
             written from another one"
        ));
    }
    let doc = veles_core::configedit::read_doc().map_err(|e| format!("{e:#}"))?;
    let segs: Vec<String> = key.split('.').map(str::to_string).collect();
    let refs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let current = veles_core::configedit::get_json(&doc, &refs);
    if carries_secret(&key, current.as_ref()) {
        return Err(format!(
            "`{key}` holds a credential here: it is changed on this node, not from another one"
        ));
    }
    let found = value_hash(current.as_ref());
    if params["expect"].as_str() != Some(found.as_str()) {
        return Err(format!(
            "`{key}` is not what this change was decided on (it changed since it was read, \
             or the request is stale): read it again and decide on its value now"
        ));
    }
    veles_core::configedit::edit(&key, |doc| match new_toml {
        None => veles_core::configedit::unset(doc, &refs),
        Some(t) => veles_core::configedit::set_value(doc, &refs, t),
    })
    .map_err(|e| format!("{e:#}"))?;
    let effect = veles_core::configedit::effect_for(&key);
    let cfg = veles_core::config::load().unwrap_or_default();
    let spreads = cfg.cluster.configsync_auto && !cfg.cluster.memsync_peers.is_empty();
    Ok(json!({
        "key": key,
        "set": new_json.is_some(),
        "value": new_json.unwrap_or(Value::Null),
        "hash": value_hash(veles_core::configedit::read_doc().ok().and_then(|d| veles_core::configedit::get_json(&d, &refs)).as_ref()),
        "effect": effect.as_str(),
        "note": effect.note(),
        "spreads": spreads,
        "result": if spreads {
            format!("`{key}` written in its config.toml; config sync carries it to the nodes that sync config with it")
        } else {
            format!("`{key}` written in its config.toml")
        },
    }))
}

// ── asking a node ────────────────────────────────────────────────────

/// A fresh change-request envelope: `request_id` and `at`.
fn envelope(mut params: Value, who: &str) -> Value {
    params["request_id"] = json!(uuid::Uuid::new_v4().simple().to_string());
    params["at"] = json!(chrono::Utc::now().timestamp());
    params["asked_by"] = json!(who);
    params
}

/// Is this node itself the one asked for?
fn is_self(node: &NodeRow) -> bool {
    crate::fleet_obs::me().as_deref() == Some(node.id.as_str())
}

/// Ask registry node `requested` (id or name) for `params` — answered
/// here for this node itself, else over the node's configured A2A link.
pub async fn ask(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
    params: Value,
) -> Result<(NodeRow, Value), OperationError> {
    let node = crate::fleet_ops::find_node(store, requested)?;
    if is_self(&node) {
        let s = store.clone();
        let mut v = crate::fleet_obs::blocking(move || {
            answer_local(&s, &params, &veles_core::supervisor::detect(), false)
        })
        .await?
        .map_err(OperationError::Refused)?;
        v["via"] = json!("local");
        return Ok((node, v));
    }
    let (peer, url, bearer) = crate::fleet_obs::dial_target(cfg, &node).ok_or_else(|| {
        OperationError::NotConfigured(format!(
            "no [a2a_agents] entry names {}; its registry endpoint is not dialed",
            node.name
        ))
    })?;
    let client = veles_core::a2a::A2AClient::with_timeout(ASK_TIMEOUT);
    match client.rpc(&url, bearer.as_deref(), METHOD, params).await {
        Ok(mut v) => {
            v["via"] = json!(peer);
            Ok((node, v))
        }
        Err(e) => Err(rpc_error(&node, &format!("{e:#}"))),
    }
}

fn rpc_error(node: &NodeRow, text: &str) -> OperationError {
    if text.contains("-32601") {
        OperationError::Refused(format!(
            "{} cannot be managed from the command center: it runs veles {}, and restart \
             and config over the cluster link need 0.2.2 or later",
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

fn refuse_in_lockdown(store: &Arc<Mutex<Store>>) -> Result<(), OperationError> {
    if veles_core::lockdown::current(Some(store)).is_some() {
        return Err(OperationError::Refused(
            "this gateway is in lockdown: it restarts and reconfigures no node until the \
             lockdown is released"
                .into(),
        ));
    }
    Ok(())
}

/// A node's boot, supervisor and running work.
pub async fn status(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
) -> Result<Value, OperationError> {
    let (node, mut v) = ask(store, cfg, requested, json!({"op": "status"})).await?;
    v["node"] = json!(node.id);
    v["name"] = json!(node.name);
    Ok(v)
}

/// Restart a node's gateway through whatever supervises it. Asks its
/// status first: a node that knows no supervisor, or runs turns (unless
/// `force`), is refused before anything is sent. With `wait`, polls
/// until the node answers with a new boot or [`BACK_TIMEOUT`] passes
/// (never for this node itself: the answer would come from the process
/// that is stopping).
pub async fn restart(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
    force: bool,
    confirmed: bool,
    wait: bool,
    who: &str,
) -> Result<Value, OperationError> {
    refuse_in_lockdown(store)?;
    let before = status(store, cfg, requested).await?;
    let name = before["name"].as_str().unwrap_or(requested).to_string();
    if before["supervisor"]["can_restart"] != json!(true) {
        return Err(OperationError::Refused(format!(
            "{name} cannot restart itself: {}",
            before["supervisor"]["detail"]
                .as_str()
                .unwrap_or("no supervisor known")
        )));
    }
    let turns = before["in_flight"]["turns_total"].as_i64().unwrap_or(0);
    if turns > 0 && !force {
        return Err(OperationError::Refused(format!(
            "{name} is running {turns} turn(s); a restart cuts them off (desktop turns resume \
             from their saved history, connector, A2A, mail, cron and subagent turns do not). \
             Restart with force to do it anyway"
        )));
    }
    if !confirmed {
        return Err(OperationError::NeedsConfirmation(format!(
            "restarting {name} stops its gateway ({}); confirm it",
            before["supervisor"]["detail"].as_str().unwrap_or("")
        )));
    }
    let boot = before["boot"].as_str().unwrap_or("").to_string();
    let params = envelope(
        json!({"op": "restart", "expect_boot": boot, "force": force}),
        who,
    );
    let (node, mut v) = ask(store, cfg, requested, params).await?;
    v["node"] = json!(node.id);
    v["name"] = json!(node.name);
    v["boot_before"] = json!(boot);
    if wait && !is_self(&node) {
        let back = wait_back(store, cfg, &node.id, &boot, BACK_TIMEOUT).await;
        v["back"] = back["back"].clone();
        v["back_after_secs"] = back["after_secs"].clone();
        v["result"] = back["result"].clone();
    }
    Ok(v)
}

/// Poll a node's status until it answers with a boot other than
/// `before`, or `timeout` passes.
pub async fn wait_back(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    node: &str,
    before: &str,
    timeout: Duration,
) -> Value {
    let started = Instant::now();
    while started.elapsed() < timeout {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Ok(s) = status(store, cfg, node).await {
            if s["boot"].as_str().is_some_and(|b| b != before) {
                let secs = started.elapsed().as_secs();
                return json!({"back": true, "after_secs": secs, "boot": s["boot"],
                              "result": format!("back after {secs}s (veles {})", s["version"].as_str().unwrap_or("?"))});
            }
        }
    }
    json!({"back": false, "after_secs": timeout.as_secs(),
           "result": format!("not back after {}s: look at the node's log where it runs", timeout.as_secs())})
}

/// Read one config key on a node.
pub async fn config_get(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
    key: &str,
) -> Result<Value, OperationError> {
    key_verdict(key, false).map_err(OperationError::InvalidOperation)?;
    let (node, mut v) = ask(
        store,
        cfg,
        requested,
        json!({"op": "config_get", "key": key}),
    )
    .await?;
    v["node"] = json!(node.id);
    v["name"] = json!(node.name);
    Ok(v)
}

/// Write one config key on a node (`raw` text, read as a TOML literal;
/// `None` unsets it). `expect` is the hash [`config_get`] answered;
/// without one the key is read first.
#[allow(clippy::too_many_arguments)]
pub async fn config_set(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    requested: &str,
    key: &str,
    raw: Option<&str>,
    expect: Option<&str>,
    confirmed: bool,
    who: &str,
) -> Result<Value, OperationError> {
    refuse_in_lockdown(store)?;
    key_verdict(key, true).map_err(OperationError::InvalidOperation)?;
    let expect = match expect {
        Some(e) => e.to_string(),
        None => {
            let read = config_get(store, cfg, requested, key).await?;
            read["hash"].as_str().unwrap_or("").to_string()
        }
    };
    if !confirmed {
        return Err(OperationError::NeedsConfirmation(format!(
            "writing `{key}` on {requested} changes that node's config; confirm it"
        )));
    }
    let mut params = json!({"op": "config_set", "key": key, "expect": expect});
    match raw {
        Some(r) => params["raw"] = json!(r),
        None => params["value"] = Value::Null,
    }
    let (node, mut v) = ask(store, cfg, requested, envelope(params, who)).await?;
    v["node"] = json!(node.id);
    v["name"] = json!(node.name);
    Ok(v)
}

// ── bulk ─────────────────────────────────────────────────────────────

/// The nodes a bulk `action` (`probe`, or `sync` of `kind`) reaches:
/// probe, every other registry node with an `[a2a_agents]` entry; sync,
/// every node that is a configured peer for the kind.
pub fn bulk_targets(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    action: &str,
    kind: &str,
) -> Result<Vec<NodeRow>, OperationError> {
    let nodes = store
        .lock_ok()
        .nodes_list()
        .map_err(OperationError::Failed)?;
    let me = crate::fleet_obs::me();
    let targets: Vec<NodeRow> = match action {
        "probe" => nodes
            .into_iter()
            .filter(|n| me.as_deref() != Some(n.id.as_str()))
            .filter(|n| crate::fleet_obs::dial_target(cfg, n).is_some())
            .collect(),
        "sync" if crate::fleet_ops::valid_kind(kind) => nodes
            .into_iter()
            .filter(|n| me.as_deref() != Some(n.id.as_str()))
            .filter(|n| crate::fleet_ops::peer_for(cfg, kind, n).is_some())
            .collect(),
        "sync" => {
            return Err(OperationError::InvalidOperation(format!(
                "`{kind}` is not a sync kind (store, memory, config, vault)"
            )))
        }
        other => {
            return Err(OperationError::InvalidOperation(format!(
                "a bulk action is probe or sync, not {other:?}"
            )))
        }
    };
    Ok(targets)
}

/// Run a bulk action on every target, once confirmed. Unconfirmed, the
/// answer is the target list the confirmation shows (nothing is run).
pub async fn bulk(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    action: &str,
    kind: &str,
    confirmed: bool,
) -> Result<Value, OperationError> {
    let targets = bulk_targets(store, cfg, action, kind)?;
    let listed: Vec<Value> = targets
        .iter()
        .map(|n| json!({"id": n.id, "name": n.name}))
        .collect();
    if targets.is_empty() {
        return Err(OperationError::NotConfigured(if action == "probe" {
            "no other registry node has an [a2a_agents] entry to probe".into()
        } else {
            format!("no registry node is a configured {kind} sync peer")
        }));
    }
    if !confirmed {
        return Ok(
            json!({"action": action, "kind": (action == "sync").then_some(kind),
                   "confirm_required": true, "targets": listed}),
        );
    }
    let mut results = Vec::new();
    if action == "probe" {
        let asks = targets.iter().map(|n| {
            let (store, cfg, id) = (store.clone(), cfg.clone(), n.id.clone());
            async move { crate::fleet_ops::probe(&store, &cfg, &id).await }
        });
        for (n, r) in targets.iter().zip(futures::future::join_all(asks).await) {
            results.push(match r {
                Ok(v) => json!({"id": n.id, "name": n.name, "ok": v["reachable"] == json!(true),
                                "result": summary_of(&v)}),
                Err(e) => json!({"id": n.id, "name": n.name, "ok": false, "result": e.to_string()}),
            });
        }
    } else {
        // One at a time: a push of the same kind to two peers shares
        // this node's cursor bookkeeping and backoff.
        for n in &targets {
            let r = crate::fleet_ops::operate(store, cfg, kind, &n.id, "push", true).await;
            results.push(match r {
                Ok(v) => json!({"id": n.id, "name": n.name, "ok": true, "result": summary_of(&v)}),
                Err(e) => json!({"id": n.id, "name": n.name, "ok": false, "result": e.to_string()}),
            });
        }
    }
    let failed = results.iter().filter(|r| r["ok"] != json!(true)).count();
    Ok(json!({
        "action": action,
        "kind": (action == "sync").then_some(kind),
        "targets": listed,
        "results": results,
        "failed": failed,
        "result": format!("{} of {} target(s) ok", results.len() - failed, results.len()),
    }))
}

/// A bulk answer as chat text.
pub fn bulk_text(v: &Value) -> String {
    let what = if v["action"] == json!("sync") {
        format!("sync {}", v["kind"].as_str().unwrap_or(""))
    } else {
        "probe".into()
    };
    let mut out = String::new();
    if v["confirm_required"] == json!(true) {
        out.push_str(&format!("Bulk {what} would reach:\n"));
        for t in v["targets"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "- {} ({})\n",
                t["name"].as_str().unwrap_or("?"),
                t["id"].as_str().unwrap_or("?")
            ));
        }
        out.push_str("Repeat the command with a trailing `confirm` to run it.\n");
        return out;
    }
    out.push_str(&format!(
        "Bulk {what}: {}\n",
        v["result"].as_str().unwrap_or("")
    ));
    for r in v["results"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "- {}: {} {}\n",
            r["name"].as_str().unwrap_or("?"),
            if r["ok"] == json!(true) {
                "ok"
            } else {
                "FAILED"
            },
            r["result"].as_str().unwrap_or("")
        ));
    }
    out
}

/// A config read or write as chat text.
pub fn config_text(v: &Value) -> String {
    let key = v["key"].as_str().unwrap_or("?");
    let node = v["name"].as_str().unwrap_or("?");
    if v["op"] == json!("config_set") {
        return format!(
            "{node}: {} ({}).\n",
            v["result"].as_str().unwrap_or(""),
            v["note"].as_str().unwrap_or("")
        );
    }
    let value = if v["withheld"] == json!(true) {
        "(withheld: a credential stays on its node)".to_string()
    } else if v["set"] == json!(true) {
        v["value"].to_string()
    } else {
        "(not set: the default applies)".to_string()
    };
    format!(
        "{node}: {key} = {value}\n{}{}",
        if v["held"] == json!(true) {
            "A held section: a change waits for that node's owner.\n"
        } else {
            ""
        },
        v["note"]
            .as_str()
            .map(|n| format!("{n}\n"))
            .unwrap_or_default()
    )
}

/// A restart answer as chat text.
pub fn restart_text(v: &Value) -> String {
    let name = v["name"].as_str().unwrap_or("?");
    match v["back"].as_bool() {
        Some(true) => format!(
            "{name}: restarted; {}\n",
            v["result"].as_str().unwrap_or("")
        ),
        Some(false) => format!(
            "{name}: restart sent; {}\n",
            v["result"].as_str().unwrap_or("")
        ),
        None => format!("{name}: {}\n", v["result"].as_str().unwrap_or("restarting")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(op: &str) -> Value {
        json!({"op": op, "request_id": uuid::Uuid::new_v4().simple().to_string(),
               "at": chrono::Utc::now().timestamp()})
    }

    /// A replayed or stale change request is refused, and so is one
    /// with no id or time; the id is remembered.
    #[test]
    fn a_change_request_must_be_fresh_and_is_taken_once() {
        let r = req("restart");
        assert!(fresh(&r).is_ok());
        let again = fresh(&r).unwrap_err();
        assert!(again.contains("replay"), "{again}");
        let mut old = req("restart");
        old["at"] = json!(chrono::Utc::now().timestamp() - WINDOW_SECS - 5);
        assert!(fresh(&old).unwrap_err().contains("stale"));
        let mut ahead = req("restart");
        ahead["at"] = json!(chrono::Utc::now().timestamp() + WINDOW_SECS + 5);
        assert!(fresh(&ahead).is_err());
        assert!(fresh(&json!({"op": "restart", "at": 1})).is_err());
        let mut no_at = req("restart");
        no_at.as_object_mut().unwrap().remove("at");
        assert!(fresh(&no_at).is_err());
    }

    /// The keys config sync carries and nothing else; a held section is
    /// not written from a peer; a credential neither way.
    #[test]
    fn config_keys_follow_config_sync_rules() {
        assert!(key_verdict("model.default", true).is_ok());
        for local in [
            "cluster.memsync_peers",
            "node.name",
            "a2a_agents.vps.url",
            "tui.theme",
        ] {
            let e = key_verdict(local, false).unwrap_err();
            assert!(e.contains("node-local"), "{local}: {e}");
        }
        for held in [
            "acl.entries",
            "approvals.mode",
            "sandbox.network",
            "plugins.enabled",
        ] {
            assert!(key_verdict(held, false).is_ok(), "{held} may be read");
            let e = key_verdict(held, true).unwrap_err();
            assert!(e.contains("held section"), "{held}: {e}");
        }
        // A table that holds a node-local key is node-local too: written
        // whole from a peer, `platforms.a2a.extra = {host = "0.0.0.0"}`
        // would rebind that node's A2A server to every interface, and
        // `web = {…}` would choose where its others' traffic leaves.
        for parent in [
            "platforms.a2a.extra",
            "platforms.a2a",
            "platforms.desktop",
            "platforms",
            "web",
            "sandbox",
        ] {
            for writing in [false, true] {
                let e = key_verdict(parent, writing).unwrap_err();
                assert!(e.contains("node-local"), "{parent} ({writing}): {e}");
            }
        }
        assert!(key_verdict("platforms.irc.server", true).is_ok());
        assert!(key_verdict("platforms.a2a.extra.port", true).is_ok());
        assert!(key_verdict("web.searxng_timeout_secs", true).is_ok());
        assert!(key_verdict("model..x", false).is_err());
        assert!(carries_secret("platforms.telegram.token", None));
        assert!(carries_secret(
            "model.default",
            Some(&json!("sk-ant-abcdefghijklmnopqrstu"))
        ));
        assert!(carries_secret(
            "platforms",
            Some(&json!({"irc": {"sasl_password": "x"}}))
        ));
        assert!(!carries_secret(
            "model.default",
            Some(&json!("deepseek-v4"))
        ));
        assert_ne!(value_hash(None), value_hash(Some(&json!(""))));
        assert_eq!(value_hash(Some(&json!(5))), value_hash(Some(&json!(5))));
    }

    /// `configsync_auto = false` holds what other nodes send for this
    /// node's owner: a peer's write is refused, while this gateway's own
    /// command center still writes its own config.
    #[test]
    fn a_node_whose_owner_decides_config_takes_no_write_from_a_peer() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let path = veles_core::config::config_dir().join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[cluster]\nconfigsync_auto = false\n\n[model]\ndefault = \"alpha\"\n",
        )
        .unwrap();
        let hash = config_get_local("model.default").unwrap()["hash"].clone();
        let write = |from_peer: bool| {
            let mut w = req("config_set");
            w["key"] = json!("model.default");
            w["raw"] = json!("beta");
            w["expect"] = hash.clone();
            config_set_local(&w, from_peer)
        };
        let e = write(true).unwrap_err();
        assert!(e.contains("configsync_auto"), "{e}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("beta"));
        write(false).expect("this gateway's own write");
        assert!(std::fs::read_to_string(&path).unwrap().contains("beta"));
    }

    /// Every action written to the trail is read back newest first, with
    /// who asked, what, which node and the result, a credential-shaped
    /// result redacted.
    #[test]
    fn the_audit_trail_keeps_who_what_which_node_and_the_result() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        audit("desktop", "probe", "node-a", true, "reachable in 4 ms");
        audit(
            "telegram:1",
            "restart",
            "node-b",
            false,
            "refused: sk-ant-abcdefghijklmnopqrstuvwxyz",
        );
        let rows = audit_recent(10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["action"], "restart");
        assert_eq!(rows[0]["who"], "telegram:1");
        assert_eq!(rows[0]["node"], "node-b");
        assert_eq!(rows[0]["ok"], false);
        assert!(!rows[0]["result"]
            .as_str()
            .unwrap()
            .contains("abcdefghijklmnop"));
        assert_eq!(rows[1]["action"], "probe");
        assert!(audit_text(&rows).contains("node-a"));
        let r = audited(
            "desktop",
            "forget",
            "node-c",
            Err(OperationError::Refused("still heartbeating".into())),
        );
        assert!(r.is_err());
        assert_eq!(audit_recent(1)[0]["action"], "forget");
    }
}
