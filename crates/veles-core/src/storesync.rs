//! B5 — store replication: each node's session data shipped to peers as
//! read-only replica files (`replicas/<node-id>.db`), so any node can
//! answer "which node owns session X" and serve history reads when the
//! owner is down. The design lives in `PLAN-compute-pool.md` §"What to
//! actually build" item 4; the shape decisions (USER, 2026-09-28):
//!
//! - **Hand-rolled, not Litestream** — a third always-running Go binary
//!   lost to "the standing preference is not to run things"; the
//!   hand-rolled gossip and the configsync transport are the precedent.
//! - **Logical rows, not WAL frames.** The units memsync and configsync
//!   already ship. WAL frames would copy `memory_revisions`,
//!   `singleton_leases` and the `nodes` registry wholesale — bypassing
//!   memsync's per-path conflict detection and the arbiter's authority —
//!   and would pin both nodes to identical schema versions.
//!
//! The mechanism: every replicated table carries AFTER INSERT/UPDATE/
//! DELETE triggers appending full row images to a `store_log` (columns
//! named in the SQL, so schema drift regenerates them — the generated
//! SQL's sha is the version, kept in meta); a push ships log rows past
//! a cursor; the receiver folds them into `replicas/<node>.db` — a
//! SEPARATE veles-schema database, never the receiver's own
//! `state.db`. Ids stay as-is: AUTOINCREMENT collisions are impossible
//! because a replica is the owner's own image, not a merge into local
//! tables. Session takeover (B6 adopt) is what re-keys rows — its
//! design, not this one.
//!
//! What replicates: session DATA only — the tables in
//! [`REPLICATED_TABLES`]. What never does: memory (`memory_revisions`/
//! `memories` — memsync's, with its per-path conflict detection; a log
//! row naming one is REFUSED, not skipped), the `nodes` registry
//! (gossip's), `singleton_leases` and cron state (the arbiter's),
//! `config_log`/`config_scan` (configsync's), `inbound_ledger`,
//! approvals and triggers (node-local control state).
//!
//! Rows whose image is credential-shaped are withheld and named, the
//! configsync defense-in-depth rule; a withheld message leaves a hole
//! the apply report names, rather than shipping a key in the clear.

use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Tables whose rows replicate to peer replicas.
///
/// `session_runs` is NOT here (it was, 2026-09-28): it is the live-turn
/// registry — pid, host and a heartbeat every few seconds — so it logged
/// a full row per heartbeat for a fact that is only true on the node
/// that wrote it. B6's quiesce check reads the local table.
///
/// `session_prompt` IS here (2026-09-29): a takeover from a replica
/// sends the dead owner's frozen prompt and tool list, so the provider's
/// cache for the session survives the failover as it survives a move.
/// So is `session_takeovers`: the owner learns it was taken over from
/// its replica of the node that did it (`crate::takeover`).
pub const REPLICATED_TABLES: [&str; 13] = [
    "sessions",
    "messages",
    "usage",
    "checkpoints",
    "irc_lines",
    "todos",
    "session_overrides",
    "session_locale",
    "session_queue",
    "engine_notes",
    "skill_uses",
    "session_prompt",
    "session_takeovers",
];

/// The meta key holding the owning node's id on a PRIMARY store.
pub const OWNER_META_KEY: &str = "veles.owner_node";
/// The meta key a replica carries, naming the node it mirrors.
pub const REPLICA_OF_META_KEY: &str = "veles.replica_of";
/// The replica's applied cursor (highest `store_log.seq` folded in).
pub const APPLIED_SEQ_META_KEY: &str = "veles.applied_seq";
/// Meta key holding the sha of the installed trigger SQL — drift (a
/// column added to a replicated table) recreates the triggers.
const TRIGGERS_SHA_META_KEY: &str = "veles.store_triggers_sha";

// ── the sender: log table, triggers, cursors ────────────────────────

pub(crate) const STORE_LOG_SQL: &str = "CREATE TABLE IF NOT EXISTS store_log (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                at TEXT NOT NULL,
                tbl TEXT NOT NULL,
                op TEXT NOT NULL,             -- I | U | D
                pk TEXT NOT NULL,             -- JSON object of pk cols
                row_json TEXT NOT NULL        -- full row as a JSON object
            )";

/// Primary-key column names per replicated table (the schemas live in
/// `store.rs`; this list is the trigger/applier contract).
pub(crate) fn table_pk_cols(tbl: &str) -> Vec<&'static str> {
    match tbl {
        "sessions" => vec!["id"],
        "messages" => vec!["msg_id"],
        "usage" => vec!["usage_id"],
        "checkpoints" => vec!["id"],
        "irc_lines" => vec!["id"],
        "session_overrides" => vec!["session_id"],
        "session_locale" => vec!["session_id"],
        "todos" => vec!["session_id", "item_id"],
        "session_queue" => vec!["session_id", "seq"],
        "engine_notes" => vec!["id"],
        "skill_uses" => vec!["id"],
        "session_prompt" => vec!["session_id"],
        "session_takeovers" => vec!["session_id"],
        _ => vec![],
    }
}

/// A column list for `tbl` from a live connection, schema order.
pub(crate) fn table_columns(conn: &Connection, tbl: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{tbl}\")"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

/// `json_object('col', NEW."col", …)` over the given columns — the row
/// image the log stores. Columns are named because SQLite has no
/// `to_json(NEW)`; the cost is that a column change regenerates the
/// triggers, which the sha check turns into a non-event.
fn row_object_expr(cols: &[String], ref_row: &str) -> String {
    let inner = cols
        .iter()
        .map(|c| format!("'{c}', {ref_row}.\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("json_object({inner})")
}

/// The full trigger SQL for every replicated table, generated from the
/// live column lists. Deterministic for a schema; its sha is the
/// installed-triggers version.
fn generate_trigger_sql(conn: &Connection) -> Result<String> {
    let mut out = String::new();
    for tbl in REPLICATED_TABLES {
        let cols = table_columns(conn, tbl)?;
        anyhow::ensure!(
            !cols.is_empty(),
            "replicated table {tbl} does not exist — schema drift?"
        );
        let pk: Vec<String> = table_pk_cols(tbl)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        anyhow::ensure!(!pk.is_empty(), "no pk columns known for {tbl}");
        for (op, op_word, ref_row) in [
            ("I", "INSERT", "NEW"),
            ("U", "UPDATE", "NEW"),
            ("D", "DELETE", "OLD"),
        ] {
            // A DELETE logs the OLD image under the OLD pk values.
            let src = if op == "D" { "OLD" } else { ref_row };
            out.push_str(&format!(
                "CREATE TRIGGER IF NOT EXISTS store_log_{tbl}_{op}
                 AFTER {op_word} ON \"{tbl}\" BEGIN
                   INSERT INTO store_log(at, tbl, op, pk, row_json)
                   VALUES (STRFTIME('%Y-%m-%dT%H:%M:%fZ','now'), '{tbl}', '{op}',
                           {pk_obj}, {row_obj});
                 END;\n",
                pk_obj = row_object_expr(&pk, src),
                row_obj = row_object_expr(&cols, src),
            ));
        }
    }
    Ok(out)
}

/// Install `store_log` + the AFTER triggers on every replicated table,
/// and stamp this store's owning node id ([`OWNER_META_KEY`]) if it has
/// none. Idempotent; recreates the triggers when the generated SQL's
/// sha differs from the installed version (a replicated table's columns
/// changed). Callers: the gateway at boot (best-effort, so the log
/// accumulates), and the `veles replicas` family.
pub fn install_triggers(store: &Store) -> Result<String> {
    let conn = store.conn();
    conn.execute_batch(STORE_LOG_SQL)?;
    stamp_owner(store)?;
    let sql = generate_trigger_sql(conn)?;
    let sha = crate::memory::MemoryStore::content_sha(&sql);
    let installed: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![TRIGGERS_SHA_META_KEY],
            |r| r.get(0),
        )
        .ok();
    if installed.as_deref() == Some(sha.as_str()) {
        return Ok("unchanged".into());
    }
    drop_log_triggers(conn);
    let bare = sql.replace("CREATE TRIGGER IF NOT EXISTS", "CREATE TRIGGER");
    conn.execute_batch(&bare)?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![TRIGGERS_SHA_META_KEY, sha],
    )?;
    Ok(sha)
}

/// The owner stamp: the node id this store belongs to, written once. A
/// replica never runs this (its rows arrive by apply, which sets its own
/// markers), so presence of the stamp without the replica marker IS the
/// primary's identity — `session_owner` answers from it. It is identity,
/// not replication: every gateway stamps it, log or no log.
pub fn stamp_owner(store: &Store) -> Result<String> {
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?.id;
    store.conn().execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO NOTHING",
        rusqlite::params![OWNER_META_KEY, me],
    )?;
    Ok(me)
}

/// Tables that USED to replicate: their triggers are dropped whenever the
/// log is (re)installed or removed, so a store that ran an older build
/// stops logging them.
const RETIRED_LOG_TABLES: [&str; 1] = ["session_runs"];

fn drop_log_triggers(conn: &Connection) {
    for tbl in REPLICATED_TABLES.iter().chain(RETIRED_LOG_TABLES.iter()) {
        for op in ["I", "U", "D"] {
            let _ = conn.execute(&format!("DROP TRIGGER IF EXISTS store_log_{tbl}_{op}"), []);
        }
    }
}

/// Remove the log: the triggers, the rows, the installed-version stamp.
/// A node with nobody to replicate to must not pay for replication — the
/// triggers copy every message into `store_log` in full, and nothing
/// would ever read or prune those rows. The owner stamp stays (it is
/// identity, not replication). Returns whether a log was there.
pub fn uninstall_log(store: &Store) -> Result<bool> {
    let conn = store.conn();
    let had: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'store_log')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    drop_log_triggers(conn);
    if had {
        conn.execute("DELETE FROM store_log", [])?;
    }
    conn.execute(
        "DELETE FROM meta WHERE key = ?1",
        rusqlite::params![TRIGGERS_SHA_META_KEY],
    )?;
    Ok(had)
}

/// The log follows the config: installed while `[cluster]
/// storesync_peers` names anyone, removed when it names nobody. The
/// gateway calls this at boot (2026-09-28's B5 installed it on EVERY
/// gateway, so a single node copied each message it stored into the log
/// forever).
///
/// Turning replication off and on again leaves a hole the replicas
/// cannot see (AUTOINCREMENT keeps the seq climbing, so it is not a
/// gap): the writes made while it was off reach them only as far as the
/// fresh install's backfill re-sends them — the sessions active in the
/// last `backfill_days`.
pub fn sync_log_install(
    store: &Store,
    storesync_peers: &[String],
    backfill_days: u32,
) -> Result<String> {
    stamp_owner(store)?;
    if storesync_peers.is_empty() {
        return Ok(if uninstall_log(store)? {
            "removed (no storesync_peers)".into()
        } else {
            "off (no storesync_peers)".into()
        });
    }
    install_log(store, backfill_days)
}

/// Install the log; when it is FRESH (never installed, or removed since)
/// also [`backfill`] the last `backfill_days` of sessions. The triggers
/// see writes from now on only, and a replica holding a session's tail
/// without its start is no base for a takeover.
pub fn install_log(store: &Store, backfill_days: u32) -> Result<String> {
    let fresh = meta_get(store.conn(), TRIGGERS_SHA_META_KEY).is_none();
    let state = install_triggers(store)?;
    if !fresh || backfill_days == 0 {
        return Ok(state);
    }
    let r = backfill(store, backfill_days)?;
    Ok(format!(
        "{state}; backfilled {} row(s) of {} session(s) active in the last {} day(s)",
        r.rows, r.sessions, r.days
    ))
}

/// What a [`backfill`] put in the log.
#[derive(Debug, Default, Serialize)]
pub struct BackfillReport {
    pub days: u32,
    /// Sessions active in the window.
    pub sessions: usize,
    /// Log rows written for them.
    pub rows: usize,
}

/// The column naming a row's session in each replicated table — what a
/// backfill selects on. IRC lines belong to a network buffer, not a
/// session: a backfill leaves them to the live log.
fn session_col(tbl: &str) -> Option<&'static str> {
    match tbl {
        "sessions" => Some("id"),
        "irc_lines" => None,
        _ => Some("session_id"),
    }
}

