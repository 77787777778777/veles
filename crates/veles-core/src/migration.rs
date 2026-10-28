//! Session migration between pool nodes (B6): quiesce → copy → adopt
//! → fence. PLAN-compute-pool.md §"Session migration between nodes"
//! with the five design questions ANSWERED there (2026-09-28):
//!
//!   1. the parent/child fan migrates as a SUBTREE unit; migrating a
//!      child whose parent stays is refused by name (--force orphans
//!      honestly);
//!   2. the prompt-cache re-pay is WARNED, never gated;
//!   3. the retired source TOMBSTONES in place — `sessions.node =
//!      migrated_to(<target>@<epoch>)`, rows stay (they are exactly
//!      what B5 replicates);
//!   4. checkpoint blobs ship INSIDE the bundle keyed
//!      `<session>/<stamp>-<name>` and adopt rewrites the absolute
//!      `blob` column against the target's own root;
//!   5. adoption is one-step behind its OWN allowlist,
//!      `[cluster] migrate_peers` — an allowlisted push auto-adopts
//!      (the task_peers precedent).
//!
//! THE INVARIANT the protocol exists to protect: exactly one writer
//! at every instant. `migrating_to` is a third state beside owned and
//! NULL that EVERY turn-writer refuses (the fence lives in
//! `Agent::run_turn_inner`, the one chokepoint all four surfaces pass
//! through). The split-brain case — the target adopted but the source
//! never saw the reply — fails closed: the source QUERIES the target
//! before it may un-park, and un-parks only on an explicit "not
//! adopted" answer.
//!
//! What does NOT travel (plan non-goals, stated to the operator by the
//! migrate command, never silently): in-flight turn state (the fence
//! waits for the boundary), background shells/subagents/monitors
//! (process-local; their FILES persist, their completions never arrive
//! on the target), and the usage ledger (billing stays where it was
//! spent). Prompt-cache warmth DOES travel since 2026-09-29: the frozen
//! system prompt and tool list ride the bundle byte for byte, and the
//! move reaches the model as one injected row after that prefix.

use crate::store::Store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The `sessions.node` value that parks a session mid-handoff: no
/// writer may run a turn under it, and the only exits are adoption on
/// the target or an explicit un-park/reclaim on the source.
pub const PARK_PREFIX: &str = "migrating_to:";
/// The tombstone a retired source stamps (question 3): the rows stay
/// — they are the B5 replica's content — the POINTER is the record.
pub const RETIRED_PREFIX: &str = "migrated_to:";

pub fn is_parked(node: Option<&str>) -> bool {
    node.is_some_and(|n| n.starts_with(PARK_PREFIX))
}

pub fn is_retired(node: Option<&str>) -> bool {
    node.is_some_and(|n| n.starts_with(RETIRED_PREFIX))
}

/// The park target of a `migrating_to:<target>` stamp.
pub fn park_target(node: Option<&str>) -> Option<String> {
    node.and_then(|n| n.strip_prefix(PARK_PREFIX))
        .map(str::to_string)
}

// ── the subtree (question 1) ────────────────────────────────────────

/// A session and every descendant, parents before children (the
/// insert order an adopt needs). A cycle in `parent` — which the store
/// does not promise is impossible — terminates the walk: a session
/// already in the set is never re-added.
pub fn session_subtree(store: &Store, root: &str) -> Result<Vec<SessionRow>> {
    session_subtree_on(store.conn(), root)
}

/// [`session_subtree`] over any connection with the sessions table — a
/// replica file too, which is where a takeover reads it.
pub fn session_subtree_on(conn: &rusqlite::Connection, root: &str) -> Result<Vec<SessionRow>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(row) = session_row(conn, &id)? {
            out.push(row);
        }
        for child in child_ids(conn, &id)? {
            stack.push(child);
        }
    }
    // The stack walk is pre-order DFS, so a parent always lands before
    // its children — but fill the depths too, so every caller (adopt's
    // insert order, the pins) reads them without a second pass.
    with_depths(&mut out, root);
    out.sort_by_key(|r| r.depth);
    Ok(out)
}

/// One `sessions` row as the bundle carries it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub title: Option<String>,
    pub parent: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// The node stamp as found (informational; adopt re-stamps).
    pub node: Option<String>,
    /// Depth in the subtree walk (root = 0). Adopt inserts in this
    /// order so a `parent` always lands before its child references
    /// it. Not the store's truth — derived, and re-derivable.
    pub depth: u32,
}

fn session_row(conn: &rusqlite::Connection, id: &str) -> Result<Option<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, parent, created_at, updated_at, node FROM sessions WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![id], |r| {
        Ok(SessionRow {
            id: r.get(0)?,
            title: r.get(1)?,
            parent: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
            node: r.get(5)?,
            depth: 0,
        })
    })?;
    Ok(rows.next().transpose()?)
}

fn child_ids(conn: &rusqlite::Connection, id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM sessions WHERE parent = ?1")?;
    let rows = stmt
        .query_map(rusqlite::params![id], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The subtree walk with depths filled — `session_subtree`'s rows
/// carry the depth an adopt's insert order needs. (Split so the walk
/// above stays readable; the cost is one extra pass over a list that
/// is dozens of rows at most.)
fn with_depths(rows: &mut [SessionRow], root: &str) {
    // A parent map first: walking `rows` while mutating it is a
    // borrow error, and cloning dozens of (id → parent) pairs costs
    // nothing.
    let parents: std::collections::HashMap<String, Option<String>> = rows
        .iter()
        .map(|r| (r.id.clone(), r.parent.clone()))
        .collect();
    for r in rows.iter_mut() {
        let mut d = 0u32;
        let mut cur = r.parent.clone();
        while let Some(p) = cur {
            if p == root {
                d += 1;
                break;
            }
            let Some(next) = parents.get(&p) else {
                break;
            };
            d += 1;
            cur = next.clone();
            if d > 64 {
                break; // a cycle guard, not a real depth
            }
        }
        r.depth = d;
    }
}

// ── the bundle ──────────────────────────────────────────────────────

/// One message row as it travels. The SOURCE msg_id rides along:
/// msg_id is per-DB AUTOINCREMENT, so the target assigns fresh ones,
/// and the mapping lands in `migration_msg_map` so a reader holding a
/// source-era id (a search hit from a replica, a scroll anchor) can
/// still be reconciled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub source_msg_id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub injection: Option<String>,
    pub created_at: String,
    /// Compacted-away history travels too, still archived: it is what
    /// search recall and the transcript's "earlier" rows read.
    #[serde(default)]
    pub archived: bool,
}

/// One engine note (a guard, a compaction, a failover…). Notes are
/// transcript entries — they must keep their place, so the anchor
/// travels as the SOURCE msg id and adopt maps it onto the local one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteRow {
    pub session_id: String,
    pub after_source_msg_id: i64,
    pub kind: String,
    pub summary: String,
    pub detail: Option<String>,
    pub toggle: Option<String>,
    pub created_at: String,
}

/// A session's frozen prompt as it travels: the system prompt and the
/// tool list its requests carried on the source, byte for byte. The
/// target sends the same bytes, so the provider's prompt cache — keyed on
/// the account and that prefix, not on the machine — is still warm
/// (USER 2026-09-29: "why can't it preserve cache").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRow {
    pub session_id: String,
    pub system_prompt: String,
    pub tools_json: String,
    pub format: i64,
    pub node: Option<String>,
}

/// One checkpoint row + its blob (question 4). `blob` in the STORE is
/// an absolute path under the owner's root — meaningless on the
/// target, so the bundle carries the BYTES (base64 by serde on a
/// `Vec<u8>` is wasteful; we carry the hex-sha'd bytes as a String of
/// base64 explicitly) and the relative key; adopt rewrites the path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub source_id: i64,
    pub session_id: String,
    pub path: String,
    /// base64 of the blob bytes; None when the file did not exist at
    /// snapshot time (the store allows a NULL blob).
    pub blob_b64: Option<String>,
    pub sha256: Option<String>,
    pub bytes: i64,
    pub tool: String,
    pub note: Option<String>,
    pub created_at: String,
    /// The blob's key INSIDE the bundle: `<session>/<file name>`,
    /// session-scoped so two nodes can never collide (question 4).
    pub blob_key: String,
}

/// The queued work (arrived-mid-turn text) — R4.6's parking lot. A
/// `running` row is demoted to `pending` by the drainer before this
/// is ever built (quiesce refuses otherwise), so only pending rows
/// travel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueRow {
    pub session_id: String,
    pub seq: i64,
    pub body: String,
    pub kind: String,
    /// Where this row's answer belongs: the platform-native id of the
    /// message that parked it, and the adapter instance that carried it.
    /// They travel with the row because a session that moves between
    /// nodes keeps its parked work, and draining it on the new node has
    /// to answer under the message that asked — the whole reason the
    /// columns exist (`Store::queue_push_routed`). Both default for a row
    /// bundled by an older build.
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
}

/// Overrides, locale and todos — one small table each, full copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideRow {
    pub session_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoRow {
    pub session_id: String,
    pub item_id: String,
    pub content: String,
    pub status: String,
    pub position: i64,
}

/// One migration bundle: everything that travels for one subtree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationBundle {
    pub bundle_id: String,
    pub from_node: String,
    pub from_name: String,
    /// The node the source was told to migrate TO — the adopter
    /// verifies this names ITSELF (a bundle pushed onward or
    /// misaddressed is refused, not adopted by whoever catches it).
    pub to_node: String,
    pub created_at: String,
    pub sessions: Vec<SessionRow>,
    pub messages: Vec<MessageRow>,
    pub checkpoints: Vec<CheckpointRow>,
    pub queue: Vec<QueueRow>,
    pub overrides: Vec<OverrideRow>,
    pub locale: Vec<(String, String)>,
    pub todos: Vec<TodoRow>,
    #[serde(default)]
    pub notes: Vec<NoteRow>,
    #[serde(default)]
    pub prompts: Vec<PromptRow>,
}

impl MigrationBundle {
    /// What the operator's cost warning names (question 2): the
    /// subtree's context size in chars and a token estimate.
    pub fn context_chars(&self) -> usize {
        self.messages
            .iter()
            .map(|m| m.content.len() + m.reasoning.as_ref().map(|r| r.len()).unwrap_or(0))
            .sum()
    }
}

/// Build the bundle for one subtree: every travelling row read out of
/// the source store, checkpoint blobs read off disk. The source must
/// have QUIESCED first (no live turn, no `running` queue row) — the
/// caller checks; building from a live session would copy a moving
/// target and the fence exists to make that impossible, not merely
/// unlikely.
pub fn build_bundle(
    store: &Store,
    from_node: &str,
    from_name: &str,
    to_node: &str,
    root: &str,
) -> Result<MigrationBundle> {
    build_bundle_on(store.conn(), true, from_node, from_name, to_node, root)
}

