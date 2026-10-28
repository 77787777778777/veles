//! hermes → veles session-history importer (one-shot migration aid).
//!
//! User decision 2026-08-21: at cutover, import the hermes transcript
//! history into veles' store so session_search keeps working across the
//! boundary. Design constraints, all learned the hard way on this box:
//!
//!   - The hermes DB is opened READ-ONLY. Veles must never write into
//!     ~/.hermes/state.db — the 2026-08-09 poisoning incident
//!     (messages_ai trigger into hermes' FTS table) is the cautionary
//!     tale (HANDOFF pitfall 3). Read-only open makes that structural.
//!   - Idempotent: sessions already present in the veles store are
//!     skipped, so re-running after a partial import is safe.
//!   - Refuses while the veles gateway runs (the import is one big
//!     transaction per session; the gateway's own store connection
//!     could SQLITE_BUSY against it and vice versa).
//!   - Hermes `active=0` (compaction-archived) rows import with
//!     archived=1: FTS-searchable, never loaded into context — exactly
//!     hermes' own durable-lineage semantics (audited during the 2026-08
//!     hermes patch work; see git history).
//!   - Timestamps carry over (epoch REAL → ISO TEXT) so the imported
//!     history sorts chronologically next to native veles sessions.
//!
//! Reasoning mapping: hermes keeps reasoning_content (replay) and
//! reasoning (display); veles' single `reasoning` column is the replay
//! one, so reasoning_content wins, display-reasoning is the fallback.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

/// One hermes session as seen by the importer.
pub struct HermesSession {
    pub id: String,
    pub source: String,
    pub title: Option<String>,
    pub started_at_iso: String,
    pub last_activity_iso: String,
    pub message_count: usize,
}

/// Import report line per session.
pub struct ImportResult {
    pub session: HermesSession,
    pub status: Status,
}

pub enum Status {
    Imported { messages: usize },
    SkippedExists,
    SkippedEmpty,
}

fn epoch_to_iso(epoch: f64) -> String {
    chrono::DateTime::from_timestamp(epoch as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "1970-01-01 00:00:00".to_string())
}

/// Validate that the source DB is shaped like a hermes state.db (before
/// any query — a wrong file fails loudly, not with cryptic SQL errors).
fn validate_hermes_schema(conn: &Connection) -> Result<()> {
    let has_sessions: bool = conn
        .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='sessions'")?
        .query_row([], |r| r.get::<_, i64>(0))?
        > 0;
    let has_messages: bool = conn
        .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages'")?
        .query_row([], |r| r.get::<_, i64>(0))?
        > 0;
    if !has_sessions || !has_messages {
        return Err(anyhow!(
            "source DB has no sessions/messages tables — not a hermes state.db"
        ));
    }
    // Hermes-specific columns the importer relies on.
    for col in ["timestamp", "active", "reasoning_content"] {
        let n: i64 = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name=?1")?
            .query_row(params![col], |r| r.get(0))?;
        if n == 0 {
            return Err(anyhow!(
                "messages table lacks column `{col}` — not a hermes state.db (or a very old one)"
            ));
        }
    }
    // And make sure we never open VELES' db as the source (the guard is
    // symmetric: importing veles into itself duplicates everything).
    let has_msg_id: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name='msg_id'")?
        .query_row([], |r| r.get::<_, i64>(0))?
        > 0;
    if has_msg_id {
        return Err(anyhow!(
            "source DB has a veles-shaped messages table (msg_id) — refusing to import veles into itself"
        ));
    }
    Ok(())
}

