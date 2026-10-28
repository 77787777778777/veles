//! `veles recover` — a node whose home was lost rebuilds itself from one
//! pool peer, with one command (PLAN-recover.md).
//!
//! On 2026-10-19 the laptop's home was deleted. Its sessions, memory and
//! config all still existed — in the VPS's replica of its store, in the
//! peers' memory, in the config log every peer keeps — and it took a
//! person and another model two days of `sqlite3`, `jq` and `ssh` to put
//! them back by hand (~/RESCUE/RECOVERY-ROUND2.md). This module is that
//! by-hand recovery, as code, with the traps it hit closed:
//!
//! 1. **Identity** ([`settle_identity`]). The node takes back the id it
//!    had — the one the arbiter's registry and every peer's bindings know
//!    it by — when the invite names it and `--as` confirms it; otherwise
//!    it is a new node and is told so.
//! 2. **Links** (`pool::join` with a recovery invite, `pool::mint_recovery`).
//!    The redeem rotates the arbiter's side and, by introduction, every
//!    member's: the hand-rotated tokens of the by-hand recovery, minted.
//! 3. **Sessions** ([`fold_own_replica`]). A consistent snapshot of the
//!    peer's replica of THIS node is pulled in parts, the peer's unapplied
//!    store offers from this node are folded onto it, and its rows go back
//!    into the primary store under their own ids — the one legitimate case
//!    of a replica folding into a primary, because the node folding it is
//!    its owner ("a replica is the owner's image"). The store log then
//!    continues past the highest replica cursor any member reports, so the
//!    next push lands instead of being skipped as already applied (the
//!    `rebuild-replica.sh` bug).
//! 4. **Memory**: the peer's whole (scoped) memory, plus the offers from
//!    this node still waiting in its inbox.
//! 5. **Config** (`configsync::import_recovered`): the whole config log the
//!    peer holds, imported BEFORE any scan so this node's next write
//!    numbers after its own history, then merged into config.toml.
//! 6. **Skills and commands**: no sync carries them (they are per node),
//!    so a recovery copies the peer's — files absent here only, never an
//!    overwrite, while the recovery window is open.
//! 7. **Vault**: new keys and a join request on the peer's inbox. Admission
//!    stays a member's own step; the command prints exactly what to run.
//!
//! The peer's side is [`serve`]: one A2A marker, answered inline (no
//! session, no turn), for a peer it can bind to a node — and never with
//! another node's image.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::config::VelesConfig;
use crate::lockutil::LockOk;
use crate::store::Store;

/// The first line of a recovery ask.
pub const ASK_MARKER: &str = "[veles recover ask]";
/// The first line of every answer to one.
pub const REPLY_MARKER: &str = "[veles recover reply]";
/// Bytes of a replica snapshot per part. A part travels base64'd twice
/// (in its JSON, then in the fence): 4 MiB is ~7.5 MB on the wire.
pub const PART_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of file content per `files` answer, and the largest file one
/// carries (a larger one is named as skipped).
const FILES_BUDGET: usize = 4 * 1024 * 1024;
/// How long a snapshot waits in staging for its parts to be pulled.
const SNAPSHOT_TTL_SECS: u64 = 2 * 60 * 60;
/// The roots a recovery copies files from, under the config dir.
pub const FILE_ROOTS: &[&str] = &["skills", "commands"];
/// The steps `--skip` names.
pub const STEPS: &[&str] = &["store", "memory", "config", "files", "vault"];

// ── the wire ─────────────────────────────────────────────────────────

/// One recovery ask. `node` is the node the asker says it is: the peer
/// answers only when that is the node it binds the asker to.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Ask {
    pub op: String,
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub index: u64,
    #[serde(default)]
    pub root: String,
}

pub fn is_ask(raw: &str) -> bool {
    raw.trim_start().starts_with(ASK_MARKER)
}

pub fn encode_ask(ask: &Ask) -> Result<String> {
    Ok(crate::mailnet::fence(
        ASK_MARKER,
        &serde_json::to_string(ask)?,
    ))
}

pub fn decode_ask(raw: &str) -> Result<Ask> {
    serde_json::from_str(&crate::mailnet::unfence(raw)?).context("not a recovery ask")
}

pub fn encode_reply(v: &Value) -> Result<String> {
    Ok(crate::mailnet::fence(
        REPLY_MARKER,
        &serde_json::to_string(v)?,
    ))
}

pub fn decode_reply(raw: &str) -> Result<Value> {
    if !raw.trim_start().starts_with(REPLY_MARKER) {
        bail!("{}", raw.chars().take(300).collect::<String>());
    }
    serde_json::from_str(&crate::mailnet::unfence(raw)?).context("not a recovery reply")
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("not base64")
}

fn sha_file(path: &Path) -> Result<(u64, String)> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
    Ok((
        size,
        h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    ))
}

fn random_hex(bytes: usize) -> Result<String> {
    use ring::rand::SecureRandom as _;
    let mut b = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── the peer's side ──────────────────────────────────────────────────

/// Which node the authenticated A2A peer `peer` is, here, and how this
/// node knows: a recovery that re-linked it (the arbiter's ledger, or the
/// introduction's mark on a member), else the node that redeemed its
/// invite here (a new node right after its join), else its gossip, its
/// store offers, or the registry row of that name (`pool::member_node`).
pub fn peer_node(store: &Store, peer: &str) -> Option<(String, &'static str)> {
    if let Some((id, _)) = crate::pool::recovery_for(store, peer) {
        return Some((id, "recovery"));
    }
    if let Some(id) = crate::pool::invited_node(peer) {
        return Some((id, "invite"));
    }
    let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|n| n.id)
        .unwrap_or_default();
    crate::pool::member_node(store, &me, peer, None)
        .ok()
        .map(|(id, _, via)| (id, via))
}

fn refused(e: impl std::fmt::Display) -> Value {
    json!({"status": "refused", "error": e.to_string()})
}

/// Answer one recovery ask from the authenticated peer `peer`. Never an
/// error: a refusal is an answer, and says why.
///
/// What each op may give, and to whom:
/// - everything needs a peer this node can bind to a node, and an ask
///   that names that node: a node recovers only its own image;
/// - the replica and its pending store offers: a peer in `[cluster]
///   storesync_peers` — the list it shipped that replica through;
/// - memory and config: a peer in `[cluster] memsync_peers` — what it
///   would get by push anyway, now all at once;
/// - skills and commands, which no sync carries: the same, and only
///   while a recovery window is open for that peer (a recovery invite
///   redeemed here, or an introduction that said so).
pub fn serve(store: &std::sync::Mutex<Store>, cfg: &VelesConfig, peer: &str, ask: &Ask) -> Value {
    match serve_inner(store, cfg, peer, ask) {
        Ok(v) => v,
        Err(e) => refused(format!("{e:#}")),
    }
}

fn serve_inner(
    store: &std::sync::Mutex<Store>,
    cfg: &VelesConfig,
    peer: &str,
    ask: &Ask,
) -> Result<Value> {
    let (bound, window) = {
        let s = store.lock_ok();
        (
            peer_node(&s, peer),
            crate::pool::recovery_for(&s, peer).map(|(_, until)| until),
        )
    };
    let Some((node, via)) = bound else {
        bail!(
            "this node does not know which node '{peer}' is (no recovery, gossip, store offer \
             or registry row names it): nothing is served"
        );
    };
    if ask.node != node {
        bail!(
            "'{peer}' is node {node} here ({via}), and the ask is for {:?}: a node recovers only \
             its own image",
            ask.node
        );
    }
    let listed = |list: &[String], what: &str| -> Result<()> {
        if list.iter().any(|p| p == peer) {
            Ok(())
        } else {
            bail!("'{peer}' is not in this node's [cluster] {what}: nothing of that kind is served")
        }
    };
    let window_open = window.is_some_and(|until| until > now_secs());
    match ask.op.as_str() {
        "inventory" => inventory(store, cfg, peer, &node, via, window_open),
        "replica_snapshot" => {
            listed(&cfg.cluster.storesync_peers, "storesync_peers")?;
            snapshot(&node)
        }
        "replica_part" => {
            listed(&cfg.cluster.storesync_peers, "storesync_peers")?;
            part(&node, &ask.token, ask.index)
        }
        "replica_done" => {
            let _ = std::fs::remove_file(staged(&node, &ask.token)?);
            Ok(json!({"status": "ok"}))
        }
        "store_inbox" => {
            listed(&cfg.cluster.storesync_peers, "storesync_peers")?;
            let offers = store_offers_from(&node, ask.index == 0)?;
            let Some(p) = offers.get(ask.index as usize) else {
                bail!("no store offer {} from {node} here", ask.index);
            };
            let entry = crate::storesync::inbox_read(p)?;
            Ok(json!({"status": "ok", "count": offers.len(), "offer": entry.offer}))
        }
        "memory" => {
            listed(&cfg.cluster.memsync_peers, "memsync_peers")?;
            memory(cfg, ask.index)
        }
        "memory_inbox" => {
            listed(&cfg.cluster.memsync_peers, "memsync_peers")?;
            let offers = memory_offers_from(&node)?;
            let Some(o) = offers.get(ask.index as usize) else {
                bail!("no memory offer {} from {node} here", ask.index);
            };
            Ok(json!({"status": "ok", "count": offers.len(), "offer": o}))
        }
        "config" => {
            listed(&cfg.cluster.memsync_peers, "memsync_peers")?;
            config_rows(store, &node)
        }
        "files" => {
            listed(&cfg.cluster.memsync_peers, "memsync_peers")?;
            if !window_open {
                bail!(
                    "skills and commands are served only during a recovery: no recovery window \
                     is open for '{peer}' here (a recovery invite redeemed on the arbiter opens \
                     one for a day)"
                );
            }
            files(&ask.root, ask.index)
        }
        other => bail!("no recovery op {other:?}"),
    }
}