/// Put the current rows of every session active in the last `days`
/// (`sessions.updated_at`, which each stored message moves) into the log
/// as inserts, so the next push carries them whole: sessions first, then
/// their rows in every replicated table. A replica that already holds a
/// row just replaces it with the same image. Needs the log (refused when
/// no storesync peer is configured); `days = 0` writes nothing.
pub fn backfill(store: &Store, days: u32) -> Result<BackfillReport> {
    let conn = store.conn();
    anyhow::ensure!(
        meta_get(conn, TRIGGERS_SHA_META_KEY).is_some(),
        "no store log on this node — `[cluster] storesync_peers` names nobody"
    );
    let mut report = BackfillReport {
        days,
        ..Default::default()
    };
    if days == 0 {
        return Ok(report);
    }
    let window = format!("-{days} days");
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        report.sessions = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE updated_at >= datetime('now', ?1)",
            [&window],
            |r| r.get::<_, i64>(0),
        )? as usize;
        for tbl in REPLICATED_TABLES {
            let Some(key) = session_col(tbl) else {
                continue;
            };
            let cols = table_columns(conn, tbl)?;
            let pk: Vec<String> = table_pk_cols(tbl).into_iter().map(String::from).collect();
            let order = pk
                .iter()
                .map(|c| format!("t.\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO store_log(at, tbl, op, pk, row_json)
                 SELECT STRFTIME('%Y-%m-%dT%H:%M:%fZ','now'), '{tbl}', 'I', {pk_obj}, {row_obj}
                 FROM \"{tbl}\" AS t
                 WHERE t.\"{key}\" IN
                     (SELECT id FROM sessions WHERE updated_at >= datetime('now', ?1))
                 ORDER BY {order}",
                pk_obj = row_object_expr(&pk, "t"),
                row_obj = row_object_expr(&cols, "t"),
            );
            report.rows += conn.execute(&sql, [&window])?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(report)
}

/// Meta key prefix for the highest replica cursor a peer has REPORTED
/// applying (the "(replica cursor N)" in its replies). Distinct from the
/// push cursor: a stored offer can still be refused, and a gap heal asks
/// for rows from the applied cursor on — so only applied rows are safe
/// to prune.
pub const PEER_ACKED_PREFIX: &str = "veles.storesync.acked.";

/// Record what `peer_name` reports having applied. Monotone.
pub fn record_acked(store: &Store, peer_name: &str, applied: i64) {
    let key = format!("{PEER_ACKED_PREFIX}{peer_name}");
    let cur: i64 = meta_get(store.conn(), &key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if applied > cur {
        let _ = meta_put(store.conn(), &key, &applied.to_string());
    }
}

/// Rows [`prune_log`] deletes per call. The caller takes the store lock
/// for each batch and releases it in between. One DELETE of the whole
/// acked tail held that lock for up to 17 s on a busy disk, because
/// freeing a large row reads its overflow pages.
pub const PRUNE_BATCH: usize = 200;

/// Delete up to `limit` of the log rows every storesync peer has
/// applied, oldest first. Nothing is pruned while any peer has never
/// reported a cursor — its replica may still need everything. Returns
/// the rows removed; fewer than `limit` means none prunable are left.
pub fn prune_log(store: &Store, storesync_peers: &[String], limit: usize) -> Result<usize> {
    let mut floor = i64::MAX;
    for p in storesync_peers {
        match meta_get(store.conn(), &format!("{PEER_ACKED_PREFIX}{p}"))
            .and_then(|v| v.parse::<i64>().ok())
        {
            Some(n) => floor = floor.min(n),
            None => return Ok(0),
        }
    }
    if storesync_peers.is_empty() || floor <= 0 {
        return Ok(0);
    }
    Ok(store.conn().execute(
        "DELETE FROM store_log WHERE seq IN
           (SELECT seq FROM store_log WHERE seq <= ?1 ORDER BY seq LIMIT ?2)",
        rusqlite::params![floor, i64::try_from(limit).unwrap_or(i64::MAX)],
    )?)
}

/// The store's owner node id, if stamped (a primary stamped by
/// [`install_triggers`]; a replica carries the mirrored node's id).
pub fn owner_of(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![OWNER_META_KEY],
        |r| r.get(0),
    )
    .ok()
}

/// Does this connection carry the replica marker (a peer's store image)?
pub fn is_replica(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![REPLICA_OF_META_KEY],
        |r| r.get::<_, String>(0),
    )
    .is_ok()
}

// ── the offer ───────────────────────────────────────────────────────

/// One log row as it travels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreLogRow {
    pub seq: i64,
    pub at: String,
    pub tbl: String,
    pub op: String,
    pub pk: String,
    pub row_json: String,
}

/// A store offer: log rows past a cursor, plus the sender's identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreOffer {
    pub from_node: String,
    pub from_name: String,
    /// The cursor the sender diffed from (informational).
    pub since_seq: i64,
    pub rows: Vec<StoreLogRow>,
    /// Part numbering for chunked transport (0/0 = single).
    pub part: u32,
    pub parts: u32,
    /// The sender's log no longer holds seqs at or below this (0 = it
    /// holds everything asked for). Set only when the offer starts below
    /// it: the replica may skip to `base`, since nothing can fill the
    /// range — see [`log_floor`].
    #[serde(default)]
    pub base: i64,
}

/// Read log rows past `since_seq`, at most `limit` (bounded offers).
pub fn log_rows(store: &Store, since_seq: i64, limit: u64) -> Result<Vec<StoreLogRow>> {
    let mut stmt = store.conn().prepare(LOG_ROWS_SQL)?;
    let rows = stmt
        .query_map(rusqlite::params![since_seq, limit], log_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

const LOG_ROWS_SQL: &str = "SELECT seq, at, tbl, op, pk, row_json FROM store_log
     WHERE seq > ?1 ORDER BY seq LIMIT ?2";

fn log_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoreLogRow> {
    Ok(StoreLogRow {
        seq: r.get(0)?,
        at: r.get(1)?,
        tbl: r.get(2)?,
        op: r.get(3)?,
        pk: r.get(4)?,
        row_json: r.get(5)?,
    })
}

/// The highest `store_log.seq` (the cursor a receiver reports back).
pub fn max_seq(store: &Store) -> Result<i64> {
    let n: Option<i64> = store
        .conn()
        .query_row("SELECT MAX(seq) FROM store_log", [], |r| r.get(0))?;
    Ok(n.unwrap_or(0))
}

/// The highest seq the log no longer holds. Rows at or below it were
/// pruned (every peer then known had applied them) or never kept (the
/// log was off), so a replica behind it — a peer added later, a wiped
/// one — cannot be filled from the log. It skips to the floor instead,
/// and [`push_incremental_to_peer`] backfills the recent sessions for it.
pub fn log_floor(store: &Store) -> Result<i64> {
    let conn = store.conn();
    let min: Option<i64> = conn.query_row("SELECT MIN(seq) FROM store_log", [], |r| r.get(0))?;
    if let Some(m) = min {
        return Ok(m - 1);
    }
    // An empty log: every seq ever issued is gone.
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'store_log'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0))
}

/// Build an offer of everything past `since_seq`, capped at
/// [`OFFER_ROW_LIMIT`] rows and [`OFFER_BYTE_BUDGET`] bytes of row
/// images (at least one row) — a bigger delta is chunked by the caller
/// walking the cursor until an offer comes back empty, or moved with
/// `veles replicas offer` + a file.
pub fn build_offer(
    store: &Store,
    from_node: &str,
    from_name: &str,
    since_seq: i64,
) -> Result<StoreOffer> {
    // Read only as far as the byte budget, always at least one row. This
    // loaded all OFFER_ROW_LIMIT rows (up to 60 KB each) with the store
    // lock held, and then kept the first 768 KB of them.
    let mut rows = Vec::new();
    {
        let mut stmt = store.conn().prepare(LOG_ROWS_SQL)?;
        let mut found = stmt.query(rusqlite::params![since_seq, OFFER_ROW_LIMIT])?;
        let mut bytes = 0usize;
        while let Some(r) = found.next()? {
            let row = log_row(r)?;
            bytes += row.row_json.len();
            if bytes > OFFER_BYTE_BUDGET && !rows.is_empty() {
                break;
            }
            rows.push(row);
        }
    }
    let floor = log_floor(store)?;
    Ok(StoreOffer {
        from_node: from_node.to_string(),
        from_name: from_name.to_string(),
        since_seq,
        rows,
        part: 0,
        parts: 0,
        base: if since_seq < floor { floor } else { 0 },
    })
}

// ── the receiver: the replica file ──────────────────────────────────

/// Rows per offer chunk; the cursor makes chunk walking exact.
pub const OFFER_ROW_LIMIT: u64 = 500;

/// Bytes of row images per offer chunk, memsync's chunk size. 500 rows
/// is not a size: the 2026-10-03 batch translation run wrote rows up to
/// 60 KB, a 500-row chunk passed 2 MB once base64'd, and an A2A server
/// on axum's default body limit answered 413 in plain text — which the
/// sender read as "error decoding response body", on every tick.
pub const OFFER_BYTE_BUDGET: usize = 768 * 1024;

/// A search hit from a replica, tagged with the source node.
#[derive(Debug, Serialize)]
pub struct ReplicaHit {
    pub node: String,
    pub msg_id: i64,
    pub session_id: String,
    pub role: String,
    pub snippet: String,
    pub created_at: String,
}

/// What an apply did.
#[derive(Debug, Default, Serialize)]
pub struct ApplyReport {
    pub replica: String,
    pub applied_rows: usize,
    pub skipped_uptodate: usize,
    pub deleted_rows: usize,
    pub columns_added: Vec<String>,
    pub withheld_secret_rows: usize,
    pub final_seq: i64,
    /// The cursor this replica skipped FROM, when the sender's log no
    /// longer held the rows after it ([`StoreOffer::base`]).
    pub rebased_from: Option<i64>,
}