/// [`build_bundle`] over any connection. `read_blobs = false` is a
/// replica's case: its checkpoint rows name files on ANOTHER machine, so
/// no blob is read (a same-named local path would be the wrong file) and
/// each travels as absent — named in the adopt report.
pub fn build_bundle_on(
    conn: &rusqlite::Connection,
    read_blobs: bool,
    from_node: &str,
    from_name: &str,
    to_node: &str,
    root: &str,
) -> Result<MigrationBundle> {
    let sessions = session_subtree_on(conn, root)?;
    if sessions.is_empty() {
        anyhow::bail!("no such session: {root}");
    }
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    // A replica has the columns its rows brought: `archived` exists once
    // any message row with it was applied.
    let archived = {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('messages')")?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if cols.iter().any(|c| c == "archived") {
            "COALESCE(archived, 0)"
        } else {
            "0"
        }
    };

    let mut messages = Vec::new();
    for sid in &ids {
        let mut stmt = conn.prepare(&format!(
            "SELECT msg_id, session_id, role, content, reasoning, tool_calls, tool_call_id,
                    tool_name, injection, COALESCE(created_at, ''), {archived}
             FROM messages WHERE session_id = ?1
             ORDER BY msg_id"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok(MessageRow {
                    source_msg_id: r.get(0)?,
                    session_id: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                    reasoning: r.get(4)?,
                    tool_calls: r.get(5)?,
                    tool_call_id: r.get(6)?,
                    tool_name: r.get(7)?,
                    injection: r.get(8)?,
                    created_at: r.get(9)?,
                    archived: r.get::<_, i64>(10)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        messages.extend(rows);
    }

    let mut checkpoints = Vec::new();
    let mut missing_blobs = Vec::new();
    for sid in &ids {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, path, blob, sha256, bytes, tool, note,
                    COALESCE(created_at, '')
             FROM checkpoints WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, session, path, blob, sha, bytes, tool, note, created) in rows {
            use base64::Engine as _;
            // The blob column is an ABSOLUTE path under the source's
            // root. Read the bytes; a blob that is gone is named in
            // the report, never silently dropped (the plan's
            // "withheld and named" discipline).
            let (blob_b64, key) = match &blob {
                None => (
                    None,
                    format!("{}/{id}-absent", crate::checkpoints::sanitize(&session)),
                ),
                Some(p) => {
                    let file = std::path::Path::new(p);
                    let name = file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("cp-{id}"));
                    let key = format!("{}/{name}", crate::checkpoints::sanitize(&session));
                    if !read_blobs {
                        checkpoints.push(CheckpointRow {
                            source_id: id,
                            session_id: session,
                            path,
                            blob_b64: None,
                            sha256: sha,
                            bytes,
                            tool,
                            note,
                            created_at: created,
                            blob_key: key,
                        });
                        continue;
                    }
                    match std::fs::read(file) {
                        Ok(b) => (
                            Some(base64::engine::general_purpose::STANDARD.encode(&b)),
                            key,
                        ),
                        Err(e) => {
                            missing_blobs.push(format!("checkpoint #{id} ({p}): {e}"));
                            (None, key)
                        }
                    }
                }
            };
            checkpoints.push(CheckpointRow {
                source_id: id,
                session_id: session,
                path,
                blob_b64,
                sha256: sha,
                bytes,
                tool,
                note,
                created_at: created,
                blob_key: key,
            });
        }
    }
    if !missing_blobs.is_empty() {
        tracing::warn!(
            "migration bundle for {root}: {} checkpoint blob(s) unreadable: {}",
            missing_blobs.len(),
            missing_blobs.join("; ")
        );
    }

    let mut queue = Vec::new();
    let mut overrides = Vec::new();
    let mut locale = Vec::new();
    let mut todos = Vec::new();
    let mut notes = Vec::new();
    let mut prompts = Vec::new();
    for sid in &ids {
        let mut stmt = conn.prepare(
            "SELECT system_prompt, tools_json, format, node FROM session_prompt
             WHERE session_id = ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok(PromptRow {
                    session_id: sid.to_string(),
                    system_prompt: r.get(0)?,
                    tools_json: r.get(1)?,
                    format: r.get(2)?,
                    node: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        prompts.extend(rows);
        let mut stmt = conn.prepare(
            "SELECT session_id, after_msg_id, kind, summary, detail, toggle,
                    COALESCE(created_at, '')
             FROM engine_notes WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok(NoteRow {
                    session_id: r.get(0)?,
                    after_source_msg_id: r.get(1)?,
                    kind: r.get(2)?,
                    summary: r.get(3)?,
                    detail: r.get(4)?,
                    toggle: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        notes.extend(rows);

        let mut stmt = conn.prepare(
            "SELECT session_id, seq, body, kind, reply_to, route FROM session_queue
             WHERE session_id = ?1 AND state IN ('pending', 'held') ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok(QueueRow {
                    session_id: r.get(0)?,
                    seq: r.get(1)?,
                    body: r.get(2)?,
                    kind: r.get(3)?,
                    reply_to: r.get(4)?,
                    route: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        queue.extend(rows);

        let mut stmt = conn.prepare(
            "SELECT session_id, provider, model, reasoning FROM session_overrides
             WHERE session_id = ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok(OverrideRow {
                    session_id: r.get(0)?,
                    provider: r.get(1)?,
                    model: r.get(2)?,
                    reasoning: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        overrides.extend(rows);

        if let Some(l) = conn
            .query_row(
                "SELECT locale FROM session_locale WHERE session_id = ?1",
                rusqlite::params![sid],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            locale.push((sid.to_string(), l));
        }

        let mut stmt = conn.prepare(
            "SELECT session_id, item_id, content, status, position FROM todos
             WHERE session_id = ?1 ORDER BY position",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![sid], |r| {
                Ok(TodoRow {
                    session_id: r.get(0)?,
                    item_id: r.get(1)?,
                    content: r.get(2)?,
                    status: r.get(3)?,
                    position: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        todos.extend(rows);
    }

    Ok(MigrationBundle {
        bundle_id: crate::a2a::new_task_id(),
        from_node: from_node.to_string(),
        from_name: from_name.to_string(),
        to_node: to_node.to_string(),
        created_at: crate::a2a::now_iso(),
        sessions,
        messages,
        checkpoints,
        queue,
        overrides,
        locale,
        todos,
        notes,
        prompts,
    })
}

/// Give every session of a bundle a new id (`ids`: old → new), with every
/// row, parent link and blob key following — a fork is the same history
/// under a session id of its own. Titles say so.
pub fn rename_sessions(
    bundle: &mut MigrationBundle,
    ids: &std::collections::HashMap<String, String>,
) {
    let new = |id: &str| ids.get(id).cloned().unwrap_or_else(|| id.to_string());
    for s in &mut bundle.sessions {
        let old = std::mem::take(&mut s.id);
        s.title = Some(format!(
            "{} (fork)",
            s.title.clone().unwrap_or_else(|| old.clone())
        ));
        s.id = new(&old);
        s.parent = s.parent.as_deref().map(new);
    }
    for m in &mut bundle.messages {
        m.session_id = new(&m.session_id);
    }
    for c in &mut bundle.checkpoints {
        c.session_id = new(&c.session_id);
        let name = c
            .blob_key
            .split_once('/')
            .map(|(_, n)| n.to_string())
            .unwrap_or_default();
        c.blob_key = format!("{}/{name}", crate::checkpoints::sanitize(&c.session_id));
    }
    for q in &mut bundle.queue {
        q.session_id = new(&q.session_id);
    }
    for o in &mut bundle.overrides {
        o.session_id = new(&o.session_id);
    }
    for (sid, _) in &mut bundle.locale {
        *sid = new(sid);
    }
    for t in &mut bundle.todos {
        t.session_id = new(&t.session_id);
    }
    for n in &mut bundle.notes {
        n.session_id = new(&n.session_id);
    }
    for p in &mut bundle.prompts {
        p.session_id = new(&p.session_id);
    }
}

// ── the adopter ─────────────────────────────────────────────────────

/// One plain path component: non-empty, no separator, no leading dot
/// (so neither `.` nor `..`), no NUL.
fn plain_part(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && !s.contains(['/', '\\', '\0'])
}

/// Everything in a bundle that picks a row or a file is the SENDER's
/// claim, so it is checked before anything lands:
///
/// - every child row (messages, checkpoints, queue, overrides, locale,
///   todos) belongs to a session the bundle itself carries — adopt must
///   never write into a local session the bundle only names;
/// - a checkpoint's `blob_key` is exactly `<its session's blob dir>/<file
///   name>` (the dir is `checkpoints::sanitize(session)`, as the source
///   stores it), both plain path parts, so the blob lands under this
///   node's root and nowhere else.
fn validate_bundle(bundle: &MigrationBundle) -> Result<()> {
    let ids: std::collections::HashSet<&str> =
        bundle.sessions.iter().map(|s| s.id.as_str()).collect();
    let foreign = bundle
        .messages
        .iter()
        .map(|m| m.session_id.as_str())
        .chain(bundle.checkpoints.iter().map(|c| c.session_id.as_str()))
        .chain(bundle.queue.iter().map(|q| q.session_id.as_str()))
        .chain(bundle.overrides.iter().map(|o| o.session_id.as_str()))
        .chain(bundle.locale.iter().map(|(sid, _)| sid.as_str()))
        .chain(bundle.todos.iter().map(|t| t.session_id.as_str()))
        .chain(bundle.notes.iter().map(|n| n.session_id.as_str()))
        .chain(bundle.prompts.iter().map(|p| p.session_id.as_str()))
        .find(|sid| !ids.contains(sid));
    if let Some(sid) = foreign {
        anyhow::bail!(
            "the bundle carries rows for session {sid}, which it does not migrate — refused whole"
        );
    }
    for c in &bundle.checkpoints {
        let ok = c.blob_key.split_once('/').is_some_and(|(dir, name)| {
            dir == crate::checkpoints::sanitize(&c.session_id)
                && plain_part(dir)
                && plain_part(name)
        });
        anyhow::ensure!(
            ok,
            "checkpoint #{} has blob key {:?} — it must be <session>/<file name>; refused whole",
            c.source_id,
            c.blob_key
        );
    }
    Ok(())
}

/// What an adopt did, as the target reports it back.
#[derive(Debug, Default, Serialize)]
pub struct AdoptReport {
    pub bundle_id: String,
    pub adopted_node: String,
    pub sessions: usize,
    pub messages: usize,
    pub checkpoints: usize,
    pub queue: usize,
    /// Blobs named in the bundle but absent/unreadable — stated, never
    /// silent.
    pub missing_blobs: Vec<String>,
    /// Sessions the target ALREADY held live (a re-push after a lost
    /// reply): idempotent, reported, not re-adopted.
    pub already_live: Vec<String>,
}

/// Adopt one bundle: everything lands in ONE transaction, sessions
/// stamped with THIS node, message rows re-inserted under fresh
/// AUTOINCREMENT ids with the source mapping recorded, checkpoint
/// blobs written under the target's own root and their paths
/// rewritten. Idempotent: a session already live here (a re-push
/// after a lost reply) is reported and skipped, so a retry never
/// doubles history.
pub fn adopt(store: &Store, me_node: &str, bundle: &MigrationBundle) -> Result<AdoptReport> {
    let here = moved_note(&bundle.from_name, &bundle.from_node, me_node);
    adopt_with(store, me_node, bundle, Some(here), &|_| Ok(()))
}

/// [`adopt`], with the injected row the model reads first (`note`: where
/// the session came from and what that means — a move, a takeover, a
/// fork), and `in_tx` run inside the adopt's transaction before it
/// commits: a takeover records itself there, a fork tombstones its
/// original, so neither can land half-done.
pub fn adopt_with(
    store: &Store,
    me_node: &str,
    bundle: &MigrationBundle,
    note: Option<String>,
    in_tx: &dyn Fn(&rusqlite::Connection) -> Result<()>,
) -> Result<AdoptReport> {
    if bundle.to_node != me_node {
        anyhow::bail!(
            "the bundle is addressed to node '{}' — this node is '{}'; refusing to adopt \
             somebody else's migration",
            bundle.to_node,
            me_node
        );
    }
    validate_bundle(bundle)?;
    use base64::Engine as _;
    let mut report = AdoptReport {
        bundle_id: bundle.bundle_id.clone(),
        adopted_node: me_node.to_string(),
        missing_blobs: Vec::new(),
        ..Default::default()
    };

    let conn = store.conn();
    // The msg-id mapping is the TARGET's local record of what it
    // adopted (source msg_id ↔ its fresh AUTOINCREMENT id) — how a
    // reader holding a source-era id (a replica search hit, a scroll
    // anchor) reconciles. Node-local by nature: NOT in
    // REPLICATED_TABLES, and created here because adopt is its only
    // writer.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_msg_map (
            session_id TEXT NOT NULL,
            local_msg_id INTEGER NOT NULL,
            source_node TEXT NOT NULL,
            source_msg_id INTEGER NOT NULL,
            PRIMARY KEY (session_id, source_node, source_msg_id)
        );
        CREATE INDEX IF NOT EXISTS idx_msg_map_local
            ON migration_msg_map(session_id, local_msg_id);",
    )?;
    // Blob files of a tombstone this adopt replaces — removed only after
    // COMMIT, and never one this adopt just wrote under the same name.
    let mut stale_blobs: Vec<std::path::PathBuf> = Vec::new();
    let mut written_blobs: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        // Which sessions are already live HERE (idempotent re-push)?
        let mut already = std::collections::HashSet::new();
        for s in &bundle.sessions {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1 AND (node IS NULL OR node NOT LIKE ?2)",
                rusqlite::params![s.id, format!("{RETIRED_PREFIX}%")],
                |r| r.get(0),
            )?;
            if n > 0 {
                already.insert(s.id.clone());
            }
        }

        // A TOMBSTONE of a bundle session is this node's copy from before
        // the session left (A→B→A). The bundle carries the whole history
        // again, so the old rows go first — adopting on top of them
        // doubled every message. Usage rows stay: billing belongs to the
        // node that spent it.
        for s in &bundle.sessions {
            if already.contains(&s.id) {
                continue;
            }
            let mut stmt = conn.prepare(
                "SELECT blob FROM checkpoints WHERE session_id = ?1 AND blob IS NOT NULL",
            )?;
            let blobs = stmt
                .query_map(rusqlite::params![s.id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            stale_blobs.extend(blobs.into_iter().map(std::path::PathBuf::from));
            for sql in [
                "DELETE FROM messages_fts WHERE session_id = ?1",
                "DELETE FROM messages WHERE session_id = ?1",
                "DELETE FROM checkpoints WHERE session_id = ?1",
                "DELETE FROM session_queue WHERE session_id = ?1",
                "DELETE FROM todos WHERE session_id = ?1",
                "DELETE FROM session_overrides WHERE session_id = ?1",
                "DELETE FROM session_locale WHERE session_id = ?1",
                "DELETE FROM engine_notes WHERE session_id = ?1",
                "DELETE FROM migration_msg_map WHERE session_id = ?1",
                "DELETE FROM session_prompt WHERE session_id = ?1",
            ] {
                conn.execute(sql, rusqlite::params![s.id])?;
            }
        }

        for s in &bundle.sessions {
            if already.contains(&s.id) {
                // Idempotent re-push: a session already live here is
                // NOT touched at all — a rename or retitle since the
                // first adopt is this node's truth, and the bundle's
                // copy is stale metadata. (Its messages/queue/etc are
                // skipped below by the same set.)
                continue;
            }
            // INSERT OR REPLACE: the row this node adopts is the
            // truth from now on; a tombstone or a stale copy of the
            // same id is exactly what adoption replaces.
            conn.execute(
                "INSERT OR REPLACE INTO sessions (id, title, parent, created_at, updated_at, node)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    s.id,
                    s.title,
                    // A parent that is NOT in the bundle and NOT live
                    // in this store would dangle across files — orphan
                    // it honestly rather than point at a row this
                    // store does not have (question 1's --force shape,
                    // enforced structurally at adopt time too).
                    s.parent.as_ref().and_then(|p| {
                        let in_bundle = bundle.sessions.iter().any(|b| b.id == *p);
                        let live_here: i64 = conn
                            .query_row(
                                "SELECT COUNT(*) FROM sessions WHERE id = ?1 AND (node IS NULL OR node NOT LIKE ?2)",
                                rusqlite::params![p, format!("{RETIRED_PREFIX}%")],
                                |r| r.get(0),
                            )
                            .unwrap_or(0);
                        if in_bundle || live_here > 0 {
                            Some(p.clone())
                        } else {
                            None
                        }
                    }),
                    s.created_at,
                    crate::a2a::now_iso(),
                    me_node,
                ],
            )?;
            report.sessions += 1;
        }

        // source msg id → local msg id, per session: the notes' anchors.
        let mut local_of: std::collections::HashMap<&str, std::collections::BTreeMap<i64, i64>> =
            std::collections::HashMap::new();
        for m in &bundle.messages {
            if already.contains(&m.session_id) {
                continue; // history of an already-live session stays as-is
            }
            // The FTS insert trigger (store.rs) populates the index —
            // one insert path, replica and live alike.
            conn.execute(
                "INSERT INTO messages (session_id, role, content, reasoning, tool_calls,
                                       tool_call_id, tool_name, injection, created_at, archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    m.session_id,
                    m.role,
                    m.content,
                    m.reasoning,
                    m.tool_calls,
                    m.tool_call_id,
                    m.tool_name,
                    m.injection,
                    m.created_at,
                    m.archived as i64,
                ],
            )?;
            let local: i64 = conn.last_insert_rowid();
            local_of
                .entry(m.session_id.as_str())
                .or_default()
                .insert(m.source_msg_id, local);
            conn.execute(
                "INSERT INTO migration_msg_map (session_id, local_msg_id, source_node, source_msg_id)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![m.session_id, local, bundle.from_node, m.source_msg_id],
            )?;
            report.messages += 1;
        }

        let blob_root = crate::checkpoints::blob_root();
        for c in &bundle.checkpoints {
            if already.contains(&c.session_id) {
                continue;
            }
            // Write the blob under THIS node's root, then store the
            // rewritten absolute path (question 4).
            let blob_path = match &c.blob_b64 {
                None => {
                    report
                        .missing_blobs
                        .push(format!("checkpoint #{} ({})", c.source_id, c.path));
                    None
                }
                Some(b64) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64.as_bytes())
                        .context("checkpoint blob is not base64")?;
                    let rel = std::path::Path::new(&c.blob_key);
                    let dest = blob_root.join(rel);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &bytes)?;
                    written_blobs.insert(dest.clone());
                    Some(dest.display().to_string())
                }
            };
            conn.execute(
                "INSERT INTO checkpoints (session_id, path, blob, sha256, bytes, tool, note, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    c.session_id,
                    c.path,
                    blob_path,
                    c.sha256,
                    c.bytes,
                    c.tool,
                    c.note,
                    c.created_at,
                ],
            )?;
            report.checkpoints += 1;
        }

        // Queued messages arrive HELD (D2): each would run as a turn
        // here, and a node must not be able to plant turns on another by
        // shipping it a session. `/queue run` releases them.
        for q in &bundle.queue {
            if already.contains(&q.session_id) {
                continue;
            }
            // The row's threading target travels with it: the parked
            // work keeps where its answer belongs across the move, and a
            // drain on THIS node answers under the message that asked.
            conn.execute(
                "INSERT OR REPLACE INTO session_queue
                 (session_id, seq, body, state, kind, reply_to, route)
                 VALUES (?1, ?2, ?3, 'held', ?4, ?5, ?6)",
                rusqlite::params![q.session_id, q.seq, q.body, q.kind, q.reply_to, q.route],
            )?;
            report.queue += 1;
        }

        for o in &bundle.overrides {
            if already.contains(&o.session_id) {
                continue;
            }
            conn.execute(
                "INSERT OR REPLACE INTO session_overrides (session_id, provider, model, reasoning, updated_at)
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                rusqlite::params![o.session_id, o.provider, o.model, o.reasoning],
            )?;
        }
        for (sid, loc) in &bundle.locale {
            if already.contains(sid) {
                continue;
            }
            conn.execute(
                "INSERT OR REPLACE INTO session_locale (session_id, locale) VALUES (?1, ?2)",
                rusqlite::params![sid, loc],
            )?;
        }
        for t in &bundle.todos {
            if already.contains(&t.session_id) {
                continue;
            }
            conn.execute(
                "INSERT OR REPLACE INTO todos (session_id, item_id, content, status, position, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                rusqlite::params![t.session_id, t.item_id, t.content, t.status, t.position],
            )?;
        }

        // Notes keep their place: the anchor is the local id of the last
        // travelled message at or before the source anchor (0 = before
        // any message, as the source had it).
        for n in &bundle.notes {
            if already.contains(&n.session_id) {
                continue;
            }
            let after = local_of
                .get(n.session_id.as_str())
                .and_then(|m| m.range(..=n.after_source_msg_id).next_back())
                .map(|(_, local)| *local)
                .unwrap_or(0);
            conn.execute(
                "INSERT INTO engine_notes (session_id, after_msg_id, kind, summary, detail, toggle, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    n.session_id,
                    after,
                    n.kind,
                    n.summary,
                    n.detail,
                    n.toggle,
                    n.created_at
                ],
            )?;
        }

        // The frozen prompts, as the source's requests carried them.
        for p in &bundle.prompts {
            if already.contains(&p.session_id) {
                continue;
            }
            conn.execute(
                "INSERT OR REPLACE INTO session_prompt
                     (session_id, system_prompt, tools_json, format, node, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                rusqlite::params![
                    p.session_id,
                    p.system_prompt,
                    p.tools_json,
                    p.format,
                    p.node
                ],
            )?;
        }

        // The move, told to the model AFTER the cached prefix: the system
        // prompt still describes where the session started (on purpose —
        // rewriting it would throw the cache away), so one injected row
        // says where it runs now. A session with no history yet has no
        // prefix to keep and gets none.
        for s in &bundle.sessions {
            let Some(here) = note.as_deref() else { break };
            if already.contains(&s.id) || !local_of.contains_key(s.id.as_str()) {
                continue;
            }
            conn.execute(
                "INSERT INTO messages (session_id, role, content, injection)
                 VALUES (?1, 'user', ?2, 'migration')",
                rusqlite::params![s.id, here],
            )?;
        }

        in_tx(conn)?;
        report.already_live = already.into_iter().collect();
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            let root = crate::checkpoints::blob_root();
            for p in stale_blobs {
                if p.starts_with(&root) && !written_blobs.contains(&p) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(report)
}

/// The injected row a migrated session's model reads first on its new
/// node: where it came from, where it is, and what that means for its
/// tools and paths.
fn moved_note(from_name: &str, from_node: &str, me_node: &str) -> String {
    let me_name = crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.name)
        .unwrap_or_else(|| me_node.to_string());
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into());
    format!(
        "[moved] This session moved from node {from_name} ({from_node}) to node {me_name} \
         ({me_node}) at {at}. It runs on {me_name} now: tools act on this machine, and the \
         working directory is {cwd}. The environment described in the system prompt above is \
         {from_name}'s, where the session started — this note is current.",
        at = crate::a2a::now_iso(),
    )
}

// ── the source side: park, retire, un-park ─────────────────────────

/// Phase 1 (prepare): stamp the whole subtree `migrating_to:<target>`.
/// From this instant every turn-writer refuses it (the fence in
/// run_turn) and inbound text parks in the queue. Refuses to park a
/// subtree that is already parked or retired — a second migration of
/// one session is a bug in the operator's head, not something to
/// paper over.
pub fn park(store: &Store, subtree: &[SessionRow], target: &str) -> Result<()> {
    for s in subtree {
        if is_parked(s.node.as_deref()) || is_retired(s.node.as_deref()) {
            anyhow::bail!(
                "session {} is already {} — finish or reclaim that migration first",
                s.id,
                s.node.as_deref().unwrap_or("?")
            );
        }
    }
    // The quiesce check and the stamp are ONE write transaction, and a
    // turn is admitted and registered in one too (Store::
    // begin_turn_fenced) — so across processes either this sees the
    // turn and refuses, or the turn sees the stamp. Checked apart, a
    // turn admitted just before the park kept writing after the bundle
    // was built, and its rows stayed behind at the source.
    let conn = store.conn();
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> Result<()> {
        let busy = is_busy(store, subtree, 60);
        if !busy.is_empty() {
            anyhow::bail!(
                "not at a turn boundary — {}\n(the session parks nothing; nothing was changed)",
                busy.join("; ")
            );
        }
        for s in subtree {
            conn.execute(
                "UPDATE sessions SET node = ?1 WHERE id = ?2",
                rusqlite::params![format!("{PARK_PREFIX}{target}"), s.id],
            )?;
        }
        Ok(())
    })();
    match r {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(())
}

/// Phase 4 (retire): the target adopted, so the source tombstones.
/// Rows STAY (question 3 — they are the B5 replica's content); the
/// node stamp is the record of where the session went and when.
pub fn retire(store: &Store, subtree: &[SessionRow], target: &str) -> Result<u64> {
    retire_on(store.conn(), subtree, target)
}

/// [`retire`] on a connection — inside a caller's transaction.
pub fn retire_on(conn: &rusqlite::Connection, subtree: &[SessionRow], target: &str) -> Result<u64> {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format!("{RETIRED_PREFIX}{target}@{epoch}");
    let mut n = 0;
    for s in subtree {
        n += conn.execute(
            "UPDATE sessions SET node = ?1 WHERE id = ?2",
            rusqlite::params![stamp, s.id],
        )? as u64;
        // The source's live-turn registry rows are cleared — the
        // adopter writes its own on its first turn (plan: quiesce-only).
        let _ = conn.execute(
            "DELETE FROM session_runs WHERE session_id = ?1",
            rusqlite::params![s.id],
        );
    }
    Ok(n)
}

/// Un-park: the push failed AND the target confirmed it never adopted
/// (or was unreachable AND confirmed not-adopted on a later query).
/// The stamp goes back to this node — the session is exactly as
/// runnable as before. NEVER called without the query: an un-park
/// against an adopted target is the double-writer the whole protocol
/// exists to prevent.
pub fn unpark(store: &Store, subtree: &[SessionRow], me_node: &str) -> Result<u64> {
    let mut n = 0;
    for s in subtree {
        n += store.conn().execute(
            "UPDATE sessions SET node = ?1 WHERE id = ?2 AND node LIKE ?3",
            rusqlite::params![me_node, s.id, format!("{PARK_PREFIX}%")],
        )? as u64;
    }
    Ok(n)
}

/// Is this session live (turn-running or queue-running) right now?
/// The quiesce gate: migration waits for the turn boundary, and this
/// is how the command says "not now" honestly.
pub fn is_busy(store: &Store, subtree: &[SessionRow], max_stale_secs: i64) -> Vec<String> {
    let mut busy = Vec::new();
    for s in subtree {
        let live: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_runs
                 WHERE session_id = ?1
                   AND (heartbeat_at IS NULL
                        OR (strftime('%s','now') - strftime('%s', heartbeat_at)) < ?2)",
                rusqlite::params![s.id, max_stale_secs],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if live > 0 {
            busy.push(format!("{}: a turn is live (session_runs heartbeat)", s.id));
            continue;
        }
        let running: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_queue WHERE session_id = ?1 AND state = 'running'",
                rusqlite::params![s.id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if running > 0 {
            busy.push(format!("{}: {running} queue row(s) mid-drain", s.id));
        }
    }
    busy
}

// ── transport: marker, fence, push, query ───────────────────────────

/// The first line of a migration message.
pub const MIGRATE_MARKER: &str = "[veles session migration]";
/// The first line of an adopted-state query (the split-brain check:
/// the source asks the target "did you adopt session X?" before it may
/// un-park; the target answers from its own store, never from memory).
pub const MIGRATE_QUERY_MARKER: &str = "[veles migration query]";
const FENCE_B64: &str = "```veles-migration-b64";

pub fn is_migration_message(text: &str) -> bool {
    text.trim_start().starts_with(MIGRATE_MARKER) && text.contains(FENCE_B64)
}

pub fn is_migration_query(text: &str) -> bool {
    text.trim_start().starts_with(MIGRATE_QUERY_MARKER)
}

/// Encode one bundle as a message: marker, summary, base64 + sha
/// fence (the cluster framing every protocol shares).
pub fn encode_bundle(bundle: &MigrationBundle) -> Result<String> {
    use base64::Engine as _;
    let json = serde_json::to_string(bundle)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    Ok(format!(
        "{MIGRATE_MARKER} {} session(s), {} message(s) from {} to {}\n\
         {FENCE_B64}\nsha256:{sha}\n{b64}\n```",
        bundle.sessions.len(),
        bundle.messages.len(),
        bundle.from_name,
        bundle.to_node,
    ))
}

/// The bundle inside a message (sha-verified).
pub fn decode_bundle(raw: &str) -> Result<MigrationBundle> {
    use base64::Engine as _;
    let start = raw
        .find(FENCE_B64)
        .context("migration: no payload fence found")?;
    let body = &raw[start + FENCE_B64.len()..];
    let body = &body[..body.find("```").unwrap_or(body.len())];
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let sha = lines
        .next()
        .and_then(|l| l.strip_prefix("sha256:"))
        .context("migration: missing sha256 line")?
        .to_string();
    let b64: String = lines.collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .context("migration: payload is not base64")?;
    let json = String::from_utf8(bytes).context("migration: payload is not UTF-8")?;
    if crate::memory::MemoryStore::content_sha(&json) != sha {
        anyhow::bail!("migration: checksum mismatch — the bundle changed in transit");
    }
    serde_json::from_str(&json).context("migration: payload is not a bundle")
}

/// Encode the split-brain query: "did you adopt session X?"
pub fn encode_query(session_id: &str, from_node: &str) -> String {
    format!("{MIGRATE_QUERY_MARKER} {session_id} from {from_node}")
}

/// Parse a query. (session_id, from_node)
pub fn decode_query(raw: &str) -> Result<(String, String)> {
    // The marker itself contains spaces, so it is stripped as a
    // PREFIX — a whitespace-split would read "migration" as the id.
    let rest = raw
        .trim()
        .strip_prefix(MIGRATE_QUERY_MARKER)
        .context("query: wrong marker")?;
    let mut parts = rest.split_whitespace();
    let sid = parts.next().context("query: no session id")?.to_string();
    let _from = parts.next().context("query: missing 'from'")?;
    let node = parts.next().context("query: no source node")?.to_string();
    Ok((sid, node))
}

/// The answer to a query, as one line: `adopted <session> <node>@<stamp>`
/// or `not-adopted <session>`. Deliberately tiny — the source parses
/// it, and a prose answer is a parse waiting to break.
pub fn encode_query_answer(store: &Store, session_id: &str) -> Result<String> {
    let row = store
        .conn()
        .query_row(
            "SELECT node FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    match row {
        // Live here = ours (or a tombstone: adopted-and-retired cannot
        // happen on the TARGET, but a migrated_to stamp means this
        // node was once a source for it — still "not adopted here").
        Some(node) if !is_retired(node.as_deref()) => Ok(format!(
            "adopted {session_id} {}",
            node.unwrap_or_else(|| "this-node".into())
        )),
        _ => Ok(format!("not-adopted {session_id}")),
    }
}

use rusqlite::OptionalExtension as _;

/// Parse a query answer: true = the target holds it live.
pub fn decode_query_answer(answer: &str) -> bool {
    answer.trim_start().starts_with("adopted ")
}

/// Where a target node is dialable from: its `[a2a_agents]` entry via
/// the B4 placement resolver. Split out of [`migrate`] so the sync
/// resolution (a `Store` is NOT `Send`) never crosses an await.
fn resolve_target(
    me: &crate::node::NodeIdentity,
    target: &str,
) -> Result<(crate::placement::Placement, String, Option<String>)> {
    let stale = crate::config::load()
        .map(|c| c.cluster.stale_after_secs)
        .unwrap_or(600) as i64;
    let home = crate::config::veles_home();
    let registry = Store::open(&home.join("state.db"))
        .context("cannot open the registry to resolve the target")?;
    let placement = {
        let spec = crate::placement::BackendSpec::Node(target.to_string());
        let routable = |n: &crate::store::NodeRow| {
            me.id == n.id || crate::delegate::remote_target_for(n).is_some()
        };
        crate::placement::resolve(&registry, &spec, Some(&me.id), stale, &routable)?
    };
    if crate::placement::is_local(&placement, Some(&me.id)) {
        anyhow::bail!("the session already lives on this node ({target}) — nothing to migrate");
    }
    let (url, bearer) = crate::delegate::remote_target_for(&placement.node).ok_or_else(|| {
        anyhow::anyhow!(
            "target node '{}' has no [a2a_agents] entry — add one with its url + auth token",
            placement.node.name
        )
    })?;
    Ok((placement, url, bearer))
}

/// Everything the source side decides BEFORE any await: subtree walk
/// (question 1's child refusal), the quiesce check, the park stamp,
/// and the built bundle — all sync against a non-Send `Store`. The
/// async orchestrator runs this, awaits the wire, then runs the sync
/// finishing leg.
/// What `prepare` produced: everything the sync legs decided, shaped
/// for the async orchestrator (which only needs the wire fields plus
/// the bundle). Named because five positional tuple fields at a call
/// site is a bug waiting for one swap.
#[derive(Debug)]
struct Prepared {
    placement: crate::placement::Placement,
    url: String,
    bearer: Option<String>,
    subtree: Vec<SessionRow>,
    bundle: MigrationBundle,
}

fn prepare(
    store: &Store,
    me: &crate::node::NodeIdentity,
    target: &str,
    root_session: &str,
    force_child: bool,
) -> Result<Prepared> {
    let (placement, url, bearer) = resolve_target(me, target)?;

    // Question 1: the subtree, and the child-without-parent refusal.
    let subtree = session_subtree(store, root_session)?;
    if subtree.is_empty() {
        anyhow::bail!("no such session: {root_session}");
    }
    if let Some(parent) = &subtree[0].parent {
        let parent_local: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                rusqlite::params![parent],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if parent_local > 0 && !force_child {
            anyhow::bail!(
                "{root_session} is a CHILD of {parent}, which is not migrating — the link \
                 would cross files. Migrate the parent (the subtree moves as a unit), or \
                 pass --force to orphan this session's parent link honestly."
            );
        }
    }

    // Phase 1: quiesce + park, one transaction — a live turn or a
    // mid-drain queue row is "not now", and nothing changes.
    park(store, &subtree, &placement.node.id)?;

    // Phase 2a: build the bundle.
    let bundle = match build_bundle(store, &me.id, &me.name, &placement.node.id, root_session) {
        Ok(b) => b,
        Err(e) => {
            let _ = unpark(store, &subtree, &me.id);
            return Err(e.context("building the bundle failed — the session was un-parked"));
        }
    };
    Ok(Prepared {
        placement,
        url,
        bearer,
        subtree,
        bundle,
    })
}

/// THE ORCHESTRATOR: the four phases from the source side.
///
/// 1. quiesce-check + park (local, sync — [`prepare`]);
/// 2. push the bundle over A2A (the target's receiver auto-adopts
///    behind `[cluster] migrate_peers` — question 5);
/// 3. on an adopt-confirmed reply: retire locally (tombstone in
///    place, question 3);
/// 4. on a failed/unconfirmed push: QUERY the target (with retry; the
///    CLI bounds the tries, a gateway caller can bound or not) and
///    un-park ONLY on an explicit not-adopted answer. An unreachable
///    target leaves the session PARKED: never lost, never
///    double-written.
///
/// `tokio::task::spawn_blocking` runs the sync legs: a `Store` is not
/// `Send`, and this fn's awaits must not hold one.
pub async fn migrate(
    me: &crate::node::NodeIdentity,
    target: &str,
    root_session: &str,
    force_child: bool,
) -> Result<String> {
    // No `&Store` parameter, deliberately: every leg runs inside
    // spawn_blocking against a store re-opened from veles_home() (a
    // Store is not Send, and holding one across these awaits would
    // make the caller's future !Send — which breaks axum's Handler
    // bound on the desktop command route). The sync halves (prepare,
    // retire, unpark) take the &Store the blocking legs open.
    let me = me.clone();
    let target = target.to_string();
    let root_session = root_session.to_string();
    let (me_b, root_b) = (me.clone(), root_session.clone());
    let Prepared {
        placement,
        url,
        bearer,
        subtree,
        bundle,
    } = crate::rt::spawn_blocking(move || {
        let home = crate::config::veles_home();
        let store = Store::open(&home.join("state.db"))?;
        prepare(&store, &me_b, &target, &root_b, force_child)
    })
    .await??;

    // Question 2: the cost warning rides the success output, and the
    // pre-push note says what does NOT travel (the plan's honesty
    // list), so the operator sees both before the reply lands.
    let ctx_chars = bundle.context_chars();
    let n_sessions = bundle.sessions.len();
    let n_msgs = bundle.messages.len();
    let n_cps = bundle.checkpoints.len();
    let text = encode_bundle(&bundle)?;

    let client = crate::a2a::A2AClient::with_timeout(std::time::Duration::from_secs(300));
    let ctx_id = crate::a2a::new_context_id();
    let push = client.call(&url, bearer.as_deref(), &text, &ctx_id).await;

    let adopted_reply = match &push {
        Ok(result) => {
            let reply = crate::a2a::extract_text(result);
            let state = result["status"]["state"].as_str().unwrap_or("?");
            if state == crate::a2a::STATE_COMPLETED && reply.starts_with("adopted ") {
                Some(reply.clone())
            } else {
                // A refusal names itself; carry it into the query path.
                tracing::warn!("migration push answered {state}: {reply}");
                None
            }
        }
        Err(e) => {
            tracing::warn!("migration push to {} failed: {e:#}", placement.node.name);
            None
        }
    };

    if let Some(reply) = adopted_reply {
        // Phase 4a: retire (tombstone in place) — sync leg, blocking.
        let target_id = placement.node.id.clone();
        let n = crate::rt::spawn_blocking(move || {
            let home = crate::config::veles_home();
            let store = Store::open(&home.join("state.db"))?;
            retire(&store, &subtree, &target_id)
        })
        .await??;
        let extra = n_sessions.saturating_sub(1);
        return Ok(format!(
            "migrated {root_session} (+{extra} subtree session(s), {n_msgs} messages, \
             {n_cps} checkpoints) to {} ({}): {reply}\n\
             {n} session(s) tombstoned here; their rows stay — B5 replicates them.\n\
             cost note: the prompt and the tool list travel byte for byte, so the first \
             turn there can hit the provider's cache (~{ctx_chars} chars, ~{} tokens) when \
             both nodes use the same provider account within its cache lifetime; otherwise \
             it re-pays that context once.\n\
             not travelled: background shells/subagents/monitors of this session died at \
             the source (their files persist; completions never arrive on the target); \
             the usage ledger stays here.",
            placement.node.name,
            placement.node.id,
            ctx_chars / crate::compaction::CHARS_PER_TOKEN,
        ));
    }

    // Phase 4b: the push did not confirm. QUERY the target — with
    // retry and NO time bound (the plan's wording: the session simply
    // stays parked, never lost). The query loop backs off and gives
    // the CLI caller a bounded number of tries (an unbounded await
    // would hang a terminal forever); each try is cheap and the
    // parked state is safe, so giving up leaves the session parked
    // and the operator with the reclaim command named.
    // The finishing legs clone what they need and re-open the store
    // inside spawn_blocking (a Store is not Send).
    let me_id = me.id.clone();
    let root_session = root_session.clone();
    let subtree_q = subtree.clone();
    for attempt in 1..=5u32 {
        let q = encode_query(&root_session, &me_id);
        match client.call(&url, bearer.as_deref(), &q, &ctx_id).await {
            Ok(result) => {
                let answer = crate::a2a::extract_text(&result);
                if decode_query_answer(&answer) {
                    // The push DID land (its reply was lost). Retire.
                    let target_id = placement.node.id.clone();
                    let sub = subtree_q.clone();
                    let n = crate::rt::spawn_blocking(move || {
                        let home = crate::config::veles_home();
                        let store = Store::open(&home.join("state.db"))?;
                        retire(&store, &sub, &target_id)
                    })
                    .await??;
                    return Ok(format!(
                        "the push reply was lost but the target confirms: {answer}\n\
                         {n} session(s) tombstoned here; migration complete."
                    ));
                }
                if answer.starts_with("not-adopted ") {
                    let me2 = me_id.clone();
                    let sub = subtree_q.clone();
                    let n = crate::rt::spawn_blocking(move || {
                        let home = crate::config::veles_home();
                        let store = Store::open(&home.join("state.db"))?;
                        unpark(&store, &sub, &me2)
                    })
                    .await??;
                    let reason = match &push {
                        Ok(result) => crate::a2a::extract_text(result),
                        Err(e) => format!("{e:#}"),
                    };
                    return Err(anyhow::anyhow!(
                        "the target did not adopt ({reason}) — {n} session(s) un-parked, \
                         nothing changed"
                    ));
                }
                tracing::warn!("migration query answered oddly: {answer}");
            }
            Err(e) => {
                tracing::warn!("migration query attempt {attempt} failed: {e:#}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt.min(4)))).await;
    }
    Err(anyhow::anyhow!(
        "the target never confirmed adoption and never denied it — {root_session} stays \
         PARKED (migrating_to:{}). Nothing is lost and no writer can double: when the \
         target is reachable again, run `veles session reclaim {root_session}` \
         (queries first, then un-parks only on a confirmed not-adopted).",
        placement.node.id
    ))
}

/// The reclaim path: query the target first; un-park ONLY on a
/// confirmed not-adopted. If the target says adopted, finish the
/// migration instead (retire) — a reclaim is never a double-writer.
pub async fn reclaim(me: &crate::node::NodeIdentity, root_session: &str) -> Result<String> {
    // See migrate(): no &Store across awaits.
    let me = me.clone();
    let root_session = root_session.to_string();
    let (me_leg1, root_leg1) = (me.clone(), root_session.clone());
    // Sync leg 1: what is parked, and where the target dials.
    let (placement, url, bearer, subtree, me_id) = crate::rt::spawn_blocking(move || {
        let home = crate::config::veles_home();
        let store = Store::open(&home.join("state.db"))?;
        let subtree = session_subtree(&store, &root_leg1)?;
        if subtree.is_empty() {
            anyhow::bail!("no such session: {root_leg1}");
        }
        let Some(node) = subtree[0].node.as_deref() else {
            anyhow::bail!("{root_leg1} is not parked (node stamp is empty) — nothing to reclaim");
        };
        let Some(target) = park_target(Some(node)) else {
            if is_retired(Some(node)) {
                anyhow::bail!(
                    "{root_leg1} is already retired here ({node}) — it lives on the \
                     target; reclaim means migrating BACK, which is a new migration from \
                     that node"
                );
            }
            anyhow::bail!("{root_leg1} is owned by {node}, not parked mid-migration");
        };
        let (placement, url, bearer) =
            resolve_target(&me_leg1, &target).context("resolving the parked target")?;
        Ok::<_, anyhow::Error>((placement, url, bearer, subtree, me_leg1.id.clone()))
    })
    .await??;

    // Await leg: the query.
    let client = crate::a2a::A2AClient::new();
    let ctx_id = crate::a2a::new_context_id();
    let q = encode_query(&root_session, &me_id);
    let result = client
        .call(&url, bearer.as_deref(), &q, &ctx_id)
        .await
        .with_context(|| format!("querying the target {}", placement.node.name))?;
    let answer = crate::a2a::extract_text(&result);

    // Sync leg 2: the answer decides — retire (it adopted) or un-park
    // (it confirmed not-adopted). Never both; that IS the fence.
    let adopted = decode_query_answer(&answer);
    let target_id = placement.node.id.clone();
    let n = crate::rt::spawn_blocking(move || {
        let home = crate::config::veles_home();
        let store = Store::open(&home.join("state.db"))?;
        if adopted {
            retire(&store, &subtree, &target_id)
        } else {
            unpark(&store, &subtree, &me_id)
        }
    })
    .await??;
    if adopted {
        return Ok(format!(
            "the target confirms it adopted ({answer}) — finishing the migration instead: \
             {n} session(s) tombstoned here"
        ));
    }
    if !answer.starts_with("not-adopted ") {
        anyhow::bail!("the target answered neither adopted nor not-adopted: {answer}");
    }
    Ok(format!(
        "{n} session(s) reclaimed — the target confirmed not-adopted; this node owns \
         {root_session} again"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in its own tempdir, with THIS thread's directories pointed
    /// at that dir for as long as the returned guard lives (the B5 log is
    /// NOT installed: migration is its own transport, not store_log
    /// rows). The scratch_home env guard is not held — the two-store
    /// tests would deadlock on its lock — but `veles_home()` must never
    /// answer the real home: the blob tests read `checkpoints::blob_root()`,
    /// and when this helper dropped its guard they wrote `mig-1/` and
    /// `d-1/` into the LIVE ~/.local/share/veles/checkpoints (found in the
    /// 2026-09-29 review). `prep_env` is the sibling that holds the full
    /// env guard, because prepare() re-opens veles_home()/state.db.
    fn scratch_store(tag: &str) -> (tempfile::TempDir, Store, crate::config::DirGuard) {
        let dir = tempfile::tempdir().expect("dir");
        let guard = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
            dir.path().to_path_buf(),
        )));
        let store = Store::open(&dir.path().join(format!("{tag}.db"))).expect("open");
        (dir, store, guard)
    }

    fn seed_session(store: &Store, id: &str, parent: Option<&str>, node: &str) {
        match parent {
            None => store.create_session(id).unwrap(),
            Some(p) => store.create_child_session(id, p, Some("child")).unwrap(),
        }
        store
            .conn()
            .execute(
                "UPDATE sessions SET node = ?1 WHERE id = ?2",
                rusqlite::params![node, id],
            )
            .unwrap();
    }

    // ── the subtree (question 1) ──────────────────────────────────

    #[test]
    fn the_subtree_walk_takes_every_descendant_parents_first() {
        let (_d, store, _g) = scratch_store("subtree");
        seed_session(&store, "root", None, "node-a");
        seed_session(&store, "kid", Some("root"), "node-a");
        seed_session(&store, "grandkid", Some("kid"), "node-a");
        seed_session(&store, "unrelated", None, "node-a");
        let sub = session_subtree(&store, "root").unwrap();
        let ids: Vec<&str> = sub.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "kid", "grandkid"], "{ids:?}");
        assert!(
            sub.iter()
                .all(|s| s.depth as usize == ids.iter().position(|i| *i == s.id).unwrap()),
            "depths ascend"
        );
    }

    #[test]
    fn a_parent_cycle_terminates() {
        let (_d, store, _g) = scratch_store("cycle");
        seed_session(&store, "x", None, "node-a");
        seed_session(&store, "y", Some("x"), "node-a");
        // Forge the cycle the store does not promise is impossible.
        store
            .conn()
            .execute("UPDATE sessions SET parent = 'y' WHERE id = 'x'", [])
            .unwrap();
        let sub = session_subtree(&store, "x").unwrap();
        assert_eq!(sub.len(), 2, "each session once: {sub:?}");
    }

    // ── park / retire / unpark stamps ─────────────────────────────

    #[test]
    fn park_retire_unpark_stamps_are_exclusive_and_named() {
        let (_d, store, _g) = scratch_store("stamps");
        seed_session(&store, "s1", None, "node-a");
        seed_session(&store, "s1-kid", Some("s1"), "node-a");
        let sub = session_subtree(&store, "s1").unwrap();

        park(&store, &sub, "node-b").unwrap();
        assert!(is_parked(
            store.session_node_stamp("s1").unwrap().as_deref()
        ));
        assert_eq!(
            park_target(store.session_node_stamp("s1-kid").unwrap().as_deref()).as_deref(),
            Some("node-b"),
            "the whole subtree parks, and names the target"
        );
        // A second park of a parked subtree refuses by name.
        let sub2 = session_subtree(&store, "s1").unwrap();
        let e = park(&store, &sub2, "node-b").unwrap_err().to_string();
        assert!(e.contains("already"), "{e}");

        retire(&store, &sub, "node-b").unwrap();
        let stamp = store.session_node_stamp("s1").unwrap().unwrap();
        assert!(is_retired(Some(&stamp)), "{stamp}");
        assert!(
            stamp.contains("node-b@"),
            "the tombstone names target@epoch: {stamp}"
        );
        assert!(!is_parked(Some(&stamp)));

        // Un-park only touches PARKED rows — a retired subtree is not
        // resurrected by a reclaim that raced a retirement.
        let n = unpark(&store, &sub, "node-a").unwrap();
        assert_eq!(n, 0, "a retired row is not un-parked");
    }

    #[test]
    fn unpark_restores_ownership_of_a_parked_subtree_only() {
        let (_d, store, _g) = scratch_store("unpark");
        seed_session(&store, "s2", None, "node-a");
        seed_session(&store, "other", None, "node-a");
        let sub = session_subtree(&store, "s2").unwrap();
        park(&store, &sub, "node-b").unwrap();
        // A foreign session's stamp must not move.
        store
            .conn()
            .execute("UPDATE sessions SET node = 'node-z' WHERE id = 'other'", [])
            .unwrap();
        let n = unpark(&store, &sub, "node-a").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            store.session_node_stamp("s2").unwrap().as_deref(),
            Some("node-a")
        );
        assert_eq!(
            store.session_node_stamp("other").unwrap().as_deref(),
            Some("node-z"),
            "unpark is scoped to the subtree's PARKED rows"
        );
    }

    // ── the bundle: build, encode, decode, tamper ─────────────────

    #[test]
    fn the_bundle_carries_every_travelling_table_and_blobs() {
        let (_d, store, _g) = scratch_store("bundle");
        seed_session(&store, "mig-1", None, "node-a");
        seed_session(&store, "mig-1-kid", Some("mig-1"), "node-a");
        store.queue_push("mig-1", "parked text").unwrap();
        // A row that parked behind a platform message, so it carries the
        // target its answer belongs under. It has to survive the move:
        // the drain runs on the NEW node, and answering there with no
        // target is what put replies under the wrong post.
        store
            .queue_push_routed(
                "mig-1",
                "routed text",
                "message",
                Some("at://did:plc:op/app.bsky.feed.post/asked"),
                Some("IRC4Fun"),
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO session_overrides (session_id, provider, model, reasoning)
                 VALUES ('mig-1', 'p', 'm', 'high')",
                [],
            )
            .unwrap();
        store.session_locale_set("mig-1", "de").unwrap();
        // A checkpoint with a REAL blob file.
        let cp_dir = crate::checkpoints::blob_root().join("mig-1");
        std::fs::create_dir_all(&cp_dir).unwrap();
        let blob = cp_dir.join("111-file.txt");
        std::fs::write(&blob, b"checkpoint bytes").unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO checkpoints (session_id, path, blob, sha256, bytes, tool, note)
                 VALUES ('mig-1', '/some/file.txt', ?1, 'abc', 16, 'patch', 'n')",
                rusqlite::params![blob.display().to_string()],
            )
            .unwrap();
        // Messages (the FTS trigger fires on insert through the store
        // API; raw inserts here are the test's own and skip it — the
        // bundle reads rows, not FTS).
        for i in 0..3 {
            store
                .conn()
                .execute(
                    "INSERT INTO messages (session_id, role, content) VALUES ('mig-1', 'user', ?1)",
                    rusqlite::params![format!("message {i}")],
                )
                .unwrap();
        }

        let b = build_bundle(&store, "node-a", "laptop", "node-b", "mig-1").unwrap();
        assert_eq!(b.sessions.len(), 2, "the subtree travels");
        assert_eq!(b.messages.len(), 3);
        assert_eq!(b.queue.len(), 2);
        assert_eq!(b.queue[0].body, "parked text");
        assert_eq!(
            b.queue[0].reply_to, None,
            "a plain row parked behind nothing carries no target"
        );
        assert_eq!(b.queue[1].body, "routed text");
        assert_eq!(
            b.queue[1].reply_to.as_deref(),
            Some("at://did:plc:op/app.bsky.feed.post/asked"),
            "the row's threading target travels with it, so the new node's \
             drain answers under the message that asked"
        );
        assert_eq!(
            b.queue[1].route.as_deref(),
            Some("IRC4Fun"),
            "and the adapter instance that carried it"
        );
        assert_eq!(b.overrides.len(), 1);
        assert_eq!(b.locale, vec![("mig-1".to_string(), "de".to_string())]);
        assert_eq!(b.checkpoints.len(), 1);
        assert!(
            b.checkpoints[0].blob_b64.is_some(),
            "the blob BYTES ride inside the bundle (question 4)"
        );
        assert!(b.checkpoints[0].blob_key.starts_with("mig-1/"));

        // Wire round trip + tamper.
        let text = encode_bundle(&b).unwrap();
        assert!(is_migration_message(&text));
        assert!(!is_migration_query(&text));
        let back = decode_bundle(&text).unwrap();
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.messages.len(), 3);
        assert_eq!(back.to_node, "node-b");
        let tampered = text.replace("sha256:", "sha256:00");
        assert!(decode_bundle(&tampered).is_err(), "a flipped sha refuses");
    }

    #[test]
    fn an_unreadable_blob_is_named_not_dropped() {
        let (_d, store, _g) = scratch_store("blobgone");
        seed_session(&store, "mig-2", None, "node-a");
        store
            .conn()
            .execute(
                "INSERT INTO checkpoints (session_id, path, blob, sha256, bytes, tool, note)
                 VALUES ('mig-2', '/f.txt', '/nonexistent/blob', 'abc', 3, 'patch', 'n')",
                [],
            )
            .unwrap();
        let b = build_bundle(&store, "node-a", "laptop", "node-b", "mig-2").unwrap();
        assert_eq!(b.checkpoints.len(), 1, "the ROW still travels");
        assert!(b.checkpoints[0].blob_b64.is_none(), "the bytes could not");
    }

    // ── adopt ─────────────────────────────────────────────────────

    #[test]
    fn adopt_lands_everything_stamps_the_node_and_maps_msg_ids() {
        let (_d1, src, _g1) = scratch_store("adopt-src");
        let (_d2, dst, _g2) = scratch_store("adopt-dst");
        seed_session(&src, "a-1", None, "node-a");
        seed_session(&src, "a-1-kid", Some("a-1"), "node-a");
        for i in 0..4 {
            src.conn()
                .execute(
                    "INSERT INTO messages (session_id, role, content) VALUES ('a-1', 'user', ?1)",
                    rusqlite::params![format!("m{i}")],
                )
                .unwrap();
        }
        src.queue_push("a-1", "queued").unwrap();
        src.queue_push_routed(
            "a-1",
            "queued behind a post",
            "message",
            Some("at://did:plc:op/app.bsky.feed.post/asked"),
            Some("IRC4Fun"),
        )
        .unwrap();
        src.session_locale_set("a-1", "fr").unwrap();

        let bundle = build_bundle(&src, "node-a", "laptop", "node-b", "a-1").unwrap();
        let rep = adopt(&dst, "node-b", &bundle).unwrap();
        assert_eq!(rep.sessions, 2);
        assert_eq!(rep.messages, 4);
        assert_eq!(rep.queue, 2);

        // The threading target LANDED on the destination: the drain runs
        // there, so a row that arrived without it would answer under
        // whatever message started that node's first turn — the bug this
        // fixes (a Bluesky answer posted two messages up-thread from the
        // question it answered).
        let rows = dst.queue_items("a-1").unwrap();
        assert_eq!(rows.len(), 2, "both parked rows landed: {rows:?}");
        let plain = rows.iter().find(|r| r.body == "queued").expect("row");
        assert_eq!(
            plain.reply_to, None,
            "a row that parked behind nothing still carries nothing"
        );
        let routed = rows
            .iter()
            .find(|r| r.body == "queued behind a post")
            .expect("routed row");
        assert_eq!(
            routed.reply_to.as_deref(),
            Some("at://did:plc:op/app.bsky.feed.post/asked"),
            "the routed row kept its target across the move"
        );
        assert_eq!(routed.route.as_deref(), Some("IRC4Fun"));
        assert_eq!(
            routed.state, "held",
            "and it still arrives HELD — a node must not plant turns on another"
        );

        // Stamped with the adopter.
        assert_eq!(
            dst.session_node_stamp("a-1").unwrap().as_deref(),
            Some("node-b")
        );
        assert_eq!(
            dst.session_node_stamp("a-1-kid").unwrap().as_deref(),
            Some("node-b")
        );
        // The parent link survived (both landed).
        assert_eq!(
            dst.session_parent("a-1-kid").unwrap().as_deref(),
            Some("a-1")
        );
        // The msg map: every source id maps to a live local id.
        let mapped: i64 = dst
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM migration_msg_map WHERE session_id = 'a-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mapped, 4);
        let live: i64 = dst
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages m JOIN migration_msg_map x
                 ON m.msg_id = x.local_msg_id AND m.session_id = x.session_id
                 WHERE x.source_node = 'node-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, 4, "every mapped id is a real row");
        // The queue rows arrive HELD (D2): nothing drains them until
        // someone here releases them — a node must not plant turns here.
        let q = dst.queue_items("a-1").unwrap();
        assert_eq!(q.len(), 2);
        assert!(
            q.iter().all(|r| r.state == "held"),
            "both rows arrive held: {q:?}"
        );
        assert!(dst.queue_take_next("a-1").unwrap().is_none());
        assert_eq!(dst.queue_run_held("a-1", None).unwrap(), 2);
        // One row at a time, releasing it as the drainer does. `release`
        // DELETES the row (its turn finished, so nothing audits it); a
        // `running` row still standing is read as a crashed drainer by the
        // next take and demoted back to `pending` — so taking twice
        // without a release re-reads seq 1 rather than advancing. That is
        // the production contract, not a quirk of this test.
        let first = dst.queue_take_next("a-1").unwrap().expect("a row");
        assert_eq!(
            first.body, "queued",
            "they drain in the order they parked, by seq"
        );
        assert!(dst.queue_release("a-1", first.seq).unwrap());
        // The second row is the routed one, and its target survived the
        // build-adopt-release-drain trip (the wire round trip is
        // `the_bundle_carries_every_travelling_table_and_blobs`). This is
        // the row a drain on THIS node answers, so losing the target here
        // is losing it for good.
        let second = dst.queue_take_next("a-1").unwrap().expect("the routed row");
        assert_eq!(second.body, "queued behind a post");
        assert_eq!(
            second.reply_to.as_deref(),
            Some("at://did:plc:op/app.bsky.feed.post/asked"),
            "the threading target survived the move and the release"
        );
        assert_eq!(second.route.as_deref(), Some("IRC4Fun"));
        assert_eq!(
            dst.session_locale_get("a-1").unwrap().as_deref(),
            Some("fr")
        );
    }

    #[test]
    fn adopt_is_idempotent_on_a_re_push() {
        let (_d1, src, _g1) = scratch_store("idem-src");
        let (_d2, dst, _g2) = scratch_store("idem-dst");
        seed_session(&src, "b-1", None, "node-a");
        src.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content) VALUES ('b-1', 'user', 'once')",
                [],
            )
            .unwrap();
        let bundle = build_bundle(&src, "node-a", "laptop", "node-b", "b-1").unwrap();
        adopt(&dst, "node-b", &bundle).unwrap();
        // The lost-reply re-push: same bundle again.
        let rep = adopt(&dst, "node-b", &bundle).unwrap();
        assert_eq!(rep.already_live, vec!["b-1".to_string()]);
        assert_eq!(rep.messages, 0, "history is NOT doubled");
        let n: i64 = dst
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id='b-1'
                 AND COALESCE(injection, '') != 'migration'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let moved: i64 = dst
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id='b-1' AND injection = 'migration'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(moved, 1, "a re-push adds no second [moved] row");
        // A local rename since the first adopt survives the re-push —
        // the already-live session is untouched, not REPLACEd with the
        // bundle's stale metadata.
        dst.conn()
            .execute(
                "UPDATE sessions SET title = 'renamed here' WHERE id = 'b-1'",
                [],
            )
            .unwrap();
        adopt(&dst, "node-b", &bundle).unwrap();
        let title: String = dst
            .conn()
            .query_row("SELECT title FROM sessions WHERE id = 'b-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            title, "renamed here",
            "the re-push must not clobber local metadata"
        );
    }

    /// Every child row names its own session, and adopt used to take
    /// that at its word: a row for a session the bundle does not carry
    /// landed in whatever LOCAL session had that id — history injected
    /// into a live conversation, and a queued row that runs as a turn.
    #[test]
    fn adopt_refuses_rows_for_sessions_the_bundle_does_not_carry() {
        let (_d1, src, _g1) = scratch_store("m1-src");
        let (_d2, dst, _g2) = scratch_store("m1-dst");
        seed_session(&dst, "victim", None, "node-b");
        seed_session(&src, "m1-1", None, "node-a");
        let mut bundle = build_bundle(&src, "node-a", "laptop", "node-b", "m1-1").unwrap();
        bundle.messages.push(MessageRow {
            source_msg_id: 1,
            session_id: "victim".into(),
            role: "user".into(),
            content: "planted".into(),
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            injection: None,
            created_at: String::new(),
            archived: false,
        });
        bundle.queue.push(QueueRow {
            session_id: "victim".into(),
            seq: 1,
            body: "run me".into(),
            kind: "message".into(),
            reply_to: None,
            route: None,
        });
        let e = adopt(&dst, "node-b", &bundle).unwrap_err().to_string();
        assert!(e.contains("victim"), "names the foreign session: {e}");
        let planted: i64 = dst
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'victim'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(planted, 0, "nothing landed in the local session");
        assert!(
            dst.queue_items("victim").unwrap().is_empty(),
            "no queued turn"
        );
        assert!(
            !dst.session_exists("m1-1").unwrap(),
            "the whole bundle refused"
        );
    }

    /// `blob_key` comes from the peer and was joined onto the blob root:
    /// an absolute key replaces the root, `..` climbs out of it.
    #[test]
    fn a_blob_key_cannot_leave_the_blob_root() {
        let (_d1, src, _g1) = scratch_store("m2-src");
        let (_d2, dst, _g2) = scratch_store("m2-dst");
        let outside = tempfile::tempdir().unwrap();
        seed_session(&src, "m2-1", None, "node-a");
        let base = build_bundle(&src, "node-a", "laptop", "node-b", "m2-1").unwrap();
        let abs = outside.path().join("abs.txt").display().to_string();
        for key in [
            abs.as_str(),
            "m2-1/../../../climb.txt",
            "m2-1/.hidden",
            "other/x",
        ] {
            let mut b = base.clone();
            b.checkpoints.push(CheckpointRow {
                source_id: 1,
                session_id: "m2-1".into(),
                path: "/f".into(),
                blob_b64: Some("aGk=".into()),
                sha256: None,
                bytes: 2,
                tool: "patch".into(),
                note: None,
                created_at: String::new(),
                blob_key: key.into(),
            });
            assert!(adopt(&dst, "node-b", &b).is_err(), "{key:?} refused");
        }
        assert!(!outside.path().join("abs.txt").exists(), "no write outside");
        assert!(!dst.session_exists("m2-1").unwrap());
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// A→B→A: the session comes home. The source kept its rows as a
    /// tombstone (question 3), and adopt used to insert the returning
    /// bundle ON TOP of them — every message twice. The tombstone's rows
    /// are the older copy of the same history; they are replaced.
    #[test]
    fn a_round_trip_leaves_one_copy() {
        let (_d1, a, _g1) = scratch_store("rt-a");
        let (_d2, b, _g2) = scratch_store("rt-b");
        seed_session(&a, "rt-1", None, "node-a");
        for i in 0..3 {
            a.append_message(
                "rt-1",
                &crate::providers::ChatMessage::user(&format!("m{i}")),
            )
            .unwrap();
        }
        a.queue_push("rt-1", "later").unwrap();
        let there = build_bundle(&a, "node-a", "a", "node-b", "rt-1").unwrap();
        adopt(&b, "node-b", &there).unwrap();
        retire(&a, &session_subtree(&a, "rt-1").unwrap(), "node-b").unwrap();
        // One more turn's worth on B, then home again.
        b.append_message("rt-1", &crate::providers::ChatMessage::user("m3"))
            .unwrap();
        let back = build_bundle(&b, "node-b", "b", "node-a", "rt-1").unwrap();
        let rep = adopt(&a, "node-a", &back).unwrap();
        assert_eq!(
            rep.messages, 5,
            "four messages and B's [moved] row travel back"
        );
        assert_eq!(
            count(
                &a,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'rt-1'
                 AND COALESCE(injection, '') != 'migration'"
            ),
            4,
            "one copy of each message"
        );
        assert_eq!(
            count(
                &a,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'rt-1'
                 AND injection = 'migration'"
            ),
            2,
            "one [moved] row per move"
        );
        assert_eq!(
            count(
                &a,
                "SELECT COUNT(*) FROM messages_fts WHERE session_id = 'rt-1'"
            ),
            count(
                &a,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'rt-1'"
            ),
            "and one search entry each"
        );
        assert_eq!(
            a.queue_items("rt-1").unwrap().len(),
            1,
            "the queue row once"
        );
        assert_eq!(
            a.session_node_stamp("rt-1").unwrap().as_deref(),
            Some("node-a"),
            "home and writable"
        );
    }

    /// Compacted history is still history: it travels, still archived.
    #[test]
    fn archived_history_travels_with_its_flag() {
        let (_d1, src, _g1) = scratch_store("arch-src");
        let (_d2, dst, _g2) = scratch_store("arch-dst");
        seed_session(&src, "ar-1", None, "node-a");
        for (i, archived) in [(0, 1), (1, 1), (2, 0)] {
            src.conn()
                .execute(
                    "INSERT INTO messages (session_id, role, content, archived)
                     VALUES ('ar-1', 'user', ?1, ?2)",
                    rusqlite::params![format!("m{i}"), archived],
                )
                .unwrap();
        }
        let b = build_bundle(&src, "node-a", "a", "node-b", "ar-1").unwrap();
        adopt(&dst, "node-b", &b).unwrap();
        assert_eq!(
            count(
                &dst,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'ar-1'
                 AND COALESCE(injection, '') != 'migration'"
            ),
            3
        );
        assert_eq!(
            count(
                &dst,
                "SELECT COUNT(*) FROM messages WHERE session_id = 'ar-1' AND archived = 1"
            ),
            2
        );
    }

    /// Engine notes (a guard firing, a compaction) are transcript entries
    /// and keep their place: the anchor is re-pointed at the local id of
    /// the message it followed.
    #[test]
    fn notes_travel_and_keep_their_place() {
        let (_d1, src, _g1) = scratch_store("note-src");
        let (_d2, dst, _g2) = scratch_store("note-dst");
        seed_session(&src, "nt-1", None, "node-a");
        // Make the destination's ids differ from the source's.
        seed_session(&dst, "other", None, "node-b");
        for i in 0..5 {
            dst.append_message(
                "other",
                &crate::providers::ChatMessage::user(&format!("x{i}")),
            )
            .unwrap();
        }
        let first = src
            .append_message("nt-1", &crate::providers::ChatMessage::user("first"))
            .unwrap();
        src.record_note("nt-1", "guard", "the guard fired here", None, None)
            .unwrap();
        src.append_message("nt-1", &crate::providers::ChatMessage::user("second"))
            .unwrap();
        let _ = first;
        let b = build_bundle(&src, "node-a", "a", "node-b", "nt-1").unwrap();
        assert_eq!(b.notes.len(), 1, "the note travels");
        adopt(&dst, "node-b", &b).unwrap();
        let (after, summary): (i64, String) = dst
            .conn()
            .query_row(
                "SELECT after_msg_id, summary FROM engine_notes WHERE session_id = 'nt-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary, "the guard fired here");
        let anchor: String = dst
            .conn()
            .query_row(
                "SELECT content FROM messages WHERE msg_id = ?1",
                [after],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(anchor, "first", "still right after the message it followed");
    }

    /// The fence and the park serialize. A turn is admitted AND
    /// registered in one transaction; park checks the registry and
    /// stamps in one — so a park can never land between a turn's
    /// admission and its registration (it used to: the stamp was read
    /// at the top of the turn and the run registered only after the
    /// user's message was persisted).
    #[test]
    fn a_turn_admitted_by_the_fence_blocks_park_until_it_ends() {
        let (_d, store, _g) = scratch_store("m5");
        seed_session(&store, "f-1", None, "node-a");
        assert_eq!(
            store.begin_turn_fenced("f-1", 1, "h", "tui").unwrap(),
            None,
            "admitted"
        );
        let sub = session_subtree(&store, "f-1").unwrap();
        let e = park(&store, &sub, "node-b").unwrap_err().to_string();
        assert!(e.contains("a turn is live"), "{e}");
        assert!(!is_parked(
            store.session_node_stamp("f-1").unwrap().as_deref()
        ));
        store.run_end("f-1").unwrap();
        park(&store, &sub, "node-b").unwrap();
        let refused = store.begin_turn_fenced("f-1", 2, "h", "tui").unwrap();
        assert!(is_parked(refused.as_deref()), "{refused:?}");
        let runs: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM session_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 0, "a refused turn registers nothing");
    }

    /// An agent on `store`, talking to the mock provider at `url`.
    fn agent_on(
        url: &str,
        store: std::sync::Arc<std::sync::Mutex<Store>>,
        sid: &str,
    ) -> crate::agent::Agent {
        let cfg = std::sync::Arc::new(crate::config::VelesConfig {
            model: crate::config::ModelSection {
                provider: Some("mock".into()),
                default: Some("mock-model".into()),
                ..Default::default()
            },
            custom_providers: vec![crate::config::ProviderEntry {
                name: "mock".into(),
                base_url: url.to_string(),
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let registry =
            std::sync::Arc::new(crate::tools::builtin_registry(store.clone(), None, sid));
        crate::agent::Agent::new(
            cfg,
            crate::hooks::HookBus::new(None),
            registry,
            store,
            sid.to_string(),
        )
        .expect("agent")
    }

    /// The JSON body of one captured HTTP request.
    fn request_json(raw: &str) -> serde_json::Value {
        let body = &raw[raw.find("\r\n\r\n").expect("headers") + 4..];
        serde_json::from_str(body).expect("json body")
    }

    /// USER 2026-09-29: "why can't it preserve cache". A provider's prompt
    /// cache is keyed on the account and a byte-identical prefix — tools,
    /// system prompt, history — not on the machine. The target used to
    /// rebuild the prompt from ITS config, memory and cwd, and offer ITS
    /// tools, so the prefix differed. Now the first request on the target
    /// carries the source's system prompt and tool list byte for byte and
    /// the source's history as its prefix; the move itself is one row
    /// after it.
    #[tokio::test]
    async fn the_first_request_on_the_target_keeps_the_sources_prefix() {
        let (_home, _env) = crate::testenv::scratch_home();
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi back\"}}]}\n\ndata: [DONE]\n\n";
        let (url, captured) =
            crate::providers::tests::sse_server_seq_capture(vec![body.into(), body.into()]).await;
        let sid = "mig-cache";

        // Node A: a turn, then the bundle.
        let a_dir = tempfile::tempdir().unwrap();
        let bundle = {
            let _g = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
                a_dir.path().to_path_buf(),
            )));
            std::fs::write(
                a_dir.path().join("node.toml"),
                "id = \"node-a\"\nname = \"a\"\n",
            )
            .unwrap();
            let store = std::sync::Arc::new(std::sync::Mutex::new(
                Store::open(&a_dir.path().join("state.db")).unwrap(),
            ));
            store.lock().unwrap().create_session(sid).unwrap();
            let agent = agent_on(&url, store.clone(), sid);
            let mut history = Vec::new();
            agent
                .run_turn(&mut history, "hello from A", None, |_| {})
                .await
                .expect("turn on A");
            let st = store.lock().unwrap();
            build_bundle(&st, "node-a", "a", "node-b", sid).unwrap()
        };

        // Node B: its own dirs and a memory entry A never had — a prompt
        // BUILT here would differ from A's.
        let b_dir = tempfile::tempdir().unwrap();
        let _g = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
            b_dir.path().to_path_buf(),
        )));
        std::fs::write(
            b_dir.path().join("node.toml"),
            "id = \"node-b\"\nname = \"b\"\n",
        )
        .unwrap();
        crate::memory::MemoryStore::new()
            .write_file("system/core.md", "B-ONLY-MEMORY: a fact only node B holds")
            .expect("memory on B");
        let store_b = std::sync::Arc::new(std::sync::Mutex::new(
            Store::open(&b_dir.path().join("state.db")).unwrap(),
        ));
        adopt(&store_b.lock().unwrap(), "node-b", &bundle).expect("adopt");
        let agent_b = agent_on(&url, store_b.clone(), sid);
        let mut history = store_b.lock().unwrap().load_history(sid).unwrap();
        agent_b
            .run_turn(&mut history, "hello from B", None, |_| {})
            .await
            .expect("turn on B");

        let reqs = captured.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "one request per node");
        let (ra, rb) = (request_json(&reqs[0]), request_json(&reqs[1]));
        assert_eq!(rb["tools"], ra["tools"], "the tool list, byte for byte");
        let (ma, mb) = (
            ra["messages"].as_array().unwrap(),
            rb["messages"].as_array().unwrap(),
        );
        assert_eq!(mb[0], ma[0], "the system prompt, byte for byte");
        assert!(
            !mb[0].to_string().contains("B-ONLY-MEMORY"),
            "B did not rebuild the prompt from its own memory"
        );
        assert_eq!(&mb[..ma.len()], &ma[..], "A's whole request is B's prefix");
        let rest: Vec<String> = mb[ma.len()..]
            .iter()
            .map(|m| m["content"].to_string())
            .collect();
        assert!(rest[0].contains("hi back"), "then A's reply: {rest:?}");
        assert!(
            rest.iter()
                .any(|c| c.contains("[moved]") && c.contains("node b")),
            "then the move, after the prefix: {rest:?}"
        );
        assert!(rest.last().unwrap().contains("hello from B"), "{rest:?}");
    }

    #[test]
    fn adopt_refuses_a_bundle_addressed_elsewhere() {
        let (_d1, src, _g1) = scratch_store("addr-src");
        let (_d2, dst, _g2) = scratch_store("addr-dst");
        seed_session(&src, "c-1", None, "node-a");
        let bundle = build_bundle(&src, "node-a", "laptop", "node-b", "c-1").unwrap();
        // This node is node-c: the bundle names node-b.
        let e = adopt(&dst, "node-c", &bundle).unwrap_err().to_string();
        assert!(e.contains("addressed to node 'node-b'"), "{e}");
        assert!(
            !dst.session_exists("c-1").unwrap(),
            "a misaddressed bundle lands nothing"
        );
    }

    #[test]
    fn adopt_rewrites_checkpoint_blobs_under_its_own_root() {
        let (_d1, src, _g1) = scratch_store("cp-src");
        seed_session(&src, "d-1", None, "node-a");
        let cp_dir = crate::checkpoints::blob_root().join("d-1");
        std::fs::create_dir_all(&cp_dir).unwrap();
        let blob = cp_dir.join("222-edit.rs");
        std::fs::write(&blob, b"pre-edit bytes").unwrap();
        src.conn()
            .execute(
                "INSERT INTO checkpoints (session_id, path, blob, sha256, bytes, tool, note)
                 VALUES ('d-1', '/proj/edit.rs', ?1, 'deadbeef', 14, 'patch', 'n')",
                rusqlite::params![blob.display().to_string()],
            )
            .unwrap();
        let bundle = build_bundle(&src, "node-a", "laptop", "node-b", "d-1").unwrap();

        // "The target": a DIFFERENT home, so blob_root differs — the
        // adopter must not reuse the source's absolute path.
        let (_home2, _env2) = crate::testenv::scratch_home();
        let dir2 = tempfile::tempdir().unwrap();
        let dst = Store::open(&dir2.path().join("cp-dst.db")).unwrap();
        let rep = adopt(&dst, "node-b", &bundle).unwrap();
        assert_eq!(rep.checkpoints, 1);
        assert!(rep.missing_blobs.is_empty());
        let stored: String = dst
            .conn()
            .query_row(
                "SELECT blob FROM checkpoints WHERE session_id='d-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored.starts_with(crate::checkpoints::blob_root().to_str().unwrap()),
            "the path is rewritten under THIS root: {stored}"
        );
        assert_eq!(std::fs::read(&stored).unwrap(), b"pre-edit bytes");
    }

    #[test]
    fn a_dangling_parent_is_orphaned_honestly_at_adopt() {
        let (_d1, src, _g1) = scratch_store("dang-src");
        let (_d2, dst, _g2) = scratch_store("dang-dst");
        seed_session(&src, "p-keep", None, "node-a");
        seed_session(&src, "p-child", Some("p-keep"), "node-a");
        // Bundle ONLY the child (the --force shape): its parent is not
        // in the bundle and not live on the target.
        let bundle = {
            let mut b = build_bundle(&src, "node-a", "laptop", "node-b", "p-child").unwrap();
            b.sessions.retain(|s| s.id == "p-child");
            b
        };
        adopt(&dst, "node-b", &bundle).unwrap();
        assert_eq!(
            dst.session_parent("p-child").unwrap(),
            None,
            "the cross-file link is orphaned, not left dangling"
        );
    }

    // ── the query protocol ────────────────────────────────────────

    #[test]
    fn the_query_round_trips_and_answers_from_the_store() {
        let (_d, store, _g) = scratch_store("query");
        let q = encode_query("s-9", "node-a");
        assert!(is_migration_query(&q));
        assert!(!is_migration_message(&q));
        let (sid, from) = decode_query(&q).unwrap();
        assert_eq!(sid, "s-9");
        assert_eq!(from, "node-a");

        // Unknown session: not-adopted.
        let a = encode_query_answer(&store, "s-9").unwrap();
        assert_eq!(a, "not-adopted s-9");
        assert!(!decode_query_answer(&a));

        // Live here: adopted.
        seed_session(&store, "s-9", None, "node-b");
        let a = encode_query_answer(&store, "s-9").unwrap();
        assert!(a.starts_with("adopted s-9 "), "{a}");
        assert!(decode_query_answer(&a));

        // A TOMBSTONE is not an adoption — this node was the source.
        store
            .conn()
            .execute(
                "UPDATE sessions SET node = 'migrated_to:node-c@123' WHERE id = 's-9'",
                [],
            )
            .unwrap();
        let a = encode_query_answer(&store, "s-9").unwrap();
        assert_eq!(
            a, "not-adopted s-9",
            "a retired stamp is not this node adopting"
        );
    }

    // ── the fence ─────────────────────────────────────────────────

    #[tokio::test]
    async fn a_parked_session_refuses_every_turn_and_parks_the_text() {
        let (_d, store, _g) = scratch_store("fence");
        seed_session(&store, "f-1", None, "node-a");
        let sub = session_subtree(&store, "f-1").unwrap();
        park(&store, &sub, "node-b").unwrap();

        // The agent-side fence: an agent on the parked session cannot
        // run a turn, and the text lands in the queue.
        let agent = crate::testenv::mock_agent("http://127.0.0.1:0/v1", "f-1");
        {
            // The mock agent has its own store handle; stamp THAT one
            // (create the row first — mock_agent does not).
            let s = agent.store.lock().unwrap();
            s.create_session("f-1").unwrap();
            s.conn()
                .execute(
                    "UPDATE sessions SET node = 'migrating_to:node-b' WHERE id = 'f-1'",
                    [],
                )
                .unwrap();
        }
        let mut history = Vec::new();
        let err = agent
            .run_turn(&mut history, "hello during a migration", None, |_| {})
            .await
            .expect_err("a parked session refuses turns");
        let msg = err.to_string();
        assert!(msg.contains("migrating to node-b"), "{msg}");
        assert!(msg.contains("parked"), "{msg}");
        let parked = {
            let s = agent.store.lock().unwrap();
            s.queue_items("f-1").unwrap()
        };
        assert_eq!(parked.len(), 1, "the text parked instead of vanishing");
        assert_eq!(parked[0].body, "hello during a migration");
        assert!(
            history.is_empty(),
            "a refused turn persists NOTHING — no debris"
        );
    }

    /// The WAKE path is a second writer: a background completion
    /// (subagent, monitor, shell) runs `run_background_wake_turn`
    /// with no user text. It must hit the same fence — a parked
    /// session under a wake turn is the double-writer the whole
    /// protocol exists to prevent — and it has nothing to park, so it
    /// refuses cleanly.
    #[tokio::test]
    async fn a_wake_turn_under_a_parked_session_is_refused_too() {
        let agent = crate::testenv::mock_agent("http://127.0.0.1:0/v1", "f-3");
        {
            let s = agent.store.lock().unwrap();
            s.create_session("f-3").unwrap();
            s.conn()
                .execute(
                    "UPDATE sessions SET node = 'migrating_to:node-b' WHERE id = 'f-3'",
                    [],
                )
                .unwrap();
        }
        let mut history = Vec::new();
        let err = agent
            .run_background_wake_turn(&mut history, None, |_| {})
            .await
            .expect_err("the wake path is fenced too");
        assert!(err.to_string().contains("migrating to node-b"), "{err}");
        let parked = {
            let s = agent.store.lock().unwrap();
            s.queue_items("f-3").unwrap()
        };
        assert!(parked.is_empty(), "a wake turn has no user text to park");
    }

    #[tokio::test]
    async fn a_retired_session_refuses_turns_as_readable_history() {
        let agent = crate::testenv::mock_agent("http://127.0.0.1:0/v1", "f-2");
        {
            let s = agent.store.lock().unwrap();
            s.create_session("f-2").unwrap();
            s.conn()
                .execute(
                    "UPDATE sessions SET node = 'migrated_to:node-b@1700000000' WHERE id = 'f-2'",
                    [],
                )
                .unwrap();
        }
        let mut history = Vec::new();
        let err = agent
            .run_turn(&mut history, "resume me", None, |_| {})
            .await
            .expect_err("a retired session refuses turns");
        let msg = err.to_string();
        assert!(msg.contains("migrated away"), "{msg}");
        assert!(msg.contains("readable but not writable"), "{msg}");
    }

    // ── quiesce ───────────────────────────────────────────────────

    #[test]
    fn busy_names_a_live_turn_and_a_running_queue_row() {
        let (_d, store, _g) = scratch_store("busy");
        seed_session(&store, "q-1", None, "node-a");
        let sub = session_subtree(&store, "q-1").unwrap();
        assert!(is_busy(&store, &sub, 60).is_empty(), "idle = not busy");

        store
            .conn()
            .execute(
                "INSERT INTO session_runs (session_id, pid, host, surface, started_at, heartbeat_at)
                 VALUES ('q-1', 1, 'h', 'tui', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        let b = is_busy(&store, &sub, 60);
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("turn is live"), "{b:?}");

        store
            .conn()
            .execute("DELETE FROM session_runs WHERE session_id = 'q-1'", [])
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO session_queue (session_id, seq, body, state)
                 VALUES ('q-1', 1, 'mid-drain', 'running')",
                [],
            )
            .unwrap();
        let b = is_busy(&store, &sub, 60);
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("mid-drain"), "{b:?}");
    }
    // ── prepare(): the quiesce + child refusals (mutation targets) ──
    //
    // prepare() re-opens veles_home()/state.db for the REGISTRY (that
    // is what resolve_target dials by), so these tests seed BOTH the
    // sessions and the registry + [a2a_agents] route at veles_home —
    // the one store a real gateway uses. `me` is node-a; the target
    // node-b is registered, freshly heartbeated, routable, and not
    // local, so resolve_target succeeds and prepare reaches its own
    // refusal branches (the thing under mutation).

    /// Seed veles_home with node-a's identity, a routable node-b, and
    /// return the store at veles_home/state.db.
    /// The guard is RETURNED, not dropped here: scratch_home's
    /// DirOverride is thread-scoped and dies with the EnvGuard — a
    /// helper that swallows it leaves every later `veles_home()`
    /// reading the REAL home (caught by the DEBUG run: resolve_target
    /// opened /home/user/.local/share/veles mid-test).
    fn prep_env() -> (tempfile::TempDir, crate::testenv::EnvGuard, Store) {
        let (home, env) = crate::testenv::scratch_home();
        let cfgdir = crate::config::config_dir();
        std::fs::create_dir_all(&cfgdir).unwrap();
        std::fs::write(cfgdir.join("node.toml"), "id = \"node-a\"\nname = \"a\"\n").unwrap();
        std::fs::write(
            cfgdir.join("config.toml"),
            "[a2a_agents.b]\nurl = \"http://127.0.0.1:0/a2a\"\n\
             auth = { type = \"bearer\", token = \"t\" }\n",
        )
        .unwrap();
        // config::load() caches in a process-global LIVE cell; without
        // this the freshly-written [a2a_agents] is invisible and
        // resolve_target reports node-b unroutable ("no such node").
        crate::config::invalidate_live();
        let store = Store::open(&crate::config::veles_home().join("state.db")).unwrap();
        store
            .node_register(
                "node-b",
                "b",
                "h",
                "0",
                "always-on",
                None,
                Some("http://127.0.0.1:0/a2a"),
                None,
                None,
            )
            .unwrap();
        store.node_heartbeat("node-b", Some(5)).unwrap();
        (home, env, store)
    }

    fn me_a() -> crate::node::NodeIdentity {
        crate::node::NodeIdentity {
            id: "node-a".into(),
            name: "a".into(),
        }
    }

    #[test]
    fn prepare_refuses_to_migrate_a_session_with_a_live_turn() {
        let (_h, _env, store) = prep_env();
        seed_session(&store, "p-1", None, "node-a");
        store
            .conn()
            .execute(
                "INSERT INTO session_runs (session_id, pid, host, surface, started_at, heartbeat_at)
                 VALUES ('p-1', 1, 'h', 'tui', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        let err = prepare(&store, &me_a(), "b", "p-1", false)
            .expect_err("a live turn is not a turn boundary");
        assert!(
            err.to_string().contains("turn boundary"),
            "the refusal names the reason: {err:#}"
        );
        // Refused BEFORE parking — the stamp is untouched.
        assert!(
            !is_parked(store.session_node_stamp("p-1").unwrap().as_deref()),
            "a refused prepare changes nothing"
        );
    }

    #[test]
    fn migrating_a_child_whose_parent_stays_is_refused_unless_forced() {
        let (_h, _env, store) = prep_env();
        seed_session(&store, "keep-parent", None, "node-a");
        seed_session(&store, "lonely-child", Some("keep-parent"), "node-a");
        let err = prepare(&store, &me_a(), "b", "lonely-child", false)
            .expect_err("a lone child breaks the parent link");
        let msg = err.to_string();
        assert!(msg.contains("CHILD of keep-parent"), "{msg}");
        assert!(msg.contains("--force"), "the escape hatch is named: {msg}");
        assert!(
            !is_parked(store.session_node_stamp("lonely-child").unwrap().as_deref()),
            "refused before parking"
        );
        // --force takes it, orphaning the parent link honestly: the
        // prepare succeeds and parks the child alone.
        let Prepared {
            subtree: sub,
            bundle,
            ..
        } = prepare(&store, &me_a(), "b", "lonely-child", true).expect("forced");
        assert_eq!(sub.len(), 1, "the child alone");
        assert!(is_parked(
            store.session_node_stamp("lonely-child").unwrap().as_deref()
        ));
        assert_eq!(bundle.sessions.len(), 1);
    }

    #[test]
    fn prepare_parks_the_whole_subtree_and_builds_one_bundle() {
        let (_h, _env, store) = prep_env();
        seed_session(&store, "mig-root", None, "node-a");
        seed_session(&store, "mig-kid", Some("mig-root"), "node-a");
        let Prepared {
            subtree: sub,
            bundle,
            ..
        } = prepare(&store, &me_a(), "b", "mig-root", false).expect("prepares");
        assert_eq!(sub.len(), 2, "the subtree parks as a unit");
        assert!(is_parked(
            store.session_node_stamp("mig-root").unwrap().as_deref()
        ));
        assert!(is_parked(
            store.session_node_stamp("mig-kid").unwrap().as_deref()
        ));
        assert_eq!(bundle.sessions.len(), 2);
        assert_eq!(bundle.to_node, "node-b");
    }
}