fn replica_summary(node: &str) -> Option<Value> {
    let path = crate::storesync::replica_path(node);
    if !path.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let count = |t: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))
            .unwrap_or(0)
    };
    let applied: i64 = crate::storesync::meta_get(&conn, crate::storesync::APPLIED_SEQ_META_KEY)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Some(json!({
        "applied_seq": applied,
        "sessions": count("sessions"),
        "messages": count("messages"),
        "bytes": std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
    }))
}

fn inventory(
    store: &std::sync::Mutex<Store>,
    cfg: &VelesConfig,
    peer: &str,
    node: &str,
    via: &str,
    window_open: bool,
) -> Result<Value> {
    let me = crate::node::NodeIdentity::load(&crate::config::config_dir())?.unwrap_or(
        crate::node::NodeIdentity {
            id: String::new(),
            name: String::new(),
        },
    );
    let storesync = cfg.cluster.storesync_peers.iter().any(|p| p == peer);
    let memsync = cfg.cluster.memsync_peers.iter().any(|p| p == peer);
    let count_files = |root: &str| {
        let mut v = Vec::new();
        walk(
            &crate::config::config_dir().join(root),
            Path::new(""),
            &mut v,
        );
        v.len()
    };
    let plugins: Vec<String> = std::fs::read_dir(crate::plugins::plugins_dir())
        .map(|rd| {
            let mut v: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect();
            v.sort();
            v
        })
        .unwrap_or_default();
    let config_rows = store
        .lock_ok()
        .config_log_rows()
        .map(|r| r.len())
        .unwrap_or(0);
    Ok(json!({
        "status": "ok",
        "node": node,
        "via": via,
        "server": {"id": me.id, "name": me.name},
        "replica": if storesync { replica_summary(node) } else { None },
        "store_inbox": if storesync { store_offers_from(node, true)?.len() } else { 0 },
        "memory_inbox": if memsync { memory_offers_from(node)?.len() } else { 0 },
        "config_rows": if memsync { config_rows } else { 0 },
        "storesync": storesync,
        "memsync": memsync,
        "window": window_open,
        "skills": if window_open { count_files("skills") } else { 0 },
        "commands": if window_open { count_files("commands") } else { 0 },
        "plugins": plugins,
        "vault": crate::vault::vault_dir().join("vault.json").is_file(),
    }))
}

fn staging() -> PathBuf {
    crate::config::data_dir().join("recover").join("out")
}

/// The staged snapshot `token` names, for `node` only: the file name
/// carries both, and a token is 32 hex characters or nothing.
fn staged(node: &str, token: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit()),
        "no such snapshot here"
    );
    anyhow::ensure!(
        crate::storesync::valid_node_id(node),
        "{node:?} is not a node id"
    );
    Ok(staging().join(format!("{node}-{token}.db")))
}