/// The replica schema: the replicated tables with their live shapes
/// (store.rs is the source of truth; these are the CREATEs it ships,
/// minus everything node-local), plus `messages`' FTS index and its
/// triggers so the replica is SEARCHABLE — the point of B5's read
/// surface.
///
/// The FTS row of a message is keyed by its `msg_id` (the FTS rowid).
/// Every apply is an `INSERT OR REPLACE`, whose replace fires no DELETE
/// trigger, so an index keyed any other way gained a second entry each
/// time a message's row arrived again (an update, a backfill, a rebase)
/// and search listed it twice. Keyed by msg_id, the insert trigger runs
/// under the apply's own OR REPLACE (SQLite hands the outer statement's
/// conflict rule to the trigger's statements) and replaces the entry.
const REPLICA_SCHEMA_SQL: &str = r#"
    PRAGMA journal_mode = WAL;
    CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        title TEXT,
        parent TEXT,
        node TEXT,
        created_at TEXT DEFAULT (datetime('now')),
        updated_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS messages (
        msg_id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        reasoning TEXT,
        tool_calls TEXT,
        tool_call_id TEXT,
        tool_name TEXT,
        injection TEXT,
        created_at TEXT DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, msg_id);
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        role, content, session_id UNINDEXED, msg_id UNINDEXED
    );
    CREATE TRIGGER IF NOT EXISTS messages_fts_ai
    AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, role, content, session_id, msg_id)
        VALUES (new.msg_id, new.role, new.content, new.session_id, new.msg_id);
    END;
    CREATE TRIGGER IF NOT EXISTS messages_fts_ad
    AFTER DELETE ON messages BEGIN
        DELETE FROM messages_fts WHERE rowid = old.msg_id;
    END;
    CREATE TABLE IF NOT EXISTS usage (
        usage_id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL,
        subject TEXT,
        provider TEXT NOT NULL,
        model TEXT NOT NULL,
        prompt_tokens INTEGER NOT NULL DEFAULT 0,
        completion_tokens INTEGER NOT NULL DEFAULT 0,
        cached_tokens INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cache_reported INTEGER,
        reasoning_tokens INTEGER,
        created_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS checkpoints (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL,
        path TEXT NOT NULL,
        blob TEXT,
        sha256 TEXT,
        bytes INTEGER NOT NULL DEFAULT 0,
        tool TEXT NOT NULL,
        note TEXT,
        created_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS irc_lines (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        network TEXT NOT NULL,
        presence TEXT NOT NULL,
        buffer TEXT NOT NULL,
        ts TEXT NOT NULL,
        kind TEXT NOT NULL,
        nick TEXT NOT NULL DEFAULT '',
        account TEXT,
        text TEXT NOT NULL DEFAULT '',
        own INTEGER NOT NULL DEFAULT 0,
        seq INTEGER NOT NULL DEFAULT 0,
        blocked INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS todos (
        session_id TEXT NOT NULL,
        item_id TEXT NOT NULL,
        content TEXT NOT NULL,
        status TEXT NOT NULL,
        position INTEGER NOT NULL,
        updated_at TEXT DEFAULT (datetime('now')),
        PRIMARY KEY (session_id, item_id)
    );
    CREATE TABLE IF NOT EXISTS session_overrides (
        session_id TEXT PRIMARY KEY,
        provider TEXT,
        model TEXT,
        reasoning TEXT,
        updated_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS session_locale (
        session_id TEXT PRIMARY KEY,
        locale TEXT NOT NULL,
        updated_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS session_queue (
        session_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        body TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'pending',
        queued_at TEXT NOT NULL DEFAULT (datetime('now')),
        kind TEXT NOT NULL DEFAULT 'message',
        reply_to TEXT,
        route TEXT,
        PRIMARY KEY (session_id, seq)
    );
    CREATE TABLE IF NOT EXISTS engine_notes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL,
        after_msg_id INTEGER NOT NULL,
        kind TEXT NOT NULL,
        summary TEXT NOT NULL,
        detail TEXT,
        toggle TEXT,
        created_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS skill_uses (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        session_id TEXT NOT NULL,
        at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS session_prompt (
        session_id TEXT PRIMARY KEY,
        system_prompt TEXT NOT NULL,
        tools_json TEXT NOT NULL DEFAULT '',
        format INTEGER NOT NULL,
        node TEXT,
        updated_at TEXT DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS session_takeovers (
        session_id TEXT PRIMARY KEY,
        from_node TEXT NOT NULL,
        to_node TEXT NOT NULL,
        last_source_msg_id INTEGER NOT NULL,
        authorized_by TEXT NOT NULL,
        at TEXT NOT NULL
    );
"#;

/// Meta key: the replica's FTS index is keyed by msg_id (see
/// [`REPLICA_SCHEMA_SQL`]). A replica made before that has the old
/// trigger and duplicate entries; [`open_replica`] rebuilds it once.
const REPLICA_FTS_META_KEY: &str = "veles.replica_fts";

fn rekey_replica_fts(conn: &Connection) -> Result<()> {
    if meta_get(conn, REPLICA_FTS_META_KEY).as_deref() == Some("msg_id") {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS messages_ai;
         DELETE FROM messages_fts;
         INSERT INTO messages_fts(rowid, role, content, session_id, msg_id)
             SELECT msg_id, role, content, session_id, msg_id FROM messages;",
    )?;
    meta_put(&tx, REPLICA_FTS_META_KEY, "msg_id")?;
    tx.commit()?;
    Ok(())
}

/// Is `id` usable as a node id — and so as a replica FILE NAME? Node ids
/// are UUIDs; the rule is wider (tests and hand-named nodes) but never
/// admits a path: no separator, no leading dot, nothing but
/// `[A-Za-z0-9_.-]`. An offer's `from_node` is the sender's own claim,
/// and it used to be joined onto `replicas/` as-is — `../state` named
/// the node's own state.db.
pub fn valid_node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A SQL identifier that arrived in an offer (a row key, a pk key).
/// Everything spliced into SQL text from a peer goes through here:
/// quoting alone is not enough — a `"` in the name closes the quote.
fn wire_ident(name: &str) -> Result<&str> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    anyhow::ensure!(
        ok,
        "refused: column name {name:?} in the offer is not a plain identifier"
    );
    Ok(name)
}

/// Meta key prefix binding an authenticated A2A peer name to the node id
/// its store offers speak for (first use), when gossip has not bound it.
const SENDER_BINDING_PREFIX: &str = "veles.storesync.sender.";

/// The node `peer`'s store offers were bound to on first use (see
/// [`check_sender`]), if any — evidence of which node a peer is when
/// gossip has none (`pool::member_node`).
pub fn sender_binding(store: &Store, peer: &str) -> Option<String> {
    meta_get(store.conn(), &format!("{SENDER_BINDING_PREFIX}{peer}"))
}

/// May `peer` — the AUTHENTICATED A2A name — deliver offers that claim to
/// come from `from_node`? The claim is the offer's own text and it picks
/// which replica file gets written, so it has to be bound to the peer:
///
/// - gossip's binding when there is one (the node that peer's
///   observations are about — `Store::node_bound_to_peer`);
/// - otherwise the first node this peer ever spoke for, recorded here;
/// - a node already bound to a DIFFERENT peer is refused, so one peer
///   cannot overwrite another node's replica;
/// - this node's own id is never a sender.
pub fn check_sender(store: &Store, peer: &str, from_node: &str, me: &str) -> Result<()> {
    anyhow::ensure!(
        valid_node_id(from_node),
        "the offer claims to come from {from_node:?}, which is not a node id"
    );
    anyhow::ensure!(
        from_node != me,
        "the offer claims to come from THIS node ({me})"
    );
    if let Some(bound) = store.node_bound_to_peer(peer, me)? {
        anyhow::ensure!(
            bound == from_node,
            "peer '{peer}' is node {bound} (gossip), but its offer claims {from_node}"
        );
        return Ok(());
    }
    let key = format!("{SENDER_BINDING_PREFIX}{peer}");
    if let Some(bound) = meta_get(store.conn(), &key) {
        anyhow::ensure!(
            bound == from_node,
            "peer '{peer}' speaks for node {bound} (bound on its first offer), but this \
             offer claims {from_node}"
        );
        return Ok(());
    }
    let holder: Option<String> = store
        .conn()
        .query_row(
            "SELECT key FROM meta WHERE key LIKE ?1 AND value = ?2 LIMIT 1",
            rusqlite::params![format!("{SENDER_BINDING_PREFIX}%"), from_node],
            |r| r.get(0),
        )
        .ok();
    if let Some(k) = holder {
        anyhow::bail!(
            "node {from_node} already speaks through peer '{}' — '{peer}' may not write its replica",
            k.trim_start_matches(SENDER_BINDING_PREFIX)
        );
    }
    meta_put(store.conn(), &key, from_node)?;
    Ok(())
}

/// Where replicas live: `<veles_home>/replicas/<node-id>.db`.
pub fn replica_path(from_node: &str) -> std::path::PathBuf {
    crate::config::veles_home()
        .join("replicas")
        .join(format!("{from_node}.db"))
}

pub(crate) fn meta_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get(0),
    )
    .ok()
}

pub(crate) fn meta_put(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

// ── the auto-push cursor (B5 tail) ──────────────────────────────────
//
// Auto-push needs to know where each peer's replica stands WITHOUT
// trusting the peer to answer (it may be down). The sender keeps a
// per-peer cursor in its OWN meta table — meta is not replicated
// (never in REPLICATED_TABLES), so keeping bookkeeping there adds no
// log noise. The cursor is a lower bound: auto-push re-reads it after
// every stored chunk, so a hand `push --since` between ticks is seen
// at the next one, never re-sent.

/// Meta key prefix for the auto-push cursor of peer `name`
/// (`[a2a_agents]` name, the spelling the config lists).
pub const PEER_CURSOR_PREFIX: &str = "veles.storesync.cursor.";

/// The auto-push cursor this primary holds for `peer_name` (0 = none:
/// everything must go).
pub fn peer_cursor(store: &Store, peer_name: &str) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![format!("{PEER_CURSOR_PREFIX}{peer_name}")],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Bump (never lower) the auto-push cursor for `peer_name`. Monotone:
/// a lower write is ignored — a stale tick can't undo progress a later
/// one already recorded.
pub fn advance_peer_cursor(store: &Store, peer_name: &str, to: i64) {
    if to <= peer_cursor(store, peer_name) {
        return;
    }
    let _ = store.conn().execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![format!("{PEER_CURSOR_PREFIX}{peer_name}"), to.to_string()],
    );
}

/// Set the auto-push cursor for `peer_name` to an EXACT value (heal
/// path only: adopting the receiver's reported replica cursor, which
/// may be lower than ours). Not monotone by design — that is the point.
pub fn set_peer_cursor(store: &Store, peer_name: &str, to: i64) {
    let _ = store.conn().execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![format!("{PEER_CURSOR_PREFIX}{peer_name}"), to.to_string()],
    );
}

/// The applied cursor of this node's replica of `from_node` (0 when no
/// replica exists yet). Reported back to pushers in the store reply so
/// a sender can tell STORED from APPLIED — the two drift apart
/// whenever an apply refuses while the chunk sits stored in the inbox.
pub fn replica_cursor(from_node: &str) -> i64 {
    (|| -> Result<i64> {
        let conn = open_replica(from_node)?;
        Ok(meta_get(&conn, APPLIED_SEQ_META_KEY)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    })()
    .unwrap_or(0)
}

/// The replica cursor a peer reported in its store reply (the
/// "(replica cursor N)" suffix). None = the peer said nothing parseable.
pub fn replica_cursor_from_reply(reply: &str) -> Option<i64> {
    let idx = reply.rfind("(replica cursor ")?;
    let rest = &reply[idx + "(replica cursor ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Open (creating with the replica schema if absent) the replica of
/// `from_node`, stamping its markers. The replica carries
/// [`REPLICA_OF_META_KEY`] AND [`OWNER_META_KEY`] = the mirrored node —
/// a replica of A answers "who owns these rows" with A, which is the
/// property B6's fence builds on.
pub fn open_replica(from_node: &str) -> Result<Connection> {
    anyhow::ensure!(
        valid_node_id(from_node),
        "{from_node:?} is not a node id — no replica file is opened for it"
    );
    let path = replica_path(from_node);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let conn =
        Connection::open(&path).with_context(|| format!("open replica {}", path.display()))?;
    conn.execute_batch(REPLICA_SCHEMA_SQL)?;
    meta_put(&conn, REPLICA_OF_META_KEY, from_node)?;
    meta_put(&conn, OWNER_META_KEY, from_node)?;
    rekey_replica_fts(&conn)?;
    Ok(conn)
}

/// One session another node owns, as its replica here holds it.
#[derive(Debug, Clone)]
pub struct ReplicaSession {
    /// The owning node's id (the replica's file stem).
    pub node: String,
    pub row: crate::store::SessionRow,
}

/// Every replica held here, opened READ-ONLY, ordered by node. A replica
/// that will not open is skipped: a read surface must not fail the list.
pub fn replicas_ro() -> Vec<(String, crate::store::Store)> {
    let dir = crate::config::veles_home().join("replicas");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "db") {
            continue;
        }
        let Some(node) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !valid_node_id(node) {
            continue;
        }
        match crate::store::Store::open_replica_ro(&p) {
            Ok(s) => out.push((node.to_string(), s)),
            Err(e) => tracing::debug!("replica {} unreadable: {e:#}", p.display()),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Every replica session matching `q` — ALL matches; the caller pages
/// the merged list — minus the ones `is_local` says this node holds
/// itself (taken over or migrated here: the local copy is the live one).
pub fn replica_session_rows(
    q: &crate::store::SessionQuery,
    is_local: impl Fn(&str) -> bool,
) -> Vec<ReplicaSession> {
    let mut all = q.clone();
    all.offset = 0;
    all.limit = 100_000;
    let mut out = Vec::new();
    for (node, store) in replicas_ro() {
        match store.session_page(&all) {
            Ok((rows, _)) => out.extend(rows.into_iter().filter(|r| !is_local(&r.id)).map(|row| {
                ReplicaSession {
                    node: node.clone(),
                    row,
                }
            })),
            Err(e) => tracing::debug!("replica {node}: listing failed: {e:#}"),
        }
    }
    out
}

/// The replica holding `session`, opened read-only — for a session this
/// node has no copy of its own.
pub fn replica_holding(session: &str) -> Option<(String, crate::store::Store)> {
    replicas_ro()
        .into_iter()
        .find(|(_, s)| s.session_exists(session).unwrap_or(false))
}

/// serde JSON value → a bindable SQLite value. JSON has no dates or
/// blobs; everything here is Null/bool/number/string.
fn json_to_sql(v: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sv;
    use serde_json::Value as Jv;
    match v {
        Jv::Null => Sv::Null,
        Jv::Bool(b) => Sv::Integer(*b as i64),
        Jv::Number(n) => {
            if let Some(i) = n.as_i64() {
                Sv::Integer(i)
            } else {
                Sv::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        Jv::String(s) => Sv::Text(s.clone()),
        // Arrays/objects never appear as column values in the
        // replicated tables (tool_calls/injection etc. are TEXT).
        other => Sv::Text(other.to_string()),
    }
}

/// ALTER the replica table for row keys it lacks (a newer sender's
/// column), returning the DDL run. Type comes from the JSON value.
fn ensure_columns(conn: &Connection, tbl: &str, row: &serde_json::Value) -> Result<Vec<String>> {
    let mut added = Vec::new();
    let existing: std::collections::BTreeSet<String> =
        table_columns(conn, tbl)?.into_iter().collect();
    for (k, v) in row.as_object().into_iter().flatten() {
        let k = wire_ident(k)?;
        if existing.contains(k) {
            continue;
        }
        let ty = match v {
            serde_json::Value::Number(n) if n.is_i64() => "INTEGER",
            serde_json::Value::Number(_) => "REAL",
            serde_json::Value::Bool(_) => "INTEGER",
            _ => "TEXT",
        };
        // `execute`, never `execute_batch`: one statement, whatever the
        // text turns out to hold.
        let ddl = format!("ALTER TABLE \"{tbl}\" ADD COLUMN \"{k}\" {ty}");
        conn.execute(&ddl, [])
            .with_context(|| format!("drift-fix {ddl}"))?;
        added.push(ddl);
    }
    Ok(added)
}

/// Apply an offer into the replica of its sender. One transaction; the
/// cursor is enforced GAPLESS (`applied+1` or refuse — a hole means a
/// re-push from zero, never a silently-stitchable history); rows at or
/// below the cursor count as up-to-date skips (a re-pushed chunk is a
/// no-op, so transport retries are safe); rows naming a table outside
/// [`REPLICATED_TABLES`] are refused — memory in particular is memsync's
/// and must never ride this path.
pub fn apply_offer(conn: &Connection, offer: &StoreOffer) -> Result<ApplyReport> {
    // The fold ban: an offer applies only into the replica of the node
    // it comes from — never into the primary (which carries no
    // replica marker) and never into another node's replica.
    let marker = meta_get(conn, REPLICA_OF_META_KEY);
    anyhow::ensure!(
        marker.as_deref() == Some(offer.from_node.as_str()),
        "refused: this store is not the replica of {} (it is {}) — replicas are \
         separate files, see open_replica",
        offer.from_node,
        marker.as_deref().unwrap_or("a primary")
    );
    let mut report = ApplyReport {
        replica: offer.from_node.clone(),
        ..Default::default()
    };
    let rows = &offer.rows;
    let tx = conn.unchecked_transaction()?;
    let applied_before: i64 = meta_get(&tx, APPLIED_SEQ_META_KEY)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut cursor = applied_before;
    if offer.base > applied_before {
        // The sender no longer has (applied_before, base]: nothing can
        // fill it, so a gap refusal would repeat forever. Skip to the
        // base. What changed in that range reaches this replica only as
        // the current images a backfill re-sends.
        cursor = offer.base;
        report.rebased_from = Some(applied_before);
    }
    let start = cursor;
    for row in rows.iter().filter(|r| r.seq > start) {
        anyhow::ensure!(
            REPLICATED_TABLES.contains(&row.tbl.as_str()),
            "log row {} names table {} — outside the replicated set; \
             memory travels by memsync, never here",
            row.seq,
            row.tbl
        );
        anyhow::ensure!(
            matches!(row.op.as_str(), "I" | "U" | "D"),
            "log row {} has op {:?}",
            row.seq,
            row.op
        );
        anyhow::ensure!(
            row.seq == cursor + 1,
            "gap at seq {} (replica cursor {cursor}) — re-push from zero: \
             `veles replicas push <peer-url>` rebuilds it",
            row.seq
        );
        // Withhold a credential-shaped row (configsync's defense-in-depth
        // rule; a hole is honest, a shipped key is not) — but its seq
        // still moves the cursor. Dropping it from the offer BEFORE this
        // loop read as a gap at the next seq, and every re-push carried
        // the same row: the laptop's replica on the VPS and the phone
        // froze at seq 1634 behind one tool result that quoted a bearer
        // token, and 200 refused offers filled both inboxes
        // (2026-10-02/04). A delete is never withheld: it acts by pk
        // alone, and skipping it would leave an older image standing.
        if row.op != "D" && crate::a2a::secret_shaped(&row.row_json) {
            report.withheld_secret_rows += 1;
            cursor = row.seq;
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(&row.row_json).context("log row image is not JSON")?;
        report
            .columns_added
            .extend(ensure_columns(&tx, &row.tbl, &v)?);
        if row.op == "D" {
            delete_by_pk(&tx, &row.tbl, &row.pk)?;
            report.deleted_rows += 1;
        } else {
            upsert_row(&tx, &row.tbl, &v)?;
            report.applied_rows += 1;
        }
        cursor = row.seq;
    }
    report.skipped_uptodate = rows.iter().filter(|r| r.seq <= applied_before).count();
    if cursor > applied_before {
        meta_put(&tx, APPLIED_SEQ_META_KEY, &cursor.to_string())?;
    }
    report.final_seq = cursor;
    tx.commit()?;
    Ok(report)
}

fn delete_by_pk(conn: &Connection, tbl: &str, pk_json: &str) -> Result<()> {
    let pk: serde_json::Value = serde_json::from_str(pk_json).context("log row pk is not JSON")?;
    let obj = pk
        .as_object()
        .with_context(|| format!("pk for {tbl} is not an object"))?;
    // The keys ARE the WHERE clause: exactly the table's pk columns, or
    // a delete could be widened past the one row it names.
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    let mut want = table_pk_cols(tbl);
    keys.sort_unstable();
    want.sort_unstable();
    anyhow::ensure!(
        keys == want,
        "refused: a delete on {tbl} keyed by {keys:?} — its primary key is {want:?}"
    );
    let mut where_clauses = Vec::new();
    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
    for (k, v) in obj {
        where_clauses.push(format!("\"{k}\" = ?{}", bindings.len() + 1));
        bindings.push(json_to_sql(v));
    }
    anyhow::ensure!(!where_clauses.is_empty(), "empty pk for {tbl} delete");
    let sql = format!(
        "DELETE FROM \"{tbl}\" WHERE {}",
        where_clauses.join(" AND ")
    );
    conn.execute(&sql, rusqlite::params_from_iter(bindings))?;
    Ok(())
}

fn upsert_row(conn: &Connection, tbl: &str, v: &serde_json::Value) -> Result<()> {
    let obj = v
        .as_object()
        .with_context(|| format!("row image for {tbl} is not an object"))?;
    let cols: Vec<&String> = obj.keys().collect();
    anyhow::ensure!(!cols.is_empty(), "empty row image for {tbl}");
    for c in &cols {
        wire_ident(c)?;
    }
    let col_sql = cols
        .iter()
        .map(|c| format!("\"{}\"", c))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=cols.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT OR REPLACE INTO \"{tbl}\" ({col_sql}) VALUES ({placeholders})");
    let bindings: Vec<rusqlite::types::Value> = obj.values().map(json_to_sql).collect();
    conn.execute(&sql, rusqlite::params_from_iter(bindings))?;
    Ok(())
}

// ── read surface ────────────────────────────────────────────────────

/// Every replica file under `<veles_home>/replicas/`, with its cursor
/// and row counts — `veles replicas list`.
pub fn list_replicas() -> Result<Vec<ReplicaInfo>> {
    let dir = crate::config::veles_home().join("replicas");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "db") {
            let Some(node) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let conn = Connection::open(&path)?;
            let applied = meta_get(&conn, APPLIED_SEQ_META_KEY)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let sessions: i64 = conn
                .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
                .unwrap_or(0);
            let messages: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap_or(0);
            out.push(ReplicaInfo {
                node: node.to_string(),
                applied_seq: applied,
                sessions,
                messages,
                bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| a.node.cmp(&b.node));
    Ok(out)
}

/// One replica's summary for `veles replicas list`.
#[derive(Debug, Serialize)]
pub struct ReplicaInfo {
    pub node: String,
    pub applied_seq: i64,
    pub sessions: i64,
    pub messages: i64,
    pub bytes: u64,
}

/// FTS search across every replica, hits tagged with the source node —
/// the B5 read surface: history stays findable when the owner is down.
pub fn search_replicas(query: &str, limit: u32) -> Result<Vec<ReplicaHit>> {
    let quoted = format!("\"{}\"", query.replace('"', "\"\""));
    let mut out = Vec::new();
    for info in list_replicas()? {
        let conn = Connection::open(replica_path(&info.node))?;
        let node = info.node.clone();
        let mut stmt = conn.prepare(
            "SELECT m.msg_id, m.session_id, m.role,
                    snippet(messages_fts, 1, '>>>', '<<<', '…', 16), m.created_at
             FROM messages_fts
             JOIN messages m ON m.msg_id = messages_fts.msg_id
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![quoted, limit], |r| {
            Ok(ReplicaHit {
                node: node.clone(),
                msg_id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                snippet: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        out.extend(rows.filter_map(|h| h.ok()));
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out.truncate(limit as usize);
    Ok(out)
}

/// Which node owns `session_id` — the local store if it is there, else
/// the first replica that has it. `None` = nobody knows (the question
/// B6's fence needs answered before any node may refuse a session).
pub fn session_owner(store: &Store, session_id: &str) -> Result<Option<String>> {
    // A row that lives in THIS store is owned here — `sessions.node` is
    // metadata the drainer checks, not the existence test (NULL stamps
    // are the pre-ownership era, still local by construction).
    let has_local: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_local > 0 {
        return Ok(owner_of(store.conn()));
    }
    for info in list_replicas()? {
        let conn = Connection::open(replica_path(&info.node))?;
        let has: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has > 0 {
            return Ok(Some(info.node));
        }
    }
    Ok(None)
}

// ── transport: marker, fences, inbox ────────────────────────────────

/// The first line of a store-offer message.
pub const STORE_MARKER: &str = "[veles store sync offer]";
const FENCE_B64: &str = "```veles-storesync-b64";

/// Does this inbound A2A text carry a store offer (not a message for
/// the agent)? Checked by the A2A server before any turn.
pub fn is_store_offer(text: &str) -> bool {
    text.trim_start().starts_with(STORE_MARKER) && text.contains(FENCE_B64)
}

/// Encode one offer as a message: marker, summary, base64 + sha fence.
///
/// The payload is DEFLATE-compressed before it is base64'd (`enc:z`
/// marker): a 500-row chunk of `store_log` JSON is ~4.7× smaller
/// deflated — the phone's radio bill when a node rejoins after days
/// away. A receiver from before the marker cannot read it (it takes the
/// `enc:z` line for base64), so the push paths send through
/// [`call_offer`], which falls back to [`encode_message_as`]`(…, false)`
/// for such a peer.
pub fn encode_message(offer: &StoreOffer) -> Result<String> {
    encode_message_as(offer, true)
}

/// [`encode_message`], compressed or in the plain layout an older
/// receiver reads (base64 over the JSON, no `enc:` line).
pub fn encode_message_as(offer: &StoreOffer, compress: bool) -> Result<String> {
    let json = serde_json::to_string(offer)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    let part = if offer.parts > 1 {
        format!(" (part {}/{})", offer.part, offer.parts)
    } else {
        String::new()
    };
    Ok(format!(
        "{STORE_MARKER} {} rows past {}{part}\n\
         Stored in the receiver's storesync inbox; applied only by \
         `veles replicas apply --inbox`.\n\
         {FENCE_B64}\nsha256:{sha}\n{}\n```",
        offer.rows.len(),
        offer.since_seq,
        fence_payload(&json, compress)
    ))
}

/// The fence's body after its sha line: `enc:z` + base64 over the
/// DEFLATE stream, or base64 over the plain JSON. Shared with memsync
/// (the same fence layout).
pub(crate) fn fence_payload(json: &str, compress: bool) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    if compress {
        format!("enc:z\n{}", b64.encode(deflate(json.as_bytes())))
    } else {
        b64.encode(json.as_bytes())
    }
}

/// DEFLATE-compress `bytes` (pure-Rust miniz; no C toolchain).
pub(crate) fn deflate(bytes: &[u8]) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    use std::io::Write as _;
    // Vec's writer is infallible; a write error here is unreachable.
    let _ = enc.write_all(bytes);
    enc.finish().unwrap_or_else(|_| bytes.to_vec())
}

/// The most an `enc:z` payload may inflate to. An honest offer is one
/// [`OFFER_BYTE_BUDGET`] chunk, or a single row past it that fit the
/// peer's 32 MiB request; DEFLATE expands up to ~1000×, so without a cap
/// one small request from a peer could fill the receiver's memory.
const INFLATE_MAX: u64 = 256 << 20;

/// Undo [`deflate`]: the payload of a fence marked `enc:z`.
pub(crate) fn inflate(bytes: &[u8]) -> Result<Vec<u8>> {
    inflate_at_most(bytes, INFLATE_MAX)
}

fn inflate_at_most(bytes: &[u8], max: u64) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;
    let mut out = Vec::new();
    ZlibDecoder::new(bytes)
        .take(max + 1)
        .read_to_end(&mut out)
        .context("payload is marked enc:z but is not a DEFLATE stream")?;
    if out.len() as u64 > max {
        anyhow::bail!("payload inflates past {max} bytes");
    }
    Ok(out)
}

// ── older receivers ─────────────────────────────────────────────────

/// Peers (by URL) that refused a compressed offer. A node from before
/// `enc:z` reads the marker line as base64 and answers "payload is not
/// base64" — on 2026-10-04 the VPS, upgraded first, had every push to
/// the laptop refused that way while the laptop ran the older binary.
/// Such a peer is sent the plain layout for this process's life; a
/// restart tries compressed again, at the cost of one refused offer.
static PLAIN_PEERS: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Did the peer refuse an offer because it cannot read the compressed
/// layout? (Its decoder's error, verbatim — both storesync's and
/// memsync's say it.)
fn refused_compressed(result: &serde_json::Value) -> bool {
    result["status"]["state"].as_str() != Some(crate::a2a::STATE_COMPLETED)
        && crate::a2a::summarize_result(result).contains("payload is not base64")
}

/// Send one sync offer (storesync's or memsync's): compressed, unless
/// `peer_url` is known to read only the plain layout. A refusal of the
/// compressed form is sent again plain at once, and remembered.
/// `encode(compress)` renders the message.
pub(crate) async fn call_offer(
    client: &crate::a2a::A2AClient,
    peer_url: &str,
    bearer: Option<&str>,
    context_id: &str,
    encode: impl Fn(bool) -> Result<String>,
) -> Result<serde_json::Value> {
    let plain = PLAIN_PEERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(peer_url);
    let result = client
        .call(peer_url, bearer, &encode(!plain)?, context_id)
        .await?;
    if plain || !refused_compressed(&result) {
        return Ok(result);
    }
    tracing::info!(
        "{peer_url} reads only uncompressed sync offers (a veles from before enc:z) — \
         sending it those until this process restarts"
    );
    PLAIN_PEERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(peer_url.to_string());
    client
        .call(peer_url, bearer, &encode(false)?, context_id)
        .await
}

/// The offer inside a message (or file): base64 fence verified against
/// its checksum, else the whole text as JSON (a file written by
/// `veles replicas offer`).
pub fn decode_message(raw: &str) -> Result<StoreOffer> {
    use base64::Engine as _;
    if let Some(start) = raw.find(FENCE_B64) {
        let body = &raw[start + FENCE_B64.len()..];
        let body = &body[..body.find("```").unwrap_or(body.len())];
        let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
        let sha = lines
            .next()
            .and_then(|l| l.strip_prefix("sha256:"))
            .context("store offer: missing sha256 line")?
            .to_string();
        // Optional encoding marker: `enc:z` = the base64 decodes to a
        // DEFLATE stream over the offer JSON. Absent = the older layout
        // (base64 over the plain JSON). Take the first remaining line;
        // if it is the marker the body follows it, else it IS the body's
        // first line and goes back in front.
        let first = lines.next().unwrap_or("");
        let compressed = first == "enc:z";
        let b64: String = if compressed {
            lines.collect()
        } else {
            std::iter::once(first).chain(lines).collect()
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .context("store offer: payload is not base64")?;
        let bytes = if compressed {
            inflate(&bytes).context("store offer")?
        } else {
            bytes
        };
        let json = String::from_utf8(bytes).context("store offer: not UTF-8")?;
        if crate::memory::MemoryStore::content_sha(&json) != sha {
            anyhow::bail!("store offer: checksum mismatch — the payload changed in transit");
        }
        return serde_json::from_str(&json).context("store offer: payload is not an offer");
    }
    let v: serde_json::Value = serde_json::from_str(raw.trim()).context("store offer: not JSON")?;
    let v = v.get("offer").cloned().unwrap_or(v);
    serde_json::from_value(v).context("store offer: JSON is not an offer")
}

/// Where received offers wait: `<veles_home>/storesync/inbox/`.
pub fn inbox_dir() -> std::path::PathBuf {
    crate::config::veles_home().join("storesync").join("inbox")
}

/// One stored offer, with who delivered it and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub received_at: String,
    pub peer: String,
    pub task_id: String,
    pub offer: StoreOffer,
}

/// How many offers may wait in one inbox before the server refuses more.
pub const INBOX_MAX: usize = 200;

/// Store an offer in `dir`, named by arrival time so a listing IS
/// arrival order (chunks apply in send order). Atomic write.
pub fn inbox_store(
    dir: &std::path::Path,
    peer: &str,
    task_id: &str,
    offer: StoreOffer,
) -> Result<std::path::PathBuf> {
    use std::collections::HashSet;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let pending = inbox_pending(dir)?;
    if pending.len() >= INBOX_MAX {
        anyhow::bail!(
            "storesync inbox full ({} offers waiting) — apply with \
             `veles replicas apply --inbox` first",
            pending.len()
        );
    }
    // One node, one queue: a node's parts apply in order, and offers
    // from different nodes do not interleave inside one part-sequence.
    let _ = HashSet::<String>::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let safe: String = task_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(40)
        .collect();
    let path = dir.join(format!("{nanos:020}-{safe}.json"));
    let entry = InboxEntry {
        received_at: chrono::Utc::now().to_rfc3339(),
        peer: peer.to_string(),
        task_id: task_id.to_string(),
        offer,
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&entry)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(path)
}

/// Pending offers in `dir`, oldest first. A missing inbox is empty.
pub fn inbox_pending(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<_> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    out.sort();
    Ok(out)
}

/// Drop applied-offer files older than `max_age_secs`, same rule and
/// same fail-closed shape as the memsync prune: only `applied/` (never
/// a pending offer or a held refusal), only files whose age can be
/// proven. Returns how many were removed.
pub fn prune_applied(dir: &std::path::Path, max_age_secs: i64) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let Ok(applied) = inbox_pending(&dir.join("applied")) else {
        return 0;
    };
    let mut removed = 0;
    for p in applied {
        let provable = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now - d.as_secs() as i64 > max_age_secs)
            .unwrap_or(false);
        if provable && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Read an inbox entry (or any offer file) back.
pub fn inbox_read(path: &std::path::Path) -> Result<InboxEntry> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if let Ok(e) = serde_json::from_str::<InboxEntry>(&raw) {
        return Ok(e);
    }
    Ok(InboxEntry {
        received_at: String::new(),
        peer: String::new(),
        task_id: String::new(),
        offer: decode_message(&raw)?,
    })
}

/// Build offers for everything past `since_seq` and push them to a
/// peer's inbox over the A2A link, chunked by [`OFFER_ROW_LIMIT``],
/// walking the cursor. Withholds secret-shaped rows (named in the
/// report). Idempotent at the receiver (cursor + REPLACE), so a failed
/// multi-chunk push is re-runnable whole.
pub async fn push_to_peer(
    store: &Store,
    from_node: &str,
    from_name: &str,
    peer_url: &str,
    bearer: Option<&str>,
    since_seq: i64,
) -> Result<String> {
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let mut out = Vec::new();
    let mut cursor = since_seq;
    loop {
        let offer = build_offer(store, from_node, from_name, cursor)?;
        if offer.rows.is_empty() {
            if cursor == since_seq {
                out.push(format!("nothing to push past seq {since_seq}"));
            }
            break;
        }
        let last = offer.rows.last().expect("non-empty").seq;
        let result = call_offer(&client, peer_url, bearer, &ctx, |z| {
            encode_message_as(&offer, z)
        })
        .await
        .with_context(|| format!("push rows past {cursor} to {peer_url}"))?;
        let state = result["status"]["state"].as_str().unwrap_or("?");
        let reply = crate::a2a::summarize_result(&result);
        if state != crate::a2a::STATE_COMPLETED {
            anyhow::bail!("{peer_url} refused the chunk past {cursor}:\n{reply}");
        }
        out.push(format!(
            "rows {}..{} ({} rows) stored in the inbox",
            cursor + 1,
            last,
            offer.rows.len()
        ));
        cursor = last;
    }
    out.push(format!(
        "apply there with `veles replicas apply --inbox` (cursor now {cursor})"
    ));
    Ok(out.join("\n"))
}

/// Meta key prefix: the log floor a backfill was last made at for a
/// peer behind it. One backfill per floor — a replica that keeps
/// refusing must not append the recent sessions again every tick.
const BACKFILLED_FOR_PREFIX: &str = "veles.storesync.backfilled_for.";

/// Backfill for `peer_name` when an offer from `cursor` would start below
/// the log's floor, once per floor. None = not needed (or already done).
pub fn backfill_below_floor(
    store: &Store,
    peer_name: &str,
    cursor: i64,
    days: u32,
) -> Result<Option<BackfillReport>> {
    let floor = log_floor(store)?;
    if cursor >= floor || days == 0 {
        return Ok(None);
    }
    let key = format!("{BACKFILLED_FOR_PREFIX}{peer_name}");
    if meta_get(store.conn(), &key).as_deref() == Some(floor.to_string().as_str()) {
        return Ok(None);
    }
    let r = backfill(store, days)?;
    meta_put(store.conn(), &key, &floor.to_string())?;
    Ok(Some(r))
}

// ── the turn-end nudge ──────────────────────────────────────────────

static PUSH_NUDGE: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

#[cfg(test)]
pub(crate) static NUDGED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A turn in `session_id` ended: push now rather than at the next gossip
/// tick, so a node that dies between turns leaves its peers a replica
/// that ends where the conversation did. Cheap anywhere; in a process
/// with no push task (the CLI) the wake-up is simply never taken.
pub fn nudge(session_id: &str) {
    #[cfg(test)]
    NUDGED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(session_id.to_string());
    let _ = session_id;
    PUSH_NUDGE.notify_one();
}

/// Wait for the next [`nudge`] (one taken per wake-up; nudges while
/// nobody waits collapse into one).
pub async fn nudged() {
    PUSH_NUDGE.notified().await
}

/// One pending offer that refused to apply (auto path).
#[derive(Debug, Clone)]
pub struct PendingRefusal {
    /// The offer's sender node (whose replica refused).
    pub sender_node: String,
    /// The apply error, verbatim.
    pub msg: String,
}

/// Drain the storesync inbox into replicas: every pending offer applied
/// oldest-first, on its sender's replica connection. The gateway calls
/// this every gossip tick when `[cluster] storesync_auto` is set; the
/// CLI's `replicas apply --inbox` is the by-hand spelling. Returns
/// (applied entries, refusals) — a refusal is NOT an error for the
/// caller: the offer stays pending in the inbox (a gap is filled by
/// the sender's heal; a data refusal is the operator's to see), and
/// `PendingRefusal` carries enough for the receiver's reply to name
/// what it needs.
pub fn apply_inbox() -> (usize, Vec<PendingRefusal>) {
    let dir = inbox_dir();
    let pending = match inbox_pending(&dir) {
        Ok(p) => p,
        Err(e) => {
            return (
                0,
                vec![PendingRefusal {
                    sender_node: String::new(),
                    msg: format!("inbox unreadable: {e:#}"),
                }],
            )
        }
    };
    if pending.is_empty() {
        return (0, Vec::new());
    }
    // An offer's verdict changes only when the inbox does: a new offer
    // arrives, or one applies and leaves for `applied/` (which moves a
    // replica cursor). So a listing identical to the last pass that
    // applied nothing gets that pass's refusals back without opening a
    // file. The wedge of 2026-10-02 had the VPS re-parse 200 refused
    // offers (142 MB) on every 30 s tick. The memo expires after ten
    // minutes, so a transient refusal (a locked replica) is retried even
    // when no new offer arrives.
    let names: Vec<std::ffi::OsString> = pending
        .iter()
        .filter_map(|p| p.file_name())
        .map(Into::into)
        .collect();
    {
        let memo = refused_pass().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((n, r, at)) = memo.get(&dir) {
            if *n == names && at.elapsed() < REFUSED_PASS_TTL {
                return (0, r.clone());
            }
        }
    }
    let done = dir.join("applied");
    let _ = std::fs::create_dir_all(&done);
    let mut applied = 0;
    let mut refusals: Vec<PendingRefusal> = Vec::new();
    for p in pending {
        let entry = match inbox_read(&p) {
            Ok(e) => e,
            Err(e) => {
                refusals.push(PendingRefusal {
                    sender_node: String::new(),
                    msg: format!("{}: {e:#}", p.display()),
                });
                continue;
            }
        };
        let from = entry.offer.from_node.clone();
        let conn = match open_replica(&from) {
            Ok(c) => c,
            Err(e) => {
                refusals.push(PendingRefusal {
                    sender_node: from,
                    msg: format!("{}: {e:#}", p.display()),
                });
                continue;
            }
        };
        match apply_offer(&conn, &entry.offer) {
            Ok(report) => {
                applied += 1;
                let _ = std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()));
                tracing::debug!(
                    "storesync: applied {} rows (cursor {}) into the {} replica{}",
                    report.applied_rows,
                    report.final_seq,
                    report.replica,
                    if report.withheld_secret_rows > 0 {
                        format!(
                            " ({} withheld: credential-shaped)",
                            report.withheld_secret_rows
                        )
                    } else {
                        String::new()
                    }
                );
            }
            Err(e) => refusals.push(PendingRefusal {
                sender_node: from,
                msg: format!("{e:#}"),
            }),
        }
    }
    {
        let mut memo = refused_pass().lock().unwrap_or_else(|p| p.into_inner());
        if applied == 0 {
            memo.insert(dir, (names, refusals.clone(), std::time::Instant::now()));
        } else {
            memo.remove(&dir);
        }
    }
    (applied, refusals)
}

/// Per inbox directory, the last pass that applied nothing: its
/// listing, what it refused, and when (see [`apply_inbox`]).
type RefusedPass = std::collections::HashMap<
    std::path::PathBuf,
    (
        Vec<std::ffi::OsString>,
        Vec<PendingRefusal>,
        std::time::Instant,
    ),
>;

/// How long an all-refused pass is trusted without re-reading the inbox.
const REFUSED_PASS_TTL: std::time::Duration = std::time::Duration::from_secs(600);

fn refused_pass() -> &'static std::sync::Mutex<RefusedPass> {
    static MEMO: std::sync::OnceLock<std::sync::Mutex<RefusedPass>> = std::sync::OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// The heal directive a receiver's reply can carry after an auto-apply
/// hit a gap: "re-push past N" names the exact missing range start (the
/// replica's applied cursor). Bounded and unambiguous — only the
/// receiver emits it, only the sender acts on it.
pub const HEAL_MARKER: &str = "gap: re-push past ";

/// Parse the heal directive out of a peer's reply. None = no gap.
pub fn heal_from_reply(reply: &str) -> Option<i64> {
    let idx = reply.rfind(HEAL_MARKER)?;
    let rest = &reply[idx + HEAL_MARKER.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Is the given error string a gap refusal asking for a re-push from
/// zero? (The receiver-side test for building a heal directive.)
pub fn is_gap_refusal(msg: &str) -> bool {
    msg.contains("re-push from zero")
}

/// One auto-push pass for a single peer: read the cursor, push chunk by
/// chunk, advance the cursor per stored chunk. A reply carrying the
/// heal directive (`gap: re-push past N` — the receiver's apply hit a
/// hole) re-pushes the named missing range ONCE, bounded: the fill
/// offer lands in the receiver's inbox and its auto-apply folds it
/// together with everything pending. Any other refusal (the peer
/// rejecting the store itself) is an error, said out loud.
///
/// Takes the shared store handle and locks it only for the short sync
/// stretches (build offer, advance cursor) — NEVER across an await
/// (the B1 lesson: a wedged peer must not stall every store user).
///
/// A peer behind the log's floor (added after pruning, or wiped) is
/// skipped to the floor by the offer's `base`, and gets the recent
/// sessions whole from a [`backfill`] of `backfill_days`.
pub async fn push_incremental_to_peer(
    store: &std::sync::Arc<std::sync::Mutex<Store>>,
    from_node: &str,
    from_name: &str,
    peer_name: &str,
    peer_url: &str,
    bearer: Option<&str>,
    backfill_days: u32,
) -> Result<String> {
    let lock = || crate::store::lock_timed(store, "storesync::push");
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let mut cursor = peer_cursor(&lock(), peer_name);
    let mut pushed_rows = 0u64;
    let mut healed = false;
    loop {
        let offer = {
            let s = lock();
            if let Some(r) = backfill_below_floor(&s, peer_name, cursor, backfill_days)? {
                tracing::info!(
                    "storesync: {peer_name} is behind this log's start — backfilled {} row(s) \
                     of {} session(s) for it",
                    r.rows,
                    r.sessions
                );
            }
            build_offer(&s, from_node, from_name, cursor)?
        };
        if offer.rows.is_empty() {
            break;
        }
        let last = offer.rows.last().expect("non-empty").seq;
        let result = call_offer(&client, peer_url, bearer, &ctx, |z| {
            encode_message_as(&offer, z)
        })
        .await
        .with_context(|| format!("auto-push past {cursor} to {peer_url}"))?;
        let state = result["status"]["state"].as_str().unwrap_or("?");
        let reply = crate::a2a::summarize_result(&result);
        if state != crate::a2a::STATE_COMPLETED {
            anyhow::bail!("{peer_url} refused the auto-push past {cursor}:\n{reply}");
        }
        pushed_rows += offer.rows.len() as u64;
        let acked = replica_cursor_from_reply(&reply);
        // Both cursors go in under one lock and one commit. Each commit
        // is an fsync, and on a busy disk one can take seconds.
        {
            let s = lock();
            let tx = s.write_tx().ok();
            advance_peer_cursor(&s, peer_name, last);
            if let Some(rep) = acked {
                record_acked(&s, peer_name, rep);
            }
            if let Some(Err(e)) = tx.map(|t| t.commit()) {
                tracing::warn!("storesync: {peer_name}'s cursors did not commit: {e:#}");
            }
        }
        cursor = last;
        // The stored-vs-applied drift the reply reports (debug only —
        // hand-apply mode lags by design; the heal directive below is
        // the actionable signal).
        if let Some(rep) = acked {
            if rep < cursor {
                tracing::debug!(
                    "storesync: {peer_name} replica cursor {rep} < stored {cursor} \
                     (pending apply or hand mode)"
                );
            }
        }
        if let Some(need) = heal_from_reply(&reply).filter(|_| !healed) {
            tracing::warn!("storesync: {peer_name} reports a gap — re-pushing past {need} once");
            healed = true;
            cursor = need;
            continue;
        }
        // A chunk cut short by the byte budget is not the end: the
        // loop ends when the next offer comes back empty.
    }
    if pushed_rows == 0 {
        Ok("nothing to push".into())
    } else if healed {
        Ok(format!(
            "pushed {pushed_rows} rows (including a gap-heal re-push)"
        ))
    } else {
        Ok(format!("pushed {pushed_rows} rows past the cursor"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch primary store with the log installed. The directory
    /// override keeps every replica/inbox path inside the tempdir —
    /// without it `veles_home()` answers the LIVE home and a test
    /// would write `replicas/` next to the real store (found the hard
    /// way: the first run did exactly that).
    fn scratch_primary(tag: &str) -> (tempfile::TempDir, Store, crate::config::DirGuard) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
        let guard = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
            dir.path().to_path_buf(),
        )));
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        install_triggers(&store).unwrap();
        let _ = tag;
        (dir, store, guard)
    }

    #[test]
    fn writes_land_in_the_log() {
        let (_d, store, _g) = scratch_primary("a");
        store
            .conn()
            .execute("INSERT INTO sessions(id, title) VALUES('s1', 'hello')", [])
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO messages(session_id, role, content)
                 VALUES('s1', 'user', 'hello world')",
                [],
            )
            .unwrap();
        let rows = log_rows(&store, 0, 100).unwrap();
        assert_eq!(rows.len(), 2, "insert and message both logged");
        assert_eq!(rows[0].tbl, "sessions");
        assert_eq!(rows[1].tbl, "messages");
        assert_eq!(rows[1].op, "I");
        let img: serde_json::Value = serde_json::from_str(&rows[1].row_json).unwrap();
        assert_eq!(img["content"], "hello world");
        assert_eq!(img["role"], "user");
    }

    #[test]
    fn updates_and_deletes_log_old_images() {
        let (_d, store, _g) = scratch_primary("b");
        store
            .conn()
            .execute(
                "INSERT INTO session_locale(session_id, locale) VALUES('s9', 'en')",
                [],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE session_locale SET locale = 'de' WHERE session_id = 's9'",
                [],
            )
            .unwrap();
        store
            .conn()
            .execute("DELETE FROM session_locale WHERE session_id = 's9'", [])
            .unwrap();
        let rows = log_rows(&store, 0, 100).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].op, "U");
        assert_eq!(rows[2].op, "D");
        let img: serde_json::Value = serde_json::from_str(&rows[2].row_json).unwrap();
        assert_eq!(img["locale"], "de", "the delete logs what it removed");
        let pk: serde_json::Value = serde_json::from_str(&rows[2].pk).unwrap();
        assert_eq!(pk["session_id"], "s9");
    }

    #[test]
    fn memory_tables_are_not_logged() {
        let (_d, store, _g) = scratch_primary("c");
        // Store::open does not create the memory tables (MemoryStore
        // owns them); make them so the trigger-absence claim is tested
        // against tables that exist.
        store
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS memory_revisions (
                 rev_id INTEGER PRIMARY KEY AUTOINCREMENT,
                 path TEXT NOT NULL, content TEXT NOT NULL,
                 sha256 TEXT NOT NULL, source TEXT NOT NULL, note TEXT,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')))",
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO memory_revisions(path, content, sha256, source)
                 VALUES('x.md', 'c', 'h', 'test')",
                [],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO nodes(id, name, host) VALUES('n1', 'n1', 'h1')",
                [],
            )
            .unwrap();
        let rows = log_rows(&store, 0, 100).unwrap();
        assert!(
            rows.iter()
                .all(|r| r.tbl != "memory_revisions" && r.tbl != "nodes"),
            "memory and the registry never ride the store log"
        );
    }

    /// The fold ban, positive form: applying an offer creates/uses a
    /// SEPARATE replica file; the local store's memory_revisions is
    /// untouched, and an offer row naming memory is refused outright.
    #[test]
    fn a_memory_row_in_an_offer_is_refused() {
        let (_d, store, _g) = scratch_primary("d");
        store
            .conn()
            .execute("INSERT INTO sessions(id, title) VALUES('s1', 'x')", [])
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        // Sabotage: a memory row smuggled into the offer.
        let mut bad = offer.clone();
        bad.rows.push(StoreLogRow {
            seq: offer.rows.last().map(|r| r.seq + 1).unwrap_or(1),
            at: String::new(),
            tbl: "memory_revisions".into(),
            op: "I".into(),
            pk: "{\"rev_id\": 1}".into(),
            row_json: "{\"rev_id\": 1, \"path\": \"x.md\"}".into(),
        });
        let conn = open_replica("nodeA").unwrap();
        let err = apply_offer(&conn, &bad).unwrap_err();
        assert!(
            err.to_string().contains("memory travels by memsync"),
            "refusal names memsync: {err}"
        );
    }

    /// The 2026-10-02 wedge. A credential-shaped row is withheld from the
    /// replica, but its seq still moves the cursor — the withheld row
    /// used to be dropped from the offer before the contiguity check,
    /// which then refused the NEXT row as a gap, on every re-push, for
    /// ever (the VPS and the phone froze at seq 1634 behind one tool
    /// result that quoted a bearer token).
    #[test]
    fn a_withheld_row_moves_the_cursor_and_is_not_a_gap() {
        let (_d, store, _g) = scratch_primary("withheld");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        for content in [
            "before the secret",
            "curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789'",
            "after the secret",
        ] {
            store
                .conn()
                .execute(
                    "INSERT INTO messages(session_id, role, content) VALUES('s1', 'tool', ?1)",
                    [content],
                )
                .unwrap();
        }
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let last = offer.rows.last().expect("rows").seq;
        let conn = open_replica("nodeA").unwrap();
        let report = apply_offer(&conn, &offer).expect("a withheld row is not a gap");
        assert_eq!(report.withheld_secret_rows, 1, "exactly the bearer row");
        assert_eq!(report.final_seq, last, "the cursor reached the offer's end");
        let contents: Vec<String> = conn
            .prepare("SELECT content FROM messages ORDER BY msg_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            contents,
            ["before the secret", "after the secret"],
            "the bearer row stayed home; the rows around it landed"
        );
        // The next offer continues from the end, not from the hole.
        store
            .conn()
            .execute(
                "INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'later')",
                [],
            )
            .unwrap();
        let next = build_offer(&store, "nodeA", "a", last).unwrap();
        let report = apply_offer(&conn, &next).expect("the stream goes on");
        assert!(report.applied_rows >= 1);
    }

    /// An offer is capped by bytes as well as rows (at least one row),
    /// and walking the cursor until an offer comes back empty still
    /// delivers every row — a chunk cut short is not the end.
    #[test]
    fn offers_are_capped_by_bytes_and_the_walk_reaches_the_end() {
        let (_d, store, _g) = scratch_primary("bytes");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('big')", [])
            .unwrap();
        let big = "x".repeat(300 * 1024);
        for _ in 0..6 {
            store
                .conn()
                .execute(
                    "INSERT INTO messages(session_id, role, content) VALUES('big', 'tool', ?1)",
                    [&big],
                )
                .unwrap();
        }
        let head = log_rows(&store, 0, 1_000_000).unwrap().last().unwrap().seq;
        let (mut cursor, mut chunks) = (0, 0);
        loop {
            let offer = build_offer(&store, "nodeA", "a", cursor).unwrap();
            if offer.rows.is_empty() {
                break;
            }
            let bytes: usize = offer.rows.iter().map(|r| r.row_json.len()).sum();
            assert!(
                offer.rows.len() == 1 || bytes <= OFFER_BYTE_BUDGET,
                "a chunk of {} rows carried {bytes} bytes",
                offer.rows.len()
            );
            cursor = offer.rows.last().unwrap().seq;
            chunks += 1;
        }
        assert_eq!(cursor, head, "the walk reached the log's head");
        assert!(chunks >= 3, "six 300 KB rows took {chunks} chunk(s)");
    }

    #[test]
    fn a_gap_is_refused_and_zero_rebuild_works() {
        let (_d, store, _g) = scratch_primary("e");
        for i in 0..4 {
            store
                .conn()
                .execute(&format!("INSERT INTO sessions(id) VALUES('s{i}')"), [])
                .unwrap();
        }
        // Apply only rows past 1 (a hole at 1).
        let full = build_offer(&store, "nodeA", "a", 0).unwrap();
        let mut partial = full.clone();
        partial.rows.drain(0..1);
        let conn = open_replica("nodeA").unwrap();
        let err = apply_offer(&conn, &partial).unwrap_err();
        assert!(err.to_string().contains("gap"), "gap refusal: {err}");
        // The full offer from zero applies clean.
        let report = apply_offer(&conn, &full).unwrap();
        assert_eq!(report.applied_rows, 4);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4);
    }

    #[test]
    fn reapply_is_skipped_not_duplicated() {
        let (_d, store, _g) = scratch_primary("f");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let conn = open_replica("nodeA").unwrap();
        let first = apply_offer(&conn, &offer).unwrap();
        assert_eq!(first.applied_rows, 1);
        let again = apply_offer(&conn, &offer).unwrap();
        assert_eq!(again.applied_rows, 0);
        assert_eq!(again.skipped_uptodate, 1);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "no duplicate");
    }

    /// An INSERT followed by an UPDATE of the same row is two log rows
    /// with the same pk — the second must REPLACE the first, not hit a
    /// UNIQUE violation. This is the pin that fails if `INSERT OR
    /// REPLACE` degrades to a plain INSERT.
    #[test]
    fn updates_overwrite_the_inserted_row_not_conflict() {
        let (_d, store, _g) = scratch_primary("f2");
        store
            .conn()
            .execute("INSERT INTO sessions(id, title) VALUES('s1', 'one')", [])
            .unwrap();
        store
            .conn()
            .execute("UPDATE sessions SET title = 'two' WHERE id = 's1'", [])
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let conn = open_replica("nodeA").unwrap();
        let report =
            apply_offer(&conn, &offer).expect("insert-then-update must not hit a UNIQUE violation");
        assert_eq!(report.applied_rows, 2);
        let title: String = conn
            .query_row("SELECT title FROM sessions WHERE id = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "two", "the update's image is the final one");
    }

    #[test]
    fn deletes_travel() {
        let (_d, store, _g) = scratch_primary("g");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        store
            .conn()
            .execute("DELETE FROM sessions WHERE id = 's1'", [])
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let conn = open_replica("nodeA").unwrap();
        let report = apply_offer(&conn, &offer).unwrap();
        assert_eq!(report.deleted_rows, 1);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the delete removed the row on the replica");
    }

    #[test]
    fn round_trip_makes_the_replica_searchable() {
        let (_d, store, _g) = scratch_primary("h");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        for i in 0..3 {
            store
                .conn()
                .execute(
                    "INSERT INTO messages(session_id, role, content)
                     VALUES('s1', 'user', ?1)",
                    [format!("unique-needle-{i}")],
                )
                .unwrap();
        }
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let conn = open_replica("nodeA").unwrap();
        apply_offer(&conn, &offer).unwrap();
        // The replica's FTS was populated by its own messages_ai
        // trigger during apply — search works on the replica file.
        let hits: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT m.session_id FROM messages_fts
                     JOIN messages m ON m.msg_id = messages_fts.msg_id
                     WHERE messages_fts MATCH '\"unique-needle\"'",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(hits.len(), 3, "FTS over the replica finds the history");
        // And the owner question answers from the row.
        assert_eq!(
            session_owner(&store, "s1").unwrap(),
            Some(owner_of(store.conn()).unwrap())
        );
    }

    #[test]
    fn wire_round_trip_and_tamper_detection() {
        let (_d, store, _g) = scratch_primary("i");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let msg = encode_message(&offer).unwrap();
        assert!(is_store_offer(&msg));
        let back = decode_message(&msg).unwrap();
        assert_eq!(back.rows.len(), offer.rows.len());
        // Tamper INSIDE the base64 payload (an edit to the plain-text
        // summary line is undetectable by design — it is not covered
        // by the checksum).
        let mut chars: Vec<u8> = msg.clone().into_bytes();
        let mid = chars.len() / 2;
        let flipped = if chars[mid] == b'A' { b'B' } else { b'A' };
        chars[mid] = flipped;
        let tampered = String::from_utf8(chars).unwrap();
        assert!(decode_message(&tampered).is_err(), "checksum catches edits");
    }

    #[test]
    fn wire_accepts_both_compressed_and_plain_payloads() {
        let (_d, store, _g) = scratch_primary("z");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        for i in 0..8 {
            store
                .conn()
                .execute(
                    "INSERT INTO messages(session_id, role, content)
                     VALUES('s1', 'user', ?1)",
                    [format!("compress-me-{i}-{i}-{i}")],
                )
                .unwrap();
        }
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let json = serde_json::to_string(&offer).unwrap();
        let sha = crate::memory::MemoryStore::content_sha(&json);

        // The new wire form: enc:z + base64 over the DEFLATE stream.
        let msg = encode_message(&offer).unwrap();
        assert!(msg.contains("enc:z"), "encode now marks the payload");
        assert!(msg.len() < json.len(), "the fence beats the raw JSON");
        let back = decode_message(&msg).unwrap();
        assert_eq!(back.rows.len(), offer.rows.len());

        // The pre-compression wire form a mixed pool still sends: no
        // enc: line, base64 over the plain JSON. Same sha, so a receiver
        // that only knows the old layout verifies it identically.
        use base64::Engine as _;
        let plain_b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let plain = format!(
            "{STORE_MARKER} {} rows past {}\n{FENCE_B64}\nsha256:{sha}\n{plain_b64}\n```",
            offer.rows.len(),
            offer.since_seq
        );
        let back_plain = decode_message(&plain).unwrap();
        assert_eq!(
            back_plain.rows.len(),
            offer.rows.len(),
            "a plain (uncompressed) offer from an older node still decodes"
        );
    }

    /// A peer from before `enc:z`, on a real socket: it refuses a
    /// compressed offer with its decoder's own words and stores a plain
    /// one. Returns its URL and every offer text it was sent.
    fn old_receiver() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { return };
                let mut raw = Vec::new();
                let mut buf = [0u8; 65536];
                let mut need = None;
                while need.is_none_or(|n| raw.len() < n) {
                    let Ok(n) = sock.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if need.is_none() {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                            let len = head
                                .split("content-length:")
                                .nth(1)
                                .and_then(|t| t.lines().next())
                                .and_then(|t| t.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(end + 4 + len);
                        }
                    }
                }
                let body = String::from_utf8_lossy(&raw).to_string();
                let rpc: serde_json::Value = body
                    .split_once("\r\n\r\n")
                    .and_then(|(_, b)| serde_json::from_str(b).ok())
                    .unwrap_or_default();
                let text = rpc["params"]["message"]["parts"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let (state, said) = if text.lines().any(|l| l.trim() == "enc:z") {
                    (
                        crate::a2a::STATE_REJECTED,
                        "not stored: store offer: payload is not base64: Invalid symbol 58, offset 3.",
                    )
                } else {
                    (crate::a2a::STATE_COMPLETED, "stored")
                };
                log.lock().unwrap().push(text);
                let reply = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": rpc["id"],
                    "result": {"task": crate::a2a::build_task("t", "c", state, said)},
                })
                .to_string();
                let _ = write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (url, seen)
    }

    /// The 2026-10-04 rollout: the VPS ran the compressing build, the
    /// laptop the one before, and every push between them was refused
    /// "payload is not base64". A peer that refuses the compressed form
    /// is sent the plain one at once, and only the plain one after.
    #[tokio::test]
    async fn an_older_receiver_is_sent_the_plain_layout() {
        let (_d, store, _g) = scratch_primary("old");
        let add = |n: usize| {
            store
                .conn()
                .execute("INSERT OR IGNORE INTO sessions(id) VALUES('s1')", [])
                .unwrap();
            for i in 0..n {
                store
                    .conn()
                    .execute(
                        "INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', ?1)",
                        [format!("row {i}")],
                    )
                    .unwrap();
            }
        };
        let (url, seen) = old_receiver();
        add(3);
        let out = push_to_peer(&store, "nodeA", "a", &url, None, 0)
            .await
            .expect("the push lands on an older receiver");
        assert!(out.contains("stored in the inbox"), "{out}");
        let head = max_seq(&store).unwrap();
        add(2);
        push_to_peer(&store, "nodeA", "a", &url, None, head)
            .await
            .expect("the second push lands too");

        let sent = seen.lock().unwrap().clone();
        let compressed: Vec<bool> = sent
            .iter()
            .map(|t| t.lines().any(|l| l.trim() == "enc:z"))
            .collect();
        assert_eq!(
            compressed,
            vec![true, false, false],
            "one compressed try, then plain — and plain from then on"
        );
        for t in &sent[1..] {
            assert!(decode_message(t).is_ok(), "a plain offer decodes");
        }
    }

    /// `enc:z` over something that is not DEFLATE is named, and a payload
    /// that would inflate past the cap is refused rather than allocated.
    #[test]
    fn inflate_refuses_garbage_and_bombs() {
        assert!(inflate(b"not deflate at all").is_err());
        let bomb = deflate(&vec![b'a'; 1 << 20]);
        assert!(bomb.len() < 4096, "a small request");
        let err = inflate_at_most(&bomb, 64 << 10).unwrap_err().to_string();
        assert!(err.contains("inflates past"), "{err}");
        assert_eq!(inflate_at_most(&bomb, 1 << 20).unwrap().len(), 1 << 20);
        assert_eq!(inflate(&deflate(b"offer")).unwrap(), b"offer");
    }

    #[test]
    fn owner_and_replica_markers_distinguish() {
        let (_d, store, _g) = scratch_primary("j");
        let primary_owner = owner_of(store.conn()).unwrap();
        assert!(!is_replica(store.conn()));
        let conn = open_replica("nodeB").unwrap();
        assert!(is_replica(&conn));
        assert_eq!(owner_of(&conn).as_deref(), Some("nodeB"));
        assert_ne!(
            owner_of(&conn).unwrap(),
            primary_owner,
            "a replica of B answers 'B owns these rows', not the local id"
        );
    }

    #[test]
    fn trigger_sha_drift_reinstalls() {
        let (_d, store, _g) = scratch_primary("k");
        let first = install_triggers(&store).unwrap();
        assert_eq!(first, "unchanged", "second install is a no-op");
        // Schema drift: a new column on a replicated table regenerates.
        store
            .conn()
            .execute_batch("ALTER TABLE sessions ADD COLUMN drift_test TEXT")
            .unwrap();
        let second = install_triggers(&store).unwrap();
        assert_ne!(second, "unchanged");
        assert_ne!(second, first);
        // And the regenerated trigger logs the new column.
        store
            .conn()
            .execute(
                "INSERT INTO sessions(id, drift_test) VALUES('s1', 'here')",
                [],
            )
            .unwrap();
        let rows = log_rows(&store, 0, 10).unwrap();
        let img: serde_json::Value = serde_json::from_str(&rows.last().unwrap().row_json).unwrap();
        assert_eq!(img["drift_test"], "here");
    }

    // ── B5 tail: auto-push cursors, inbox drain, heal ──────────────

    #[test]
    fn cursors_are_per_peer_and_monotone() {
        let (_d, store, _g) = scratch_primary("auto1");
        assert_eq!(peer_cursor(&store, "b"), 0, "no cursor yet");
        advance_peer_cursor(&store, "b", 5);
        advance_peer_cursor(&store, "b", 3); // stale tick: must not lower
        assert_eq!(peer_cursor(&store, "b"), 5);
        assert_eq!(peer_cursor(&store, "c"), 0, "per-peer isolation");
        set_peer_cursor(&store, "b", 2); // heal path may lower
        assert_eq!(peer_cursor(&store, "b"), 2);
    }

    #[test]
    fn apply_inbox_applies_and_keeps_refusals_pending() {
        let (_d, store, _g) = scratch_primary("auto2");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO messages(session_id, role, content)
                 VALUES('s1', 'user', 'gap-fill-needle')",
                [],
            )
            .unwrap();
        // A gap offer: starts at seq 2 while the replica cursor is 0.
        let full = build_offer(&store, "nodeA", "a", 0).unwrap();
        assert!(full.rows.len() >= 2, "two log rows make a real gap");
        let mut gapped = full.clone();
        let _ = gapped.rows.remove(0);
        let dir = inbox_dir();
        inbox_store(&dir, "a", "task-gap", gapped).unwrap();
        let (applied, refusals) = apply_inbox();
        assert_eq!(applied, 0, "the gapped offer applies nothing");
        assert_eq!(refusals.len(), 1);
        assert!(
            is_gap_refusal(&refusals[0].msg),
            "the refusal names the gap: {}",
            refusals[0].msg
        );
        assert_eq!(
            refusals[0].sender_node, "nodeA",
            "the refusal carries the sender for the heal directive"
        );
        // Still pending — a refusal never discards the offer.
        assert_eq!(
            inbox_pending(&dir).unwrap().len(),
            1,
            "the refused offer stays in the inbox"
        );
        // Now the full offer arrives (the heal fill). Arrival order
        // puts the gapped offer FIRST — it retries before the fill has
        // applied and refuses again; the fill applies behind it. Only
        // the NEXT drain resolves the gapped one as an up-to-date skip.
        // That is the convergence contract: refusals never discard,
        // every drain makes progress, the inbox empties.
        inbox_store(&dir, "a", "task-full", full).unwrap();
        let (applied, refusals) = apply_inbox();
        assert_eq!(applied, 1, "the fill applies");
        assert_eq!(refusals.len(), 1, "the older gapped offer refused again");
        let (applied, refusals) = apply_inbox();
        assert_eq!(applied, 1, "the gapped offer resolves as a skip");
        assert!(refusals.is_empty(), "no refusals left");
        assert_eq!(
            inbox_pending(&dir).unwrap().len(),
            0,
            "inbox drained, everything applied"
        );
        let cur = replica_cursor("nodeA");
        assert!(cur >= 2, "the replica advanced to {cur}");
    }

    /// An unchanged inbox after a pass that applied nothing is not read
    /// again: the wedge had the VPS re-parse 142 MB of refused offers
    /// every 30 s. Proven by corrupting the pending file in place — a
    /// re-read would report a parse error, the memo reports the gap —
    /// and a new arrival (a changed listing) reads everything again.
    #[test]
    fn an_unchanged_refused_inbox_is_not_read_again() {
        let (_d, store, _g) = scratch_primary("memo");
        for i in 0..3 {
            store
                .conn()
                .execute(&format!("INSERT INTO sessions(id) VALUES('m{i}')"), [])
                .unwrap();
        }
        let full = build_offer(&store, "nodeA", "a", 0).unwrap();
        let mut gapped = full.clone();
        let _ = gapped.rows.remove(0);
        let dir = inbox_dir();
        let path = inbox_store(&dir, "a", "task-gap", gapped).unwrap();
        let (_, first) = apply_inbox();
        assert!(is_gap_refusal(&first[0].msg), "{}", first[0].msg);

        std::fs::write(&path, "not an offer").unwrap();
        let (applied, again) = apply_inbox();
        assert_eq!(applied, 0);
        assert!(
            is_gap_refusal(&again[0].msg),
            "an unchanged inbox is answered from the last pass: {}",
            again[0].msg
        );

        inbox_store(&dir, "a", "task-full", full).unwrap();
        let (applied, after) = apply_inbox();
        assert_eq!(applied, 1, "the new arrival is read and applies");
        assert!(
            after.iter().any(|r| !is_gap_refusal(&r.msg)),
            "and the corrupted file is read again, not remembered: {after:?}"
        );
    }

    #[test]
    fn heal_and_cursor_markers_parse_from_replies() {
        let reply = "stored 3 row(s) — auto-apply: 0 applied, 1 refused \
                     (gap: re-push past 4 (re-push the missing range, oldest first)) \
                     — replica cursor 4";
        assert_eq!(heal_from_reply(reply), Some(4));
        assert_eq!(replica_cursor_from_reply("(replica cursor 12)"), Some(12));
        assert_eq!(heal_from_reply("no gap here"), None);
        assert_eq!(replica_cursor_from_reply("no cursor here"), None);
        assert!(is_gap_refusal(
            "gap at seq 3 (replica cursor 1) — re-push from zero: …"
        ));
        assert!(!is_gap_refusal("some other refusal"));
    }

    #[test]
    fn auto_apply_under_the_flag_marks_and_cursors() {
        let (_d, store, _g) = scratch_primary("auto3");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO messages(session_id, role, content)
                 VALUES('s1', 'user', 'auto-apply-needle')",
                [],
            )
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let dir = inbox_dir();
        inbox_store(&dir, "a", "task-1", offer).unwrap();
        let (applied, refusals) = apply_inbox();
        assert_eq!((applied, refusals.len()), (1, 0));
        assert!(replica_cursor("nodeA") >= 1);
        // The replica is searchable — the B5 read surface underneath
        // the tool union.
        let hits = crate::storesync::search_replicas("auto-apply-needle", 5).unwrap();
        assert!(!hits.is_empty(), "the applied replica answers FTS");
    }

    // ── 2026-09-29 review: an offer is data from a peer ────────────

    /// One offer row for `tbl` carrying `image` as its row JSON.
    fn row(seq: i64, tbl: &str, op: &str, pk: &str, image: &str) -> StoreLogRow {
        StoreLogRow {
            seq,
            at: String::new(),
            tbl: tbl.into(),
            op: op.into(),
            pk: pk.into(),
            row_json: image.into(),
        }
    }

    fn offer_of(from: &str, rows: Vec<StoreLogRow>) -> StoreOffer {
        StoreOffer {
            from_node: from.into(),
            from_name: from.into(),
            since_seq: 0,
            rows,
            part: 0,
            parts: 0,
            base: 0,
        }
    }

    /// Row keys become column names in the SQL text. Quoting them is not
    /// enough — a `"` in the key closes the quote, and the drift-fix ALTER
    /// used to run as a multi-statement batch, so a key could ATTACH the
    /// node's own state.db and write anything into it.
    #[test]
    fn a_hostile_column_name_is_refused_and_nothing_escapes() {
        let (d, _store, _g) = scratch_primary("hostile-col");
        let evil = d.path().join("evil.db");
        let key = format!(
            "x\" TEXT; ATTACH DATABASE '{}' AS e; CREATE TABLE e.pwn(a); --",
            evil.display()
        );
        let image = serde_json::json!({ "id": "s1", key: "v" }).to_string();
        let offer = offer_of(
            "nodeA",
            vec![row(1, "sessions", "I", "{\"id\":\"s1\"}", &image)],
        );
        let conn = open_replica("nodeA").unwrap();
        let err = apply_offer(&conn, &offer).unwrap_err().to_string();
        assert!(!evil.exists(), "the ATTACH never ran ({err})");
        assert!(err.contains("not a plain identifier"), "{err}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "nothing of the offer landed");
    }

    /// A delete's pk keys are the WHERE clause. They must be exactly the
    /// table's pk columns — a key that closes its quote could widen the
    /// delete to every row.
    #[test]
    fn a_hostile_pk_key_is_refused() {
        let (_d, store, _g) = scratch_primary("hostile-pk");
        for id in ["s1", "s2"] {
            store
                .conn()
                .execute("INSERT INTO sessions(id) VALUES(?1)", [id])
                .unwrap();
        }
        let conn = open_replica("nodeA").unwrap();
        apply_offer(&conn, &build_offer(&store, "nodeA", "a", 0).unwrap()).unwrap();
        let widen = serde_json::json!({ "id\" = \"id\" OR \"id": "zz" }).to_string();
        let offer = offer_of("nodeA", vec![row(3, "sessions", "D", &widen, "{}")]);
        assert!(apply_offer(&conn, &offer).is_err(), "refused");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "no row was deleted");
    }

    /// `from_node` is the offer's own claim and becomes a file name. A
    /// path-shaped claim must never open a file outside `replicas/` —
    /// `../state` is the node's own state.db.
    #[test]
    fn a_path_shaped_node_id_opens_nothing() {
        let (_d, store, _g) = scratch_primary("traversal");
        for bad in ["../state", "a/b", "", ".hidden", "x\\y"] {
            assert!(open_replica(bad).is_err(), "{bad:?} refused");
        }
        assert!(
            !is_replica(store.conn()),
            "the primary was not stamped as a replica"
        );
        assert!(open_replica("0b5c1e1e-9d2c-4f6e-8a8e-5f1f6a7b8c9d").is_ok());
    }

    /// The fold ban, for real: an offer applies only into the replica of
    /// the node it claims to come from — never into the primary, never
    /// into a different node's replica.
    #[test]
    fn apply_refuses_a_store_that_is_not_the_senders_replica() {
        let (_d, store, _g) = scratch_primary("fold");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let e = apply_offer(store.conn(), &offer).unwrap_err().to_string();
        assert!(e.contains("not the replica of nodeA"), "{e}");
        let other = open_replica("nodeB").unwrap();
        assert!(
            apply_offer(&other, &offer).is_err(),
            "B's replica refuses A"
        );
        assert!(apply_offer(&open_replica("nodeA").unwrap(), &offer).is_ok());
    }

    /// Which node an authenticated peer may speak for: bound on first
    /// use, then fixed; a node another peer speaks for is refused; this
    /// node's own id is never a sender.
    #[test]
    fn a_peer_speaks_for_one_node() {
        let (_d, store, _g) = scratch_primary("bind");
        let me = "node-me";
        check_sender(&store, "a", "nodeA", me).expect("first use binds");
        check_sender(&store, "a", "nodeA", me).expect("the same claim again");
        let e = check_sender(&store, "a", "nodeB", me)
            .unwrap_err()
            .to_string();
        assert!(e.contains("nodeA"), "names the bound node: {e}");
        let e = check_sender(&store, "b", "nodeA", me)
            .unwrap_err()
            .to_string();
        assert!(e.contains("'a'"), "names the peer that holds it: {e}");
        assert!(check_sender(&store, "c", me, me).is_err(), "not our own id");
        assert!(check_sender(&store, "c", "../state", me).is_err());
    }

    fn log_len(store: &Store) -> i64 {
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM store_log", [], |r| r.get(0))
            .unwrap()
    }

    /// A node with nobody to replicate to keeps no log. B5 installed the
    /// triggers on every gateway at boot, so a single node copied every
    /// message into `store_log` forever with no reader and no pruning.
    #[test]
    fn no_storesync_peers_means_no_log() {
        let (_d, store, _g) = scratch_primary("nolog");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        assert_eq!(log_len(&store), 1, "installed by scratch_primary");
        sync_log_install(&store, &[], 14).unwrap();
        assert_eq!(log_len(&store), 0, "the rows went with it");
        store
            .conn()
            .execute(
                "INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'x')",
                [],
            )
            .unwrap();
        assert_eq!(log_len(&store), 0, "and nothing logs any more");
        // No backfill here: this pins what the triggers log.
        sync_log_install(&store, &["peer-b".to_string()], 0).unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'y')",
                [],
            )
            .unwrap();
        assert_eq!(log_len(&store), 1, "a peer turns it back on");
    }

    /// Ownership is identity, not replication: a gateway with no peers
    /// keeps no log but still says which node its sessions belong to
    /// (gating the log install dropped the stamp with it, and
    /// `veles session owner` answered "no owner stamp" — caught by the
    /// b6 end-to-end run).
    #[test]
    fn the_owner_stamp_does_not_need_the_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
        let _g = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
            dir.path().to_path_buf(),
        )));
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        sync_log_install(&store, &[], 14).unwrap();
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        assert!(owner_of(store.conn()).is_some(), "stamped without a log");
        assert_eq!(session_owner(&store, "s1").unwrap(), owner_of(store.conn()));
    }

    /// The live-turn registry beats every few seconds; it is a local fact
    /// and must not put a row in the log per heartbeat.
    #[test]
    fn session_run_heartbeats_are_not_logged() {
        let (_d, store, _g) = scratch_primary("runs");
        store.run_start("s1", 1, "h", "tui").unwrap();
        for _ in 0..3 {
            store.run_heartbeat("s1").unwrap();
        }
        store.run_end("s1").unwrap();
        assert_eq!(log_len(&store), 0);
    }

    /// Pruning deletes only what EVERY peer has applied; a peer that has
    /// never reported a cursor holds everything back.
    #[test]
    fn pruning_keeps_what_any_peer_has_not_applied() {
        let (_d, store, _g) = scratch_primary("prune");
        for i in 0..6 {
            store
                .conn()
                .execute("INSERT INTO sessions(id) VALUES(?1)", [format!("s{i}")])
                .unwrap();
        }
        let peers = vec!["b".to_string(), "c".to_string()];
        record_acked(&store, "b", 5);
        assert_eq!(
            prune_log(&store, &peers, PRUNE_BATCH).unwrap(),
            0,
            "c never reported"
        );
        record_acked(&store, "c", 3);
        record_acked(&store, "c", 2); // monotone: a stale reply lowers nothing
                                      // In batches, oldest first: the caller takes the lock per batch.
        assert_eq!(prune_log(&store, &peers, 2).unwrap(), 2);
        assert_eq!(
            prune_log(&store, &peers, 2).unwrap(),
            1,
            "the rest of the acked tail"
        );
        assert_eq!(prune_log(&store, &peers, 2).unwrap(), 0);
        let first: i64 = store
            .conn()
            .query_row("SELECT MIN(seq) FROM store_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, 4, "c still needs 4 and on");
    }

    // ── C2a: replicas complete enough to take a session over ────────

    /// A store with history and NO log yet: `recent` active now, `stale`
    /// last touched a month ago, and an IRC line (no session).
    fn store_with_history() -> (tempfile::TempDir, Store, crate::config::DirGuard) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
        let guard = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
            dir.path().to_path_buf(),
        )));
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let c = store.conn();
        c.execute_batch(
            "INSERT INTO sessions(id, title) VALUES('recent', 'r');
             INSERT INTO sessions(id, title, updated_at)
                 VALUES('stale', 's', datetime('now', '-30 days'));
             INSERT INTO messages(session_id, role, content) VALUES('recent', 'user', 'one');
             INSERT INTO messages(session_id, role, content) VALUES('recent', 'assistant', 'two');
             INSERT INTO messages(session_id, role, content) VALUES('stale', 'user', 'old');
             INSERT INTO todos(session_id, item_id, content, status, position)
                 VALUES('recent', 't1', 'do it', 'pending', 0);
             INSERT INTO session_overrides(session_id, model) VALUES('recent', 'm');
             INSERT INTO engine_notes(session_id, after_msg_id, kind, summary)
                 VALUES('recent', 2, 'guard', 'a note');
             INSERT INTO irc_lines(network, presence, buffer, ts, kind)
                 VALUES('n', 'p', '#c', 't', 'msg');",
        )
        .unwrap();
        store
            .session_prompt_put(
                "recent",
                &crate::store::SessionPrompt {
                    system_prompt: "the frozen prompt".into(),
                    tools_json: "[]".into(),
                    format: 1,
                    node: Some("node-a".into()),
                },
            )
            .unwrap();
        (dir, store, guard)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// The log only sees writes made after it is installed. Without a
    /// backfill a replica held the tail of a session and never its start
    /// — nothing a takeover could continue from.
    #[test]
    fn a_fresh_log_carries_the_recent_sessions_whole() {
        let (_d, store, _g) = store_with_history();
        let state = sync_log_install(&store, &["peer-b".to_string()], 14).unwrap();
        assert!(state.contains("backfilled"), "{state}");
        let tables: Vec<String> = log_rows(&store, 0, 1000)
            .unwrap()
            .into_iter()
            .map(|r| r.tbl)
            .collect();
        for t in [
            "sessions",
            "messages",
            "todos",
            "session_overrides",
            "engine_notes",
            "session_prompt",
        ] {
            assert!(tables.iter().any(|x| x == t), "{t} backfilled: {tables:?}");
        }
        assert!(
            !tables.iter().any(|x| x == "irc_lines"),
            "no session, no backfill"
        );
        assert_eq!(tables[0], "sessions", "the session row goes first");

        let conn = open_replica("node-a").unwrap();
        apply_offer(&conn, &build_offer(&store, "node-a", "a", 0).unwrap()).unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'recent'"
            ),
            2
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'stale'"
            ),
            0,
            "outside the window"
        );
        let prompt: String = conn
            .query_row(
                "SELECT system_prompt FROM session_prompt WHERE session_id = 'recent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            prompt, "the frozen prompt",
            "the cache's prefix travels too"
        );

        let before = log_len(&store);
        sync_log_install(&store, &["peer-b".to_string()], 14).unwrap();
        assert_eq!(
            log_len(&store),
            before,
            "a running log is not backfilled again"
        );
    }

    /// A frozen prompt written while the log runs is logged like any row.
    #[test]
    fn a_session_prompt_write_is_logged() {
        let (_d, store, _g) = scratch_primary("prompt-log");
        store
            .session_prompt_put(
                "s1",
                &crate::store::SessionPrompt {
                    system_prompt: "p".into(),
                    tools_json: String::new(),
                    format: 1,
                    node: None,
                },
            )
            .unwrap();
        let rows = log_rows(&store, 0, 10).unwrap();
        assert!(rows.iter().any(|r| r.tbl == "session_prompt"), "{rows:?}");
    }

    /// Every apply is INSERT OR REPLACE, whose replace fires no delete
    /// trigger: a message whose row arrived twice (an update, a backfill)
    /// was indexed twice and search listed it twice.
    #[test]
    fn a_message_that_arrives_twice_is_one_search_hit() {
        let (_d, store, _g) = scratch_primary("fts-twice");
        store
            .conn()
            .execute_batch(
                "INSERT INTO sessions(id) VALUES('s1');
                 INSERT INTO messages(session_id, role, content)
                     VALUES('s1', 'user', 'twice-needle first');
                 UPDATE messages SET content = 'twice-needle second' WHERE session_id = 's1';",
            )
            .unwrap();
        let conn = open_replica("nodeA").unwrap();
        apply_offer(&conn, &build_offer(&store, "nodeA", "a", 0).unwrap()).unwrap();
        let hits = search_replicas("twice-needle", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(
            hits[0].snippet.contains("second"),
            "the newest image is indexed"
        );
        store
            .conn()
            .execute("DELETE FROM messages WHERE session_id = 's1'", [])
            .unwrap();
        apply_offer(&conn, &build_offer(&store, "nodeA", "a", 3).unwrap()).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages_fts"), 0);
    }

    /// A replica made before the index was keyed by msg_id is rebuilt
    /// once when it is next opened.
    #[test]
    fn an_old_replica_index_is_rebuilt_once() {
        let (_d, store, _g) = scratch_primary("fts-old");
        store
            .conn()
            .execute_batch(
                "INSERT INTO sessions(id) VALUES('s1');
                 INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'old-needle');",
            )
            .unwrap();
        let conn = open_replica("nodeA").unwrap();
        apply_offer(&conn, &build_offer(&store, "nodeA", "a", 0).unwrap()).unwrap();
        // Back to the old shape: the old trigger, a doubled entry.
        conn.execute_batch(
            "DROP TRIGGER messages_fts_ai; DROP TRIGGER messages_fts_ad;
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                 INSERT INTO messages_fts(role, content, session_id, msg_id)
                 VALUES (new.role, new.content, new.session_id, new.msg_id);
             END;
             INSERT INTO messages_fts(role, content, session_id, msg_id)
                 SELECT role, content, session_id, msg_id FROM messages;
             DELETE FROM meta WHERE key = 'veles.replica_fts';",
        )
        .unwrap();
        drop(conn);
        let conn = open_replica("nodeA").unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages_fts"), 1);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'messages_ai'"
            ),
            0,
            "the old trigger is gone"
        );
    }

    /// A peer that joins after the log was pruned (or whose replica was
    /// wiped) asks for rows nobody holds any more. It used to refuse every
    /// offer as a gap, for ever; it now skips to the log's start and the
    /// sender backfills the recent sessions for it — once per floor.
    #[test]
    fn a_replica_behind_the_log_start_skips_to_it_and_gets_the_backfill() {
        let (_d, store, _g) = scratch_primary("floor");
        store
            .conn()
            .execute_batch(
                "INSERT INTO sessions(id) VALUES('s1');
                 INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'a');
                 INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'b');",
            )
            .unwrap();
        record_acked(&store, "b", 3);
        assert_eq!(
            prune_log(&store, &["b".to_string()], PRUNE_BATCH).unwrap(),
            3
        );
        assert_eq!(
            log_floor(&store).unwrap(),
            3,
            "an empty log keeps its floor"
        );

        let fresh = open_replica("nodeA").unwrap();
        let bare = build_offer(&store, "nodeA", "a", 0).unwrap();
        assert_eq!(bare.base, 3);
        let r = backfill_below_floor(&store, "c", 0, 14).unwrap();
        assert!(r.is_some_and(|r| r.rows == 3), "session + two messages");
        assert!(
            backfill_below_floor(&store, "c", 0, 14).unwrap().is_none(),
            "once per floor"
        );
        let offer = build_offer(&store, "nodeA", "a", 0).unwrap();
        let report = apply_offer(&fresh, &offer).unwrap();
        assert_eq!(report.rebased_from, Some(0));
        assert_eq!(count(&fresh, "SELECT COUNT(*) FROM messages"), 2);
        assert!(
            backfill_below_floor(&store, "c", report.final_seq, 14)
                .unwrap()
                .is_none(),
            "caught up: nothing to do"
        );
    }

    /// A caught-up peer's offers carry no base.
    #[test]
    fn an_offer_from_inside_the_log_has_no_base() {
        let (_d, store, _g) = scratch_primary("nobase");
        store
            .conn()
            .execute("INSERT INTO sessions(id) VALUES('s1')", [])
            .unwrap();
        assert_eq!(build_offer(&store, "n", "n", 0).unwrap().base, 0);
        assert_eq!(build_offer(&store, "n", "n", 1).unwrap().base, 0);
    }
}