/// Enumerate candidate sessions (filters applied at the SQL level).
pub fn list_sessions(
    conn: &Connection,
    source_filter: Option<&str>,
    since_days: Option<i64>,
) -> Result<Vec<HermesSession>> {
    // Build the filter clause; the source string rides a bound parameter
    // (rusqlite 0.31 has no Connection::quote — never interpolate it).
    let mut sql = String::from(
        "SELECT s.id, s.source, s.title, s.started_at,
                COALESCE(s.last_activity_at, s.started_at),
                (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
         FROM sessions s WHERE 1=1",
    );
    let mut source_param: Option<String> = None;
    if let Some(src) = source_filter {
        sql.push_str(" AND s.source = ?1");
        source_param = Some(src.to_string());
    }
    if let Some(days) = since_days {
        let cutoff = chrono::Utc::now().timestamp() - days * 86_400;
        sql.push_str(&format!(
            " AND COALESCE(s.last_activity_at, s.started_at) >= {cutoff}.0"
        ));
    }
    sql.push_str(" ORDER BY s.started_at");
    let mut stmt = conn.prepare(&sql)?;
    let rows = match &source_param {
        Some(src) => stmt.query_map(params![src], row_to_session)?,
        None => stmt.query_map([], row_to_session)?,
    };
    let mut out = Vec::new();
    for r in rows {
        let (id, source, title, started, last, count) = r?;
        out.push(HermesSession {
            id,
            source,
            title: title.filter(|t| !t.trim().is_empty()),
            started_at_iso: epoch_to_iso(started),
            last_activity_iso: epoch_to_iso(last),
            message_count: count,
        });
    }
    Ok(out)
}

fn row_to_session(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, Option<String>, f64, f64, usize)> {
    Ok((
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, Option<String>>(2)?,
        r.get::<_, f64>(3)?,
        r.get::<_, f64>(4)?,
        r.get::<_, usize>(5)?,
    ))
}

/// Import one session's messages from the hermes DB. Returns the row
/// tuples for Store::import_session_batch:
/// (role, content, reasoning, tool_calls, tool_call_id, tool_name, created_at, archived).
#[allow(clippy::type_complexity)]
fn read_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<
    Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        bool,
    )>,
> {
    let mut stmt = conn.prepare(
        "SELECT role, content, reasoning, reasoning_content, tool_calls,
                tool_call_id, tool_name, timestamp,
                COALESCE(active, 1)
         FROM messages WHERE session_id = ?1 ORDER BY timestamp, id",
    )?;
    let rows = stmt.query_map(params![session_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, f64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (role, content, reasoning, reasoning_content, tool_calls, tcid, tname, ts, active) = r?;
        // content is NOT NULL in veles; hermes allows NULL (pure
        // tool-call assistant rows). Keep the row with empty text so
        // the transcript structure survives.
        let content = content.unwrap_or_default();
        // Veles' reasoning column is the REPLAY one: prefer
        // reasoning_content, fall back to the display copy.
        let reasoning = reasoning_content.or(reasoning);
        // Validate tool_calls is JSON before carrying it (hermes writes
        // it as a JSON string; a corrupt row shouldn't sink the import).
        let tool_calls = tool_calls.filter(|tc| serde_json::from_str::<Value>(tc).is_ok());
        out.push((
            role,
            content,
            reasoning,
            tool_calls,
            tcid,
            tname,
            epoch_to_iso(ts),
            active == 0,
        ));
    }
    Ok(out)
}

/// The whole import. `store` is the LIVE veles store (already open);
/// the hermes DB is opened read-only here.
pub fn run(
    store: &crate::store::Store,
    hermes_db: &Path,
    source_filter: Option<&str>,
    since_days: Option<i64>,
    dry_run: bool,
) -> Result<Vec<ImportResult>> {
    if crate::backup::gateway_alive() {
        return Err(anyhow!(
            "the veles gateway is running — stop it first (`veles gateway stop`)"
        ));
    }
    let conn = Connection::open_with_flags(
        hermes_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| anyhow!("cannot open {} read-only: {e}", hermes_db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    validate_hermes_schema(&conn)?;

    let sessions = list_sessions(&conn, source_filter, since_days)?;
    let mut results = Vec::new();
    for s in sessions {
        if s.message_count == 0 {
            results.push(ImportResult {
                session: s,
                status: Status::SkippedEmpty,
            });
            continue;
        }
        if store.session_exists(&s.id)? {
            results.push(ImportResult {
                session: s,
                status: Status::SkippedExists,
            });
            continue;
        }
        if dry_run {
            results.push(ImportResult {
                session: s,
                status: Status::Imported { messages: 0 },
            });
            continue;
        }
        let rows = read_messages(&conn, &s.id)?;
        // Split the archived flag out of the tuple shape the store wants.
        let archived_flags: Vec<bool> = rows.iter().map(|r| r.7).collect();
        let batch: Vec<crate::store::ImportMessageRow> = rows
            .into_iter()
            .map(
                |(role, content, reasoning, tool_calls, tcid, tname, cat, _arch)| {
                    (role, content, reasoning, tool_calls, tcid, tname, cat)
                },
            )
            .collect();
        let count = batch.len();
        store.import_session_batch(
            &s.id,
            s.title.as_deref(),
            &s.started_at_iso,
            &s.last_activity_iso,
            &batch,
        )?;
        // Mark hermes-archived (compacted) rows archived in veles too:
        // searchable, never loaded into context.
        store.mark_imported_archived(&s.id, &archived_flags)?;
        results.push(ImportResult {
            session: s,
            status: Status::Imported { messages: count },
        });
    }
    Ok(results)
}