/// A consistent copy of this node's replica of `node` (`VACUUM INTO`, a
/// read transaction: the gateway may go on applying offers into the
/// replica meanwhile), staged for its parts to be pulled.
fn snapshot(node: &str) -> Result<Value> {
    let src = crate::storesync::replica_path(node);
    anyhow::ensure!(src.is_file(), "this node holds no replica of {node}");
    let dir = staging();
    std::fs::create_dir_all(&dir)?;
    // Old snapshots a pull never finished: gone after the TTL.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.filter_map(|e| e.ok()) {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() > SNAPSHOT_TTL_SECS);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let token = random_hex(16)?;
    let out = staged(node, &token)?;
    {
        let conn =
            rusqlite::Connection::open(&src).with_context(|| format!("open {}", src.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute(
            "VACUUM INTO ?1",
            rusqlite::params![out.to_string_lossy().into_owned()],
        )
        .with_context(|| format!("snapshot {}", src.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600));
    }
    let (size, sha256) = sha_file(&out)?;
    let summary = {
        let conn = rusqlite::Connection::open_with_flags(
            &out,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        crate::storesync::meta_get(&conn, crate::storesync::APPLIED_SEQ_META_KEY)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
    };
    Ok(json!({
        "status": "ok",
        "token": token,
        "size": size,
        "sha256": sha256,
        "part_bytes": PART_BYTES,
        "parts": (size as usize).div_ceil(PART_BYTES),
        "applied_seq": summary,
    }))
}

fn part(node: &str, token: &str, index: u64) -> Result<Value> {
    use std::io::{Read as _, Seek as _};
    let path = staged(node, token)?;
    let mut f = std::fs::File::open(&path).context("no such snapshot here (expired?)")?;
    let size = f.metadata()?.len() as usize;
    let start = (index as usize)
        .checked_mul(PART_BYTES)
        .filter(|s| *s < size)
        .context("no such part")?;
    let mut buf = vec![0u8; PART_BYTES.min(size - start)];
    f.seek(std::io::SeekFrom::Start(start as u64))?;
    f.read_exact(&mut buf)?;
    Ok(json!({"status": "ok", "b64": b64(&buf)}))
}

/// The store offers from `node` waiting in this node's storesync inbox,
/// oldest first. Each is up to 768 KB of JSON, so the list is kept per
/// node between asks and rebuilt on the first (`fresh`).
fn store_offers_from(node: &str, fresh: bool) -> Result<Vec<PathBuf>> {
    static CACHE: std::sync::Mutex<Option<(String, Vec<PathBuf>)>> = std::sync::Mutex::new(None);
    let mut c = CACHE.lock_ok();
    if !fresh {
        if let Some((n, v)) = c.as_ref() {
            if n == node {
                return Ok(v.clone());
            }
        }
    }
    let mut out = Vec::new();
    for p in crate::storesync::inbox_pending(&crate::storesync::inbox_dir())? {
        if let Ok(e) = crate::storesync::inbox_read(&p) {
            if e.offer.from_node == node {
                out.push(p);
            }
        }
    }
    *c = Some((node.to_string(), out.clone()));
    Ok(out)
}

fn memory_offers_from(node: &str) -> Result<Vec<crate::memsync::SyncOffer>> {
    let mut out = Vec::new();
    for p in crate::memsync::inbox_pending(&crate::memsync::inbox_dir())? {
        if let Ok(e) = crate::memsync::inbox_read(&p) {
            if e.offer.from_node == node {
                out.push(e.offer);
            }
        }
    }
    Ok(out)
}

/// This node's memory as one scoped offer (its node-local paths and the
/// credential-shaped ones withheld, as a push would), chunk `index`.
fn memory(cfg: &VelesConfig, index: u64) -> Result<Value> {
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    let ms = crate::memory::MemoryStore::new();
    let (mut offer, mut withheld) = crate::memsync::build_offer_scoped(
        &ms,
        &me.id,
        &me.name,
        0,
        &cfg.cluster.local_memory_globs,
    )?;
    withheld.extend(crate::memsync::withhold_secret_paths(&mut offer));
    let chunks = crate::memsync::chunk_offer(&offer, crate::memsync::CHUNK_BYTES)?;
    let Some(c) = chunks.get(index as usize) else {
        bail!("no memory chunk {index}");
    };
    Ok(json!({
        "status": "ok",
        "parts": chunks.len(),
        "offer": c,
        "withheld": if index == 0 { withheld } else { Vec::new() },
    }))
}

/// The whole config log this node holds — every node's rows, the
/// asker's own among them — plus the asker's rows still waiting in this
/// node's configsync inbox. Local keys and credential-shaped values are
/// left out, as an offer leaves them out.
fn config_rows(store: &std::sync::Mutex<Store>, node: &str) -> Result<Value> {
    let rows = store.lock_ok().config_log_rows()?;
    let mut out = Vec::new();
    let mut withheld = std::collections::BTreeSet::new();
    let mut take = |r: crate::configsync::ConfigLogRow| {
        if !crate::configsync::is_replicated_key(&r.key) {
            return;
        }
        if !r.value.is_empty() && crate::a2a::secret_shaped(&r.value) {
            withheld.insert(r.key.clone());
            return;
        }
        out.push(r);
    };
    for r in rows {
        take(r);
    }
    let mut inbox = 0usize;
    for p in crate::configsync::inbox_pending(&crate::configsync::inbox_dir())? {
        if let Ok(e) = crate::configsync::inbox_read(&p) {
            if e.offer.from_node == node {
                for r in e.offer.rows.into_iter().filter(|r| r.node == node) {
                    inbox += 1;
                    take(r);
                }
            }
        }
    }
    Ok(json!({
        "status": "ok",
        "rows": out,
        "from_inbox": inbox,
        "withheld": withheld.into_iter().collect::<Vec<_>>(),
    }))
}

/// Every regular file under `root`, depth first in name order, as paths
/// relative to it. Symlinks and `.git` are passed over.
fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    let mut ents: Vec<std::fs::DirEntry> = rd.filter_map(|e| e.ok()).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let name = e.file_name();
        if name == ".git" {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let r = rel.join(&name);
        if ft.is_dir() {
            walk(root, &r, out);
        } else if ft.is_file() {
            out.push(r);
        }
    }
}

fn rel_text(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Files under `<config>/<root>` from the `index`-th on, up to the budget.
fn files(root: &str, index: u64) -> Result<Value> {
    anyhow::ensure!(
        FILE_ROOTS.contains(&root),
        "{root:?} is not a root a recovery copies (only {FILE_ROOTS:?})"
    );
    let base = crate::config::config_dir().join(root);
    let mut all = Vec::new();
    walk(&base, Path::new(""), &mut all);
    let mut out = Vec::new();
    let mut withheld = Vec::new();
    let mut skipped = Vec::new();
    let mut bytes = 0usize;
    let mut i = index as usize;
    while i < all.len() {
        let rel = &all[i];
        let path = base.join(rel);
        let len = std::fs::metadata(&path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if len > FILES_BUDGET {
            skipped.push(rel_text(rel));
            i += 1;
            continue;
        }
        if bytes + len > FILES_BUDGET && !out.is_empty() {
            break;
        }
        let Ok(data) = std::fs::read(&path) else {
            skipped.push(rel_text(rel));
            i += 1;
            continue;
        };
        if std::str::from_utf8(&data).is_ok_and(crate::a2a::secret_shaped) {
            withheld.push(rel_text(rel));
            i += 1;
            continue;
        }
        #[cfg(unix)]
        let exec = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&path)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        };
        #[cfg(not(unix))]
        let exec = false;
        bytes += len;
        out.push(json!({"path": rel_text(rel), "b64": b64(&data), "exec": exec}));
        i += 1;
    }
    Ok(json!({
        "status": "ok",
        "files": out,
        "next": if i < all.len() { Some(i) } else { None },
        "total": all.len(),
        "withheld": withheld,
        "skipped": skipped,
    }))
}

// ── the recovering node: identity ────────────────────────────────────

/// Who this node is after [`settle_identity`].
#[derive(Debug, Clone, PartialEq)]
pub enum Identity {
    /// The id the recovery invite names, taken back. `replaced` is where
    /// a different node.toml this home had was set aside.
    Adopted {
        id: String,
        name: String,
        replaced: Option<PathBuf>,
    },
    /// node.toml was already here and stays.
    Kept { id: String, name: String },
    /// No identity here and none to take back: a new node.
    Minted { id: String, name: String },
}

impl Identity {
    pub fn id(&self) -> &str {
        match self {
            Identity::Adopted { id, .. }
            | Identity::Kept { id, .. }
            | Identity::Minted { id, .. } => id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Identity::Adopted { name, .. }
            | Identity::Kept { name, .. }
            | Identity::Minted { name, .. } => name,
        }
    }
    /// The node is the one its pool knew before: its replica is its own.
    pub fn is_old(&self) -> bool {
        !matches!(self, Identity::Minted { .. })
    }
}

fn write_identity(config_dir: &Path, id: &crate::node::NodeIdentity) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let path = config_dir.join("node.toml");
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(id)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

/// Sessions in the store at `db`, and the node its owner stamp names —
/// without creating a store that is not there.
fn store_facts(db: &Path) -> Result<(i64, Option<String>)> {
    if !db.is_file() {
        return Ok((0, None));
    }
    let conn = rusqlite::Connection::open(db)?;
    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);
    Ok((sessions, crate::storesync::owner_of(&conn)))
}

/// Decide who this node is, and write it (node.toml, and the store's
/// owner stamp when it must follow).
///
/// - A recovery invite (`recover`) names the node it re-admits; `--as`
///   (its id, a prefix of 8 or more, or its name) must confirm it, and
///   the node takes that id back. Without `--as` it is refused: a new
///   identity under the old name would contradict every peer's binding
///   of that name.
/// - A home that already runs as ANOTHER node is replaced only while its
///   store holds no sessions: then node.toml is set aside and the stamp
///   rewritten. With sessions, it is refused — move the store aside.
/// - Without a recovery invite, `--as` is refused; node.toml is kept if
///   there is one, else a new identity is minted (and the caller says so).
pub fn settle_identity(
    config_dir: &Path,
    db: &Path,
    recover: Option<&crate::pool::NodeRef>,
    as_node: Option<&str>,
) -> Result<Identity> {
    let have = crate::node::NodeIdentity::load(config_dir)?;
    let Some(r) = recover else {
        if as_node.is_some() {
            bail!(
                "--as takes back the identity a RECOVERY invite names: on the arbiter, `veles \
                 pool invite <name> --recover`"
            );
        }
        return Ok(match have {
            Some(h) => Identity::Kept {
                id: h.id,
                name: h.name,
            },
            None => {
                let m = crate::node::NodeIdentity::load_or_create(config_dir)?;
                Identity::Minted {
                    id: m.id,
                    name: m.name,
                }
            }
        });
    };
    let Some(x) = as_node.map(str::trim).filter(|x| !x.is_empty()) else {
        bail!(
            "this invite re-admits {} ({}): add --as {} to take that identity back. A new node \
             joins with a plain invite (`veles pool invite <new name>`)",
            r.name,
            r.id,
            r.name
        );
    };
    let matches = x == r.id || x == r.name || (x.len() >= 8 && r.id.starts_with(x));
    if !matches {
        bail!(
            "--as {x} is not the node this invite re-admits: {} ({})",
            r.name,
            r.id
        );
    }
    let (sessions, owner) = store_facts(db)?;
    let stranger = |other: &str| -> Result<()> {
        if sessions > 0 {
            bail!(
                "this home already runs as node {other}, and its store ({}) holds {sessions} \
                 session(s): move state.db and node.toml aside, then run the recovery again",
                db.display()
            );
        }
        Ok(())
    };
    let mut replaced = None;
    match &have {
        Some(h) if h.id == r.id => {}
        Some(h) => {
            stranger(&h.id)?;
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
            let aside = config_dir.join(format!("node.toml.replaced-{stamp}"));
            std::fs::rename(config_dir.join("node.toml"), &aside)?;
            replaced = Some(aside);
        }
        None => {}
    }
    if let Some(o) = owner.as_deref().filter(|o| *o != r.id) {
        stranger(o)?;
        let conn = rusqlite::Connection::open(db)?;
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = ?2",
            rusqlite::params![r.id, crate::storesync::OWNER_META_KEY],
        )?;
    }
    let name = match &have {
        Some(h) if h.id == r.id && !h.name.is_empty() => h.name.clone(),
        _ => r.name.clone(),
    };
    if have.as_ref().map(|h| h.id.as_str()) != Some(r.id.as_str()) {
        write_identity(
            config_dir,
            &crate::node::NodeIdentity {
                id: r.id.clone(),
                name: name.clone(),
            },
        )?;
    }
    Ok(Identity::Adopted {
        id: r.id.clone(),
        name,
        replaced,
    })
}

// ── the recovering node: its own replica, folded back ────────────────

/// What [`fold_own_replica`] did.
#[derive(Debug, Default, Clone)]
pub struct FoldReport {
    /// Per table: rows put back, rows this store already had.
    pub tables: Vec<(String, usize, usize)>,
    /// The store log's next row is `log_seq + 1`.
    pub log_seq: i64,
}

impl FoldReport {
    pub fn inserted(&self, table: &str) -> usize {
        self.tables
            .iter()
            .find(|(t, _, _)| t == table)
            .map(|(_, i, _)| *i)
            .unwrap_or(0)
    }
}

/// The columns that tell one row of an AUTOINCREMENT table from another
/// with the same id: a message never moves to another session, nor
/// changes when it was written. Two rows that share the id and differ
/// here are two different rows — an id the fresh store issued again.
fn identity_cols(tbl: &str) -> &'static [&'static str] {
    match tbl {
        "messages" | "usage" | "checkpoints" | "engine_notes" => &["session_id", "created_at"],
        "skill_uses" => &["session_id", "at"],
        "irc_lines" => &["network", "buffer", "ts"],
        _ => &[],
    }
}

fn q(c: &str) -> String {
    format!("\"{}\"", c.replace('"', "\"\""))
}

/// Fold the replica file at `replica` — THIS node's image, as a peer held
/// it — back into this node's primary store.
///
/// The fold ban (`storesync::apply_offer`) says a replica is never folded
/// into a primary. A recovering node is the one exception: the image is
/// its own rows under its own ids, and it is their only writer. So the
/// rules are the owner's:
///
/// - the replica must be the image of `me`, and the store `me`'s (its
///   owner stamp), never a replica itself;
/// - a row this store already has stays — it is the owner's newer image
///   of the same row (a re-run, or a session touched since) — EXCEPT in
///   an AUTOINCREMENT table, where a shared id whose [`identity_cols`]
///   differ is a different row the fresh store numbered again: then
///   nothing is written and the fold is refused, naming the tables;
/// - rows are copied by named columns (the shapes may differ: an ALTERed
///   table appends), messages are indexed once by this store's own FTS
///   trigger, and the store log keeps no row of the fold (the peers have
///   them all);
/// - the store log then continues after `cursor` — the highest replica
///   cursor any peer reported for this node — so a peer's next apply of
///   this node's rows is gapless instead of "already up to date" forever.
///
/// Every table is copied or the whole fold is refused: a skipped table
/// is the by-hand merge's silent failure (RECOVERY-ROUND2: v2 copied
/// nothing and said so in a log line).
pub fn fold_own_replica(
    store: &Store,
    replica: &Path,
    me: &str,
    cursor: i64,
) -> Result<FoldReport> {
    use crate::storesync::{
        is_replica, meta_get, owner_of, table_columns, table_pk_cols, REPLICATED_TABLES,
        REPLICA_OF_META_KEY,
    };
    let rconn =
        rusqlite::Connection::open_with_flags(replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", replica.display()))?;
    match meta_get(&rconn, REPLICA_OF_META_KEY) {
        Some(of) if of == me => {}
        Some(of) => bail!(
            "refused: {} is the image of node {of}, and this node is {me} — a replica folds back \
             only into its owner's store",
            replica.display()
        ),
        None => bail!("refused: {} is not a replica", replica.display()),
    }
    let conn = store.conn();
    anyhow::ensure!(
        !is_replica(conn),
        "refused: this store is itself a replica, not a node's own store"
    );
    if let Some(o) = owner_of(conn).filter(|o| o != me) {
        bail!("refused: this store belongs to node {o}, and the replica is {me}'s");
    }

    // The copy plan, from two separate connections' view of the columns
    // (a schema qualifier on pragma_table_info is ignored over ATTACH).
    struct Plan {
        tbl: &'static str,
        cols: Vec<String>,
        pk: Vec<String>,
        ident: Vec<String>,
        rows: i64,
    }
    let mut plans = Vec::new();
    for tbl in REPLICATED_TABLES {
        let lcols = table_columns(conn, tbl)?;
        anyhow::ensure!(
            !lcols.is_empty(),
            "this store has no {tbl} table — run any veles command once to create its schema"
        );
        let rcols = table_columns(&rconn, tbl)?;
        if rcols.is_empty() {
            plans.push(Plan {
                tbl,
                cols: Vec::new(),
                pk: Vec::new(),
                ident: Vec::new(),
                rows: 0,
            });
            continue;
        }
        let cols: Vec<String> = lcols
            .iter()
            .filter(|c| rcols.contains(c))
            .cloned()
            .collect();
        let pk: Vec<String> = table_pk_cols(tbl).into_iter().map(String::from).collect();
        for k in &pk {
            anyhow::ensure!(
                cols.contains(k),
                "{tbl}: key column {k} missing on one side — refusing a fold that cannot match rows"
            );
        }
        let ident = identity_cols(tbl)
            .iter()
            .map(|c| c.to_string())
            .filter(|c| cols.contains(c))
            .collect();
        let rows: i64 = rconn.query_row(&format!("SELECT COUNT(*) FROM {}", q(tbl)), [], |r| {
            r.get(0)
        })?;
        plans.push(Plan {
            tbl,
            cols,
            pk,
            ident,
            rows,
        });
    }
    drop(rconn);

    conn.execute(
        "ATTACH DATABASE ?1 AS recovered",
        rusqlite::params![replica.to_string_lossy().into_owned()],
    )?;
    let result = (|| -> Result<FoldReport> {
        // The disjointness gate, before anything is written.
        let mut clashes = Vec::new();
        for p in plans.iter().filter(|p| !p.ident.is_empty() && p.rows > 0) {
            let on =
                p.pk.iter()
                    .map(|k| format!("y.{0} = x.{0}", q(k)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
            let same = p
                .ident
                .iter()
                .map(|c| format!("y.{0} IS x.{0}", q(c)))
                .collect::<Vec<_>>()
                .join(" AND ");
            let n: i64 = conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM recovered.{t} AS x JOIN main.{t} AS y ON {on}
                     WHERE NOT ({same})",
                    t = q(p.tbl)
                ),
                [],
                |r| r.get(0),
            )?;
            if n > 0 {
                clashes.push(format!("{} ({n})", p.tbl));
            }
        }
        if !clashes.is_empty() {
            bail!(
                "refused, nothing written: this store already holds rows under ids the replica \
                 uses for OTHER rows — {}. It was written to after the loss and numbered from 1 \
                 again. Move state.db aside and run the recovery again, so the replica's history \
                 lands first (renumbering the newer rows is an open item, PLAN-recover.md)",
                clashes.join(", ")
            );
        }

        let has_log: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'store_log')",
            [],
            |r| r.get(0),
        )?;
        let seq_of_log = |c: &rusqlite::Connection| -> i64 {
            c.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'store_log'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0)
        };
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let body = (|| -> Result<FoldReport> {
            let log_before: i64 = if has_log {
                conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM store_log", [], |r| {
                    r.get(0)
                })?
            } else {
                0
            };
            let seq_before = seq_of_log(conn);
            let mut report = FoldReport::default();
            for p in &plans {
                if p.cols.is_empty() {
                    report.tables.push((p.tbl.to_string(), 0, 0));
                    continue;
                }
                let list = p.cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
                let pick = p
                    .cols
                    .iter()
                    .map(|c| format!("x.{}", q(c)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let on =
                    p.pk.iter()
                        .map(|k| format!("y.{0} = x.{0}", q(k)))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                let n = conn.execute(
                    &format!(
                        "INSERT INTO main.{t} ({list}) SELECT {pick} FROM recovered.{t} AS x
                         WHERE NOT EXISTS (SELECT 1 FROM main.{t} AS y WHERE {on})",
                        t = q(p.tbl)
                    ),
                    [],
                )?;
                let kept = (p.rows as usize).saturating_sub(n);
                report.tables.push((p.tbl.to_string(), n, kept));
            }
            // The fold's rows are the peers' already: none of them is a
            // write to push. Then the log goes on after the cursor.
            conn.execute_batch(crate::storesync::STORE_LOG_SQL)?;
            if has_log {
                conn.execute("DELETE FROM store_log WHERE seq > ?1", [log_before])?;
            }
            let max_row: i64 =
                conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM store_log", [], |r| {
                    r.get(0)
                })?;
            let target = seq_before.max(max_row).max(cursor);
            let updated = conn.execute(
                "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'store_log'",
                [target],
            )?;
            if updated == 0 {
                conn.execute(
                    "INSERT INTO sqlite_sequence(name, seq) VALUES ('store_log', ?1)",
                    [target],
                )?;
            }
            crate::storesync::meta_put(
                conn,
                "veles.recovered",
                &json!({"at": chrono::Utc::now().to_rfc3339(), "replica_seq": cursor}).to_string(),
            )?;
            report.log_seq = target;
            Ok(report)
        })();
        match body {
            Ok(r) => {
                conn.execute_batch("COMMIT")?;
                Ok(r)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })();
    let _ = conn.execute("DETACH DATABASE recovered", []);
    result
}

// ── the recovering node: files ───────────────────────────────────────

/// A relative path from a peer, as a path under a root here — or nothing:
/// no absolute path, no `..`, no empty or `.` part, no drive letter.
pub fn safe_rel(p: &str) -> Option<PathBuf> {
    if p.is_empty() || p.starts_with('/') || p.contains('\\') || p.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." || c.contains(':') {
            return None;
        }
        out.push(c);
    }
    Some(out)
}

/// Write one file a peer sent under `base`, if nothing is there yet.
/// Returns whether it was written.
pub fn put_file(base: &Path, rel: &str, data: &[u8], exec: bool) -> Result<bool> {
    let r = safe_rel(rel).with_context(|| format!("refused path {rel:?} from the peer"))?;
    let path = base.join(r);
    if path.exists() || path.symlink_metadata().is_ok() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, data).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if exec { 0o755 } else { 0o644 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = exec;
    Ok(true)
}

// ── the recovering node: the run ─────────────────────────────────────

/// What `veles recover` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The recovery invite. `None` re-runs the pulls over the links a
    /// first recovery wrote (idempotent).
    pub invite: Option<crate::pool::Invite>,
    /// `--as`: the identity to take back.
    pub as_node: Option<String>,
    /// `--from`: the `[a2a_agents]` peer to pull from (default: the
    /// arbiter).
    pub from: Option<String>,
    /// Steps not to run ([`STEPS`]).
    pub skip: Vec<String>,
}

/// What a run did, what it could not, and what a person does next.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub identity: Option<Identity>,
    pub done: Vec<String>,
    pub not_restored: Vec<String>,
    pub next: Vec<String>,
}

#[derive(Debug, Clone)]
struct Peer {
    name: String,
    url: String,
    token: String,
}

async fn ask_peer(peer: &Peer, a: &Ask) -> Result<Value> {
    let client = crate::a2a::A2AClient::with_timeout(std::time::Duration::from_secs(600));
    let text = encode_ask(a)?;
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match client
            .call(
                &peer.url,
                Some(&peer.token),
                &text,
                &crate::a2a::new_context_id(),
            )
            .await
        {
            Ok(result) => {
                let v = decode_reply(&crate::a2a::extract_text(&result))
                    .with_context(|| format!("{}'s answer to {}", peer.name, a.op))?;
                if v["status"] != "ok" {
                    bail!(
                        "{} refused {}: {}",
                        peer.name,
                        a.op,
                        v["error"].as_str().unwrap_or("refused")
                    );
                }
                return Ok(v);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("no answer"))
        .context(format!(
            "{} ({}) did not answer {}",
            peer.name, peer.url, a.op
        )))
}

fn peer_of(cfg: &VelesConfig, name: &str) -> Result<Peer> {
    let link = cfg.a2a_agents.get(name).with_context(|| {
        format!("{name} is not a peer of this node ([a2a_agents.{name}]): nothing to pull from")
    })?;
    Ok(Peer {
        name: name.to_string(),
        url: link.url.clone(),
        token: link.auth.token.trim().to_string(),
    })
}

/// Rebuild this node from a pool peer: the steps in the module doc, in
/// order. `say` hears each step as it happens. Refuses while a gateway
/// runs on this home (the restore guard): every step writes under it.
pub async fn run(opts: &Options, say: &(dyn Fn(&str) + Sync)) -> Result<Outcome> {
    if crate::backup::gateway_alive() {
        bail!(
            "a gateway is running on this home — stop it first (`veles gateway stop`): a \
             recovery rewrites node.toml, state.db and config.toml under it"
        );
    }
    for s in &opts.skip {
        anyhow::ensure!(
            STEPS.contains(&s.as_str()),
            "--skip {s}: the steps are {}",
            STEPS.join(", ")
        );
    }
    let skip = |s: &str| opts.skip.iter().any(|x| x == s);
    let config_dir = crate::config::config_dir();
    let data = crate::config::veles_home();
    let db = data.join("state.db");
    let mut out = Outcome::default();

    // 1. identity
    // A re-run (no invite) needs an identity already here. Refused BEFORE
    // `settle_identity`, which would mint one: a refused run writes
    // nothing to the home (the e2e's "a refused re-run mints nothing").
    if opts.invite.is_none() && crate::node::NodeIdentity::load(&config_dir)?.is_none() {
        bail!(
            "this home has no identity and no invite was given: a first recovery needs the \
             recovery invite (`veles pool invite <name> --recover` on the arbiter)"
        );
    }
    let ident = settle_identity(
        &config_dir,
        &db,
        opts.invite.as_ref().and_then(|i| i.recover.as_ref()),
        opts.as_node.as_deref(),
    )?;
    let line = match &ident {
        Identity::Adopted { id, name, replaced } => format!(
            "identity: node {id} ({name}) taken back{}",
            replaced
                .as_ref()
                .map(|p| format!(
                    "; the identity this home had is set aside as {}",
                    p.display()
                ))
                .unwrap_or_default()
        ),
        Identity::Kept { id, name } => format!("identity: node {id} ({name}), kept"),
        Identity::Minted { id, name } => format!(
            "identity: a NEW node {id} ({name}) — the pool has not seen this id; sessions of the \
             node this home was are not this node's, and are not folded"
        ),
    };
    say(&line);
    out.done.push(line);
    let me = ident.id().to_string();
    let store = Store::open(&db)?;
    crate::storesync::stamp_owner(&store)?;

    // 2. links
    let mut not_introduced: Vec<String> = Vec::new();
    let mut source = opts.from.clone();
    if let Some(inv) = &opts.invite {
        let joined = crate::pool::join(inv).await?;
        crate::pool::seed_registry(&store, &joined);
        let line = format!(
            "links: {} and {} member(s){} — every token new",
            joined.arbiter.name,
            joined.members.len(),
            if joined.members.is_empty() {
                String::new()
            } else {
                format!(" ({})", joined.members.join(", "))
            }
        );
        say(&line);
        out.done.push(line);
        for n in &joined.not_introduced {
            not_introduced.push(n["name"].as_str().unwrap_or("?").to_string());
        }
        if let Some(w) = &joined.written.warning {
            out.next.push(w.clone());
        }
        source.get_or_insert(inv.arbiter.name.clone());
    }
    let cfg = crate::config::load()?;
    let source = match source {
        Some(s) => s,
        None => {
            let arb = cfg
                .cluster
                .arbiter
                .clone()
                .context("no --from, and this node has no [cluster] arbiter to pull from")?;
            crate::mailnet::remote(&store, &cfg, &arb)
                .context("this node does not know how it dials its arbiter: say --from <peer>")?
                .name
        }
    };
    let peer = peer_of(&cfg, &source)?;
    let inv = ask_peer(
        &peer,
        &Ask {
            op: "inventory".into(),
            node: me.clone(),
            ..Default::default()
        },
    )
    .await?;
    say(&format!(
        "source: {} ({}) knows this node as {} ({})",
        peer.name,
        peer.url,
        inv["node"].as_str().unwrap_or("?"),
        inv["via"].as_str().unwrap_or("?")
    ));

    // 3. sessions
    if skip("store") {
        out.not_restored
            .push("sessions: skipped (--skip store)".into());
    } else if !ident.is_old() {
        out.not_restored.push(
            "sessions: this is a new node, so the old node's sessions are not its own — they stay \
             in the pool's replicas of the old id (`veles replicas search` on a peer)"
                .into(),
        );
    } else if inv["replica"].is_null() {
        out.not_restored.push(format!(
            "sessions: {} holds no replica of this node{}",
            peer.name,
            if inv["storesync"] == true {
                String::new()
            } else {
                " it will serve (this node is not in its [cluster] storesync_peers)".to_string()
            }
        ));
    } else {
        match pull_store(&peer, &cfg, &me, &store, &inv, say).await {
            Ok(lines) => out.done.extend(lines),
            Err(e) => out.not_restored.push(format!("sessions: {e:#}")),
        }
    }

    // 4. memory
    if skip("memory") {
        out.not_restored
            .push("memory: skipped (--skip memory)".into());
    } else {
        match pull_memory(&peer, &cfg, &me, &inv, say).await {
            Ok(lines) => out.done.extend(lines),
            Err(e) => out.not_restored.push(format!("memory: {e:#}")),
        }
    }

    // 5. config
    if skip("config") {
        out.not_restored
            .push("config: skipped (--skip config)".into());
    } else {
        match pull_config(&peer, &me, ident.name(), &store, say).await {
            Ok(lines) => out.done.extend(lines),
            Err(e) => out.not_restored.push(format!("config: {e:#}")),
        }
    }

    // 6. skills and commands
    if skip("files") {
        out.not_restored
            .push("skills, commands: skipped (--skip files)".into());
    } else if inv["window"] != true {
        out.not_restored.push(format!(
            "skills, commands: {} serves them only while a recovery window is open for this \
             node (a recovery invite redeemed on the arbiter, or its introduction) — none is",
            peer.name
        ));
    } else {
        for root in FILE_ROOTS {
            match pull_files(&peer, &me, root, &config_dir.join(root)).await {
                Ok((line, withheld)) => {
                    say(&line);
                    out.done.push(line);
                    if !withheld.is_empty() {
                        out.not_restored.push(format!(
                            "{root}: {} withheld by {} (they hold credential-shaped text or are \
                             over 4 MiB): {}",
                            withheld.len(),
                            peer.name,
                            withheld.join(", ")
                        ));
                    }
                }
                Err(e) => out.not_restored.push(format!("{root}: {e:#}")),
            }
        }
    }
    let plugins: Vec<String> = inv["plugins"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    out.not_restored.push(format!(
        "plugins: not copied — code is installed through the plugin installer, never pulled{}",
        if plugins.is_empty() {
            String::new()
        } else {
            format!(" ({} has: {})", peer.name, plugins.join(", "))
        }
    ));

    // 7. vault
    if skip("vault") {
        out.not_restored
            .push("vault: skipped (--skip vault)".into());
    } else {
        match vault_step(&peer, &me, ident.name(), &inv, &mut out.next).await {
            Ok(line) => {
                say(&line);
                out.done.push(line);
            }
            Err(e) => out.not_restored.push(format!("vault: {e:#}")),
        }
    }

    // 8. what no step can bring back
    if !not_introduced.is_empty() {
        out.not_restored.push(format!(
            "links to {}: they did not answer the introduction, so they still hold this node's \
             OLD tokens — on the arbiter, `veles pool introduce <this node> <member>` for each",
            not_introduced.join(", ")
        ));
    }
    out.not_restored.extend(standing_losses());
    out.identity = Some(ident);
    Ok(out)
}

/// What a recovery never restores, because nothing outside this home
/// held it (PLAN-recover.md "What it cannot restore").
pub fn standing_losses() -> Vec<String> {
    vec![
        format!(
            "node-local config keys (never replicated): [node] beyond its endpoint, [cluster] \
             beyond its arbiter, peer lists and signers, [tui], [node.overrides], and {}",
            crate::configsync::NODE_KEYS.join(", ")
        ),
        "the .env (A2A_PEER_TOKENS is rewritten by the links): provider keys, bot tokens and \
         VELES_SIGNER_TOKEN come back from the vault once a member admits this node; anything \
         never put in the vault is re-entered by hand"
            .into(),
        "memory paths that never leave their node: reference/nodes/** and [cluster] \
         local_memory_globs"
            .into(),
        "session rows the peer's replica had not applied when the home was lost, and this node's \
         node-local tables (approvals and \"always allow\" rules, cron runs, triggers, the \
         inbound ledger)"
            .into(),
        "replicas of OTHER nodes this node held: each sender re-sends on its next push (the gap \
         heal)"
            .into(),
        "files under the data dir no sync carries: notes/, scripts/, apps/, app-data/, plans/, \
         certs/, media/ — only a `veles backup` has them"
            .into(),
        "outside veles: the ticket signer's key (~/.config/veles-signer), WireGuard keys, ssh \
         keys, and service units (on NixOS the gateway's PATH drop-in)"
            .into(),
    ]
}

async fn pull_store(
    peer: &Peer,
    cfg: &VelesConfig,
    me: &str,
    store: &Store,
    inv: &Value,
    say: &(dyn Fn(&str) + Sync),
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let snap = ask_peer(
        peer,
        &Ask {
            op: "replica_snapshot".into(),
            node: me.to_string(),
            ..Default::default()
        },
    )
    .await?;
    let token = snap["token"].as_str().unwrap_or_default().to_string();
    let size = snap["size"].as_u64().unwrap_or(0);
    let parts = snap["parts"].as_u64().unwrap_or(0);
    let want_sha = snap["sha256"].as_str().unwrap_or_default().to_string();
    say(&format!(
        "sessions: pulling {}'s replica of this node ({size} bytes, {parts} part(s))",
        peer.name
    ));
    let dir = crate::config::veles_home().join("recover");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{me}-replica.db"));
    let tmp = dir.join(format!("{me}-replica.db.part"));
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        for i in 0..parts {
            let p = ask_peer(
                peer,
                &Ask {
                    op: "replica_part".into(),
                    node: me.to_string(),
                    token: token.clone(),
                    index: i,
                    ..Default::default()
                },
            )
            .await?;
            f.write_all(&unb64(p["b64"].as_str().unwrap_or_default())?)?;
            if parts > 8 && (i + 1) % 8 == 0 {
                say(&format!("  part {}/{parts}", i + 1));
            }
        }
        f.sync_all()?;
    }
    let _ = ask_peer(
        peer,
        &Ask {
            op: "replica_done".into(),
            node: me.to_string(),
            token,
            ..Default::default()
        },
    )
    .await;
    let (got_size, got_sha) = sha_file(&tmp)?;
    anyhow::ensure!(
        got_size == size && got_sha == want_sha,
        "the snapshot that arrived is not the one {} made (size {got_size}/{size}, sha mismatch: \
         {})",
        peer.name,
        got_sha != want_sha
    );
    std::fs::rename(&tmp, &path)?;

    // The peer's store offers from this node it never applied: onto the
    // snapshot, in arrival order, until one does not fit.
    let waiting = inv["store_inbox"].as_u64().unwrap_or(0);
    if waiting > 0 {
        let conn = rusqlite::Connection::open(&path)?;
        let mut applied = 0u64;
        for i in 0..waiting {
            let r = ask_peer(
                peer,
                &Ask {
                    op: "store_inbox".into(),
                    node: me.to_string(),
                    index: i,
                    ..Default::default()
                },
            )
            .await?;
            let offer: crate::storesync::StoreOffer = serde_json::from_value(r["offer"].clone())?;
            match crate::storesync::apply_offer(&conn, &offer) {
                Ok(_) => applied += 1,
                Err(e) => {
                    lines.push(format!(
                        "sessions: {} of {waiting} store offer(s) waiting on {} folded onto the \
                         snapshot; the next did not fit ({e:#})",
                        applied, peer.name
                    ));
                    break;
                }
            }
        }
        if applied == waiting {
            lines.push(format!(
                "sessions: {waiting} store offer(s) {} had not applied, folded onto the snapshot",
                peer.name
            ));
        }
    }
    let mut cursor: i64 = {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        crate::storesync::meta_get(&conn, crate::storesync::APPLIED_SEQ_META_KEY)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    // Every other member that holds a replica of this node: the log goes
    // on after the furthest of them, or that one skips the new rows.
    for name in cfg.a2a_agents.keys().filter(|n| **n != peer.name) {
        let Ok(other) = peer_of(cfg, name) else {
            continue;
        };
        let asked = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            ask_peer(
                &other,
                &Ask {
                    op: "inventory".into(),
                    node: me.to_string(),
                    ..Default::default()
                },
            ),
        )
        .await;
        if let Ok(Ok(v)) = asked {
            if let Some(c) = v["replica"]["applied_seq"].as_i64() {
                if c > cursor {
                    lines.push(format!(
                        "sessions: {name}'s replica of this node is further on (cursor {c})"
                    ));
                    cursor = c;
                }
            }
        }
    }
    let rep = fold_own_replica(store, &path, me, cursor)?;
    let total: usize = rep.tables.iter().map(|(_, i, _)| *i).sum();
    let kept: usize = rep.tables.iter().map(|(_, _, k)| *k).sum();
    let line = format!(
        "sessions: {} session(s), {} message(s) back from {}'s replica ({total} row(s) in all; \
         {kept} already here); the store log goes on after seq {}",
        rep.inserted("sessions"),
        rep.inserted("messages"),
        peer.name,
        rep.log_seq
    );
    say(&line);
    lines.push(line);
    let _ = std::fs::remove_file(&path);
    Ok(lines)
}

async fn pull_memory(
    peer: &Peer,
    cfg: &VelesConfig,
    me: &str,
    inv: &Value,
    say: &(dyn Fn(&str) + Sync),
) -> Result<Vec<String>> {
    let ms = crate::memory::MemoryStore::new();
    let globs = &cfg.cluster.local_memory_globs;
    let mut lines = Vec::new();
    let (mut applied, mut unchanged, mut conflicts) = (0usize, 0usize, 0usize);
    let mut withheld: Vec<String> = Vec::new();
    let mut index = 0u64;
    let mut parts = 1u64;
    while index < parts {
        let r = ask_peer(
            peer,
            &Ask {
                op: "memory".into(),
                node: me.to_string(),
                index,
                ..Default::default()
            },
        )
        .await?;
        parts = r["parts"].as_u64().unwrap_or(0);
        if index == 0 {
            withheld = serde_json::from_value(r["withheld"].clone()).unwrap_or_default();
        }
        let offer: crate::memsync::SyncOffer = serde_json::from_value(r["offer"].clone())?;
        let rep = crate::memsync::apply_offer_scoped(&ms, &offer, globs)?;
        applied += rep.applied.len();
        unchanged += rep.unchanged.len();
        conflicts += rep.conflicts.len();
        index += 1;
    }
    let line = format!(
        "memory: {applied} path(s) from {}, {unchanged} already here{}",
        peer.name,
        if conflicts > 0 {
            format!(", {conflicts} conflict(s) kept local")
        } else {
            String::new()
        }
    );
    say(&line);
    lines.push(line);
    if !withheld.is_empty() {
        lines.push(format!(
            "memory: {} path(s) {} keeps to itself (node-local or credential-shaped): {}",
            withheld.len(),
            peer.name,
            withheld.join(", ")
        ));
    }
    // This node's own offers the peer never applied (a conflict there, or
    // its memsync_auto off): the newest memory this node had.
    let waiting = inv["memory_inbox"].as_u64().unwrap_or(0);
    let mut parked = 0usize;
    for i in 0..waiting {
        let r = ask_peer(
            peer,
            &Ask {
                op: "memory_inbox".into(),
                node: me.to_string(),
                index: i,
                ..Default::default()
            },
        )
        .await?;
        let offer: crate::memsync::SyncOffer = serde_json::from_value(r["offer"].clone())?;
        let rep = crate::memsync::apply_offer_scoped(&ms, &offer, globs)?;
        if !rep.is_clean() {
            crate::memsync::inbox_store(
                &crate::memsync::inbox_dir(),
                &peer.name,
                &format!("recover-{i}"),
                offer,
            )?;
            parked += 1;
        }
    }
    if waiting > 0 {
        let line = format!(
            "memory: {waiting} offer(s) of this node's own that {} had not applied{}",
            peer.name,
            if parked > 0 {
                format!(
                    " — {parked} conflict with what came back and wait here: `veles memory apply \
                     --inbox` to pick"
                )
            } else {
                ", applied".to_string()
            }
        );
        say(&line);
        lines.push(line);
    }
    Ok(lines)
}

async fn pull_config(
    peer: &Peer,
    me: &str,
    my_name: &str,
    store: &Store,
    say: &(dyn Fn(&str) + Sync),
) -> Result<Vec<String>> {
    let r = ask_peer(
        peer,
        &Ask {
            op: "config".into(),
            node: me.to_string(),
            ..Default::default()
        },
    )
    .await?;
    let rows: Vec<crate::configsync::ConfigLogRow> = serde_json::from_value(r["rows"].clone())?;
    let imp = crate::configsync::import_recovered(store, me, &rows)?;
    let applied = crate::configsync::apply_offer(
        store,
        me,
        &crate::configsync::ConfigSyncOffer {
            from_node: me.to_string(),
            from_name: my_name.to_string(),
            since_seq: 0,
            rows: Vec::new(),
        },
    )?;
    let mut lines = vec![format!(
        "config: {} row(s) of {} node(s) from {} ({} this node's own; {} already here); {} key(s) \
         written to config.toml",
        imp.imported,
        imp.nodes,
        peer.name,
        imp.mine,
        imp.present,
        applied.changed.len()
    )];
    say(&lines[0]);
    let withheld: Vec<String> = serde_json::from_value(r["withheld"].clone()).unwrap_or_default();
    if !withheld.is_empty() {
        lines.push(format!(
            "config: {} key(s) whose values look like credentials stayed on {}: {}",
            withheld.len(),
            peer.name,
            withheld.join(", ")
        ));
    }
    if !applied.is_clean() {
        lines.push(format!(
            "config: {} conflict(s) — the later write is in the file; decide by hand:\n{}",
            applied.conflicts.len(),
            applied.summarize()
        ));
    }
    Ok(lines)
}

async fn pull_files(
    peer: &Peer,
    me: &str,
    root: &str,
    base: &Path,
) -> Result<(String, Vec<String>)> {
    let mut index = 0u64;
    let (mut written, mut present) = (0usize, 0usize);
    // Set by every pass of the loop before its one `break`.
    let mut total: usize;
    let mut withheld = Vec::new();
    loop {
        let r = ask_peer(
            peer,
            &Ask {
                op: "files".into(),
                node: me.to_string(),
                root: root.to_string(),
                index,
                ..Default::default()
            },
        )
        .await?;
        total = r["total"].as_u64().unwrap_or(0) as usize;
        for f in r["files"].as_array().into_iter().flatten() {
            let rel = f["path"].as_str().unwrap_or_default();
            let data = unb64(f["b64"].as_str().unwrap_or_default())?;
            if put_file(base, rel, &data, f["exec"] == true)? {
                written += 1;
            } else {
                present += 1;
            }
        }
        for key in ["withheld", "skipped"] {
            for w in r[key].as_array().into_iter().flatten() {
                withheld.push(w.as_str().unwrap_or_default().to_string());
            }
        }
        match r["next"].as_u64() {
            Some(n) if n > index => index = n,
            _ => break,
        }
    }
    Ok((
        format!(
            "{root}: {written} file(s) copied from {}'s own {root} ({present} already here, {total} \
             there) — {root} are per node: these are its, not a replica of this node's",
            peer.name
        ),
        withheld,
    ))
}

async fn vault_step(
    peer: &Peer,
    me: &str,
    my_name: &str,
    inv: &Value,
    next: &mut Vec<String>,
) -> Result<String> {
    use crate::vault;
    let dir = vault::vault_dir();
    let status = vault::identity_status_in(&dir, me, my_name)?;
    let had_keys = vault::NodeKeys::load(&dir, me, my_name)?.is_some();
    if status == vault::IdentityStatus::Member {
        return Ok("vault: this node's keys are a member's — nothing to do".into());
    }
    if inv["vault"] != true {
        return Ok(format!(
            "vault: {} has none, so there is nothing to be admitted to",
            peer.name
        ));
    }
    let req = if had_keys {
        vault::Vault::open_local()?.join_request()
    } else {
        let r = vault::reidentify(&dir, me, my_name)?;
        let req = r.vault.join_request();
        let reply = vault::push_to_peer(
            &vault::Message::Join(req.clone()),
            &peer.url,
            Some(&peer.token),
        )
        .await
        .with_context(|| format!("send the join request to {}", peer.name))?;
        let _ = reply;
        req
    };
    let fp = req.fingerprint();
    next.push(format!(
        "vault: on {} (or any member), after reading the fingerprint {fp} off THIS screen: \
         `veles vault admit {me} --fingerprint {fp}` — it re-encrypts every secret for this \
         node's new key and pushes the vault here on its next sync",
        peer.name
    ));
    next.push(format!(
        "vault: then here: `veles vault apply --inbox --trust <root fingerprint>` (the root \
         fingerprint as `veles vault status` on {} prints it), and `veles vault status` to see \
         every secret readable",
        peer.name
    ));
    if had_keys {
        next.push(format!(
            "vault: if {} no longer has the join request, send it again: `veles vault join --push \
             {}`",
            peer.name, peer.name
        ));
        Ok(format!(
            "vault: keys from an earlier run kept (fingerprint {fp}); waiting for a member to \
             admit them"
        ))
    } else {
        Ok(format!(
            "vault: new keys minted (fingerprint {fp}) and the join request is in {}'s vault inbox",
            peer.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ChatMessage;

    /// THIS node's old store, with its log, and a replica of it as a
    /// peer would hold it (built by the real apply). Returns the
    /// replica's path and its cursor.
    fn old_image(me: &str) -> (PathBuf, i64) {
        let old = Store::open(&crate::config::data_dir().join("old.db")).unwrap();
        crate::storesync::install_log(&old, 0).unwrap();
        old.create_session("s-1").unwrap();
        old.append_message("s-1", &ChatMessage::user("hello recovered world"))
            .unwrap();
        old.append_message("s-1", &ChatMessage::assistant("hi there", None))
            .unwrap();
        old.create_session("s-2").unwrap();
        old.append_message("s-2", &ChatMessage::user("a second session"))
            .unwrap();
        let offer = crate::storesync::build_offer(&old, me, "laptop", 0).unwrap();
        let conn = crate::storesync::open_replica(me).unwrap();
        let rep = crate::storesync::apply_offer(&conn, &offer).unwrap();
        (crate::storesync::replica_path(me), rep.final_seq)
    }

    fn me() -> String {
        crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())
            .unwrap()
            .id
    }

    fn fresh_store() -> Store {
        let s = Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        crate::storesync::stamp_owner(&s).unwrap();
        s
    }

    fn count(s: &Store, sql: &str) -> i64 {
        s.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// The one legitimate fold of a replica into a primary: the owner
    /// takes its own image back. Every row lands under its own id and is
    /// searchable once; a second fold changes nothing; new rows number
    /// after the old ones; and the store log goes on after the replica's
    /// cursor, so the peer's next apply of this node's rows is gapless.
    #[test]
    fn a_node_folds_its_own_replica_back_whole_and_once() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = me();
        let (replica, cursor) = old_image(&me);
        assert!(cursor > 0);
        let store = fresh_store();
        let rep = fold_own_replica(&store, &replica, &me, cursor).unwrap();
        assert_eq!(rep.inserted("sessions"), 2, "{rep:?}");
        assert_eq!(rep.inserted("messages"), 3, "{rep:?}");
        assert_eq!(rep.log_seq, cursor);
        assert_eq!(store.load_history("s-1").unwrap().len(), 2);
        let mine: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE node = ?1",
                [&me],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mine, 2, "the sessions come back under this node's id");
        let fts = |s: &Store| {
            count(
                s,
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'recovered'",
            )
        };
        assert_eq!(fts(&store), 1, "searchable, once");
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM store_log"),
            0,
            "the fold logs nothing for the peers"
        );

        let again = fold_own_replica(&store, &replica, &me, cursor).unwrap();
        assert_eq!(again.inserted("messages"), 0);
        assert_eq!(again.inserted("sessions"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages_fts"), 3);

        let max_old = count(&store, "SELECT MAX(msg_id) FROM messages");
        let id = store
            .append_message("s-1", &ChatMessage::user("after the recovery"))
            .unwrap();
        assert!(
            id > max_old,
            "new rows number after the old ({id} > {max_old})"
        );

        // The log, installed as a gateway would, issues cursor + 1 next.
        crate::storesync::install_triggers(&store).unwrap();
        store
            .append_message("s-1", &ChatMessage::user("logged"))
            .unwrap();
        let first: i64 = count(&store, "SELECT MIN(seq) FROM store_log");
        assert_eq!(
            first,
            cursor + 1,
            "the next log row follows the replica's cursor"
        );
    }

    /// A store whose log is already installed (a gateway ran once with
    /// store sync on) logs none of the fold's rows — the peers hold them
    /// all — and its next write still follows the replica's cursor.
    #[test]
    fn the_fold_leaves_the_store_log_to_new_writes() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = me();
        let (replica, cursor) = old_image(&me);
        let store = fresh_store();
        crate::storesync::install_triggers(&store).unwrap();
        let rep = fold_own_replica(&store, &replica, &me, cursor).unwrap();
        assert_eq!(rep.inserted("messages"), 3);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM store_log"), 0);
        store
            .append_message("s-2", &ChatMessage::user("the first new write"))
            .unwrap();
        assert_eq!(count(&store, "SELECT MIN(seq) FROM store_log"), cursor + 1);
    }

    /// A replica is the owner's image: it folds into the owner's store
    /// and nowhere else.
    #[test]
    fn a_replica_folds_only_into_its_owners_store() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = me();
        let (replica, cursor) = old_image(&me);
        let store = fresh_store();
        let e = fold_own_replica(&store, &replica, "someone-else", cursor).unwrap_err();
        assert!(e.to_string().contains("image of node"), "{e}");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages"), 0);
        store
            .conn()
            .execute(
                "UPDATE meta SET value = 'another-node' WHERE key = ?1",
                [crate::storesync::OWNER_META_KEY],
            )
            .unwrap();
        let e = fold_own_replica(&store, &replica, &me, cursor).unwrap_err();
        assert!(
            e.to_string().contains("belongs to node another-node"),
            "{e}"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages"), 0);
    }

    /// A store written after the loss numbered its messages from 1 again:
    /// a shared id naming a different row refuses the whole fold, and
    /// nothing is written.
    #[test]
    fn a_reused_id_refuses_the_fold_and_writes_nothing() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = me();
        let (replica, cursor) = old_image(&me);
        let store = fresh_store();
        store.create_session("s-after").unwrap();
        store
            .append_message("s-after", &ChatMessage::user("written after the loss"))
            .unwrap();
        let e = fold_own_replica(&store, &replica, &me, cursor).unwrap_err();
        assert!(e.to_string().contains("messages (1)"), "{e}");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
    }

    /// The owner's newer image of a row wins over its older one: a title
    /// changed since is kept.
    #[test]
    fn this_stores_own_newer_row_is_kept() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = me();
        let (replica, cursor) = old_image(&me);
        let store = fresh_store();
        fold_own_replica(&store, &replica, &me, cursor).unwrap();
        store
            .conn()
            .execute(
                "UPDATE sessions SET title = 'renamed since' WHERE id = 's-1'",
                [],
            )
            .unwrap();
        let rep = fold_own_replica(&store, &replica, &me, cursor).unwrap();
        assert_eq!(rep.inserted("sessions"), 0);
        let title: String = store
            .conn()
            .query_row("SELECT title FROM sessions WHERE id = 's-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "renamed since");
    }

    fn laptop() -> crate::pool::NodeRef {
        crate::pool::NodeRef {
            id: "38a5e4dd-d804-4a27-9f31-53b8cfb4d32f".into(),
            name: "laptop".into(),
            url: "http://10.0.30.2:9900".into(),
        }
    }

    /// Taking an identity back: the invite names it and --as confirms it;
    /// without --as, or with another, it is refused; a home that runs as
    /// another node is replaced only while its store is empty.
    #[test]
    fn an_identity_is_taken_back_only_when_named_and_confirmed() {
        // The store stamps sessions with THIS home's node: a scratch one,
        // never the developer's own (pitfall 86).
        let (_home, _g) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        let db = dir.path().join("state.db");
        let r = laptop();
        let e = settle_identity(&cfg, &db, Some(&r), None).unwrap_err();
        assert!(e.to_string().contains("--as laptop"), "{e}");
        let e = settle_identity(&cfg, &db, Some(&r), Some("phone")).unwrap_err();
        assert!(e.to_string().contains("not the node"), "{e}");
        assert!(!cfg.join("node.toml").exists(), "a refusal writes nothing");

        let got = settle_identity(&cfg, &db, Some(&r), Some("38a5e4dd")).unwrap();
        assert_eq!(got.id(), r.id);
        assert!(matches!(got, Identity::Adopted { replaced: None, .. }));
        let on_disk = crate::node::NodeIdentity::load(&cfg).unwrap().unwrap();
        assert_eq!(
            (on_disk.id.as_str(), on_disk.name.as_str()),
            (r.id.as_str(), "laptop")
        );
        // Again: the same, nothing replaced.
        let again = settle_identity(&cfg, &db, Some(&r), Some("laptop")).unwrap();
        assert!(matches!(again, Identity::Adopted { replaced: None, .. }));

        // A home that minted itself another id, with an empty store.
        std::fs::write(cfg.join("node.toml"), "id = \"fresh-id\"\nname = \"x\"\n").unwrap();
        {
            let s = Store::open(&db).unwrap();
            s.conn()
                .execute(
                    "INSERT INTO meta(key, value) VALUES (?1, 'fresh-id')",
                    [crate::storesync::OWNER_META_KEY],
                )
                .unwrap();
        }
        let got = settle_identity(&cfg, &db, Some(&r), Some("laptop")).unwrap();
        let Identity::Adopted {
            replaced: Some(aside),
            ..
        } = got
        else {
            panic!("{got:?}")
        };
        assert!(std::fs::read_to_string(aside).unwrap().contains("fresh-id"));
        let s = Store::open(&db).unwrap();
        assert_eq!(
            crate::storesync::owner_of(s.conn()).as_deref(),
            Some(r.id.as_str())
        );

        // ...and one whose store holds sessions: refused, untouched.
        std::fs::write(cfg.join("node.toml"), "id = \"busy-id\"\nname = \"x\"\n").unwrap();
        s.create_session("s-busy").unwrap();
        let e = settle_identity(&cfg, &db, Some(&r), Some("laptop")).unwrap_err();
        assert!(e.to_string().contains("move state.db"), "{e}");
        assert!(std::fs::read_to_string(cfg.join("node.toml"))
            .unwrap()
            .contains("busy-id"));

        // No recovery invite: --as is refused, and an empty home mints.
        let dir2 = tempfile::tempdir().unwrap();
        let cfg2 = dir2.path().join("config");
        let db2 = dir2.path().join("state.db");
        assert!(settle_identity(&cfg2, &db2, None, Some("laptop")).is_err());
        let got = settle_identity(&cfg2, &db2, None, None).unwrap();
        assert!(matches!(got, Identity::Minted { .. }), "{got:?}");
        assert!(matches!(
            settle_identity(&cfg2, &db2, None, None).unwrap(),
            Identity::Kept { .. }
        ));
    }

    #[test]
    fn a_peers_path_lands_under_the_root_or_nowhere() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "c:/x",
            "a\\b",
        ] {
            assert!(safe_rel(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            safe_rel("cat/skill/SKILL.md"),
            Some(PathBuf::from("cat/skill/SKILL.md"))
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(put_file(dir.path(), "a/b.md", b"one", false).unwrap());
        assert!(
            !put_file(dir.path(), "a/b.md", b"two", false).unwrap(),
            "never overwrites"
        );
        assert_eq!(std::fs::read(dir.path().join("a/b.md")).unwrap(), b"one");
        assert!(put_file(dir.path(), "../escape", b"x", false).is_err());
    }

    /// The peer's side: it answers a peer it can bind to a node, for that
    /// node only, and each kind of data only to the list that already
    /// carries it.
    #[test]
    fn the_peer_serves_a_node_its_own_image_only() {
        let (_home, _g) = crate::testenv::scratch_home();
        let laptop = "38a5e4dd-d804-4a27-9f31-53b8cfb4d32f";
        // The replica of `laptop` this node holds.
        {
            let old = Store::open(&crate::config::data_dir().join("old.db")).unwrap();
            crate::storesync::install_log(&old, 0).unwrap();
            old.create_session("s-1").unwrap();
            old.append_message("s-1", &ChatMessage::user("from the laptop"))
                .unwrap();
            let offer = crate::storesync::build_offer(&old, laptop, "laptop", 0).unwrap();
            let conn = crate::storesync::open_replica(laptop).unwrap();
            crate::storesync::apply_offer(&conn, &offer).unwrap();
        }
        let store = Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        store
            .node_register(laptop, "laptop", "", "", "", None, None, None, None)
            .unwrap();
        let store = std::sync::Mutex::new(store);
        let mut cfg = VelesConfig::default();
        cfg.cluster.storesync_peers = vec!["laptop".into()];
        cfg.cluster.memsync_peers = vec!["laptop".into()];
        let ask = |op: &str, node: &str| Ask {
            op: op.into(),
            node: node.into(),
            ..Default::default()
        };

        let r = serve(&store, &cfg, "phone", &ask("inventory", "anything"));
        assert_eq!(r["status"], "refused", "{r}");
        let r = serve(&store, &cfg, "laptop", &ask("inventory", "another-node"));
        assert!(
            r["error"].as_str().unwrap().contains("only its own image"),
            "{r}"
        );

        let r = serve(&store, &cfg, "laptop", &ask("inventory", laptop));
        assert_eq!(r["status"], "ok", "{r}");
        assert_eq!(r["via"], "registry");
        assert_eq!(r["replica"]["messages"], 1, "{r}");
        assert_eq!(r["window"], false);

        // The snapshot, in parts, is the replica.
        let s = serve(&store, &cfg, "laptop", &ask("replica_snapshot", laptop));
        assert_eq!(s["status"], "ok", "{s}");
        let token = s["token"].as_str().unwrap().to_string();
        let mut bytes = Vec::new();
        for i in 0..s["parts"].as_u64().unwrap() {
            let p = serve(
                &store,
                &cfg,
                "laptop",
                &Ask {
                    op: "replica_part".into(),
                    node: laptop.into(),
                    token: token.clone(),
                    index: i,
                    ..Default::default()
                },
            );
            bytes.extend(unb64(p["b64"].as_str().unwrap()).unwrap());
        }
        assert_eq!(bytes.len() as u64, s["size"].as_u64().unwrap());
        let got = crate::config::data_dir().join("got.db");
        std::fs::write(&got, &bytes).unwrap();
        assert_eq!(sha_file(&got).unwrap().1, s["sha256"].as_str().unwrap());
        let conn = rusqlite::Connection::open(&got).unwrap();
        assert_eq!(
            crate::storesync::meta_get(&conn, crate::storesync::REPLICA_OF_META_KEY).as_deref(),
            Some(laptop)
        );

        // Skills and commands: only inside a recovery window.
        let skill = crate::config::config_dir().join("skills/probe/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "# probe\n").unwrap();
        let leaky = crate::config::config_dir().join("skills/leaky/SKILL.md");
        std::fs::create_dir_all(leaky.parent().unwrap()).unwrap();
        std::fs::write(
            &leaky,
            "token: ghp_0123456789abcdefghijklmnopqrstuvwxyzAB\n",
        )
        .unwrap();
        let files_ask = Ask {
            op: "files".into(),
            node: laptop.into(),
            root: "skills".into(),
            ..Default::default()
        };
        let r = serve(&store, &cfg, "laptop", &files_ask);
        assert!(
            r["error"].as_str().unwrap().contains("recovery window"),
            "{r}"
        );
        store
            .lock_ok()
            .conn()
            .execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)",
                rusqlite::params![
                    format!("{}laptop", crate::pool::RECOVERY_META_PREFIX),
                    format!("{laptop} {}", now_secs() + 3600)
                ],
            )
            .unwrap();
        let r = serve(&store, &cfg, "laptop", &files_ask);
        assert_eq!(r["status"], "ok", "{r}");
        let paths: Vec<&str> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["probe/SKILL.md"]);
        assert_eq!(r["withheld"], json!(["leaky/SKILL.md"]));
        let bad_root = Ask {
            root: "plugins".into(),
            ..files_ask.clone()
        };
        assert_eq!(
            serve(&store, &cfg, "laptop", &bad_root)["status"],
            "refused"
        );

        // Not on the list that carried it: not served.
        cfg.cluster.storesync_peers.clear();
        let r = serve(&store, &cfg, "laptop", &ask("replica_snapshot", laptop));
        assert!(
            r["error"].as_str().unwrap().contains("storesync_peers"),
            "{r}"
        );
        cfg.cluster.memsync_peers.clear();
        let r = serve(&store, &cfg, "laptop", &ask("config", laptop));
        assert!(
            r["error"].as_str().unwrap().contains("memsync_peers"),
            "{r}"
        );
    }
}
