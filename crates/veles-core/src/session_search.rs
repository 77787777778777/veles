//! session_search tool — recall across veles' own session history.
//!
//! Four calling shapes (hermes session_search parity):
//!   1. DISCOVERY — `query`: FTS5 search, deduped per session, top-N
//!      sessions each with bookends (first/last 3 user+assistant messages)
//!      and a ±window around the match.
//!   2. SCROLL — `session_id` + `around_message_id`: a ±window slice.
//!   3. READ — `session_id` only: whole session (first 20 + last 10 when
//!      large).
//!   4. BROWSE — no args: recent sessions with titles/previews.
//!
//! The FTS query is tried RAW first (supports AND/OR/quotes/wildcards),
//! falling back to a quoted phrase so literal strings with punctuation
//! still work — hermes' forgiving behavior, without giving up the syntax.
//!
//! Output is JSON text (the tool result goes to the model, not a human).

use crate::lockutil::LockOk;
use crate::store::{Store, StoredMessage};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const CONTENT_MAX: usize = 800;

pub struct SessionSearchTool {
    pub store: Arc<Mutex<Store>>,
}

pub fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}… [+{} chars]", s.chars().count() - max)
    }
}

fn msg_json(m: &StoredMessage) -> Value {
    let mut v = json!({
        "id": m.msg_id,
        "role": m.role,
        "content": trunc(&m.content, CONTENT_MAX),
        "created_at": m.created_at,
    });
    if let Some(t) = &m.tool_name {
        v["tool_name"] = json!(t);
    }
    v
}

// ── B5: the replica union ───────────────────────────────────────────
//
// session_search answers from the LOCAL store first, then the replica
// files this node holds (a peer's session history, replicated by B5).
// A session is shown once; a replica hit carries `owner`/`source` so
// the reader knows where the row lives and who owns it. Replica FTS is
// the CLI `replicas search` surface; the tool reuses it verbatim.

/// Replica hits as discovery results, skipping sessions already in
/// `exclude` (the local results — one session, one row).
fn replica_hits_json(query: &str, limit: u32, exclude: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(rhs) = crate::storesync::search_replicas(query, limit) {
        for h in rhs {
            if exclude.contains(&h.session_id) {
                continue;
            }
            out.push(json!({
                "session_id": h.session_id,
                "link": format!("@session:{}", h.session_id),
                "snippet": h.snippet,
                "match_message_id": h.msg_id,
                "when": h.created_at,
                "owner": h.node,
                "source": format!("replica:{}", h.node),
            }));
        }
    }
    out
}

/// Read `session_id` from the local store; when the local store does
/// not have it, the first REPLICA that does serves the read (read-only
/// fallback — the owner is asleep, history stays readable). Returns
/// None when nobody has it.
fn read_from_replica(session_id: &str) -> Option<Value> {
    replica_session_json(session_id, None)
}

/// SCROLL on a replica: a ±window slice around `around` (msg_id anchor,
/// replica-local). Returns None when no replica holds the session.
fn scroll_from_replica(session_id: &str, around: i64, window: u32) -> Option<Value> {
    replica_session_json(session_id, Some((around, window)))
}

/// One message row as discovery/read output (the replica spelling of
/// `msg_json` — no StoredMessage allocation in between).
fn msg_row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "role": r.get::<_, String>(1)?,
        "content": trunc(&r.get::<_, String>(2)?, CONTENT_MAX),
        "created_at": r.get::<_, String>(3)?,
    }))
}

/// The shared replica read: whole-session or windowed by
/// `Some((anchor, window))`. One replica file is opened at most.
fn replica_session_json(session_id: &str, window: Option<(i64, u32)>) -> Option<Value> {
    let infos = crate::storesync::list_replicas().ok()?;
    for info in infos {
        let Ok(conn) = rusqlite::Connection::open(crate::storesync::replica_path(&info.node))
        else {
            continue;
        };
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        let title: String = conn
            .query_row(
                "SELECT COALESCE(title, '') FROM sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |r| r.get(0),
            )
            .unwrap_or_default();
        let mut messages = Vec::new();
        let sql = match window {
            None => "SELECT msg_id, role, content, created_at FROM messages
                     WHERE session_id = ?1 ORDER BY msg_id"
                .to_string(),
            Some((_anchor, w)) => format!(
                "SELECT msg_id, role, content, created_at FROM messages
                 WHERE session_id = ?1
                   AND msg_id BETWEEN ?2 - {w} AND ?2 + {w}
                 ORDER BY msg_id"
            ),
        };
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let rows = match window {
                None => stmt.query_map(rusqlite::params![session_id], msg_row_json),
                Some((anchor, _)) => {
                    stmt.query_map(rusqlite::params![session_id, anchor], msg_row_json)
                }
            };
            if let Ok(rows) = rows {
                for m in rows.flatten() {
                    messages.push(m);
                }
            }
        }
        // First 20 + last 10 when large and unwindowed — the READ
        // shape's contract.
        let total = messages.len();
        if window.is_none() && total > 30 {
            let head: Vec<Value> = messages.iter().take(20).cloned().collect();
            let tail: Vec<Value> = messages.iter().skip(total - 10).cloned().collect();
            let mut v = head;
            v.extend(tail);
            messages = v;
        }
        let mut out = json!({
            "session_id": session_id,
            "title": title,
            "owner": info.node,
            "source": format!("replica:{}", info.node),
            "note": "read from a replica — the owner node holds the live row; \
                     writes go to the owner (B6's migration is the write path)",
            "total_messages": total,
            "returned": messages.len(),
            "messages": messages,
        });
        if let Some((anchor, _)) = window {
            out["around_message_id"] = json!(anchor);
        }
        return Some(out);
    }
    None
}

impl SessionSearchTool {
    fn discovery(&self, query: &str, limit: u32, sort: &str) -> String {
        let store = self.store.lock_ok();
        let hits = match search_tolerant(&store, query, limit * 8) {
            Ok(h) => h,
            Err(e) => return format!("error: FTS search failed: {e}"),
        };
        if hits.is_empty() {
            // B5: nothing LOCAL — the replicas may still hold it (a
            // peer's session history, findable while its owner sleeps).
            let replica_results = replica_hits_json(query, limit, &[]);
            if replica_results.is_empty() {
                return json!({"results": [], "note": "no matches in veles session history"})
                    .to_string();
            }
            return json!({"results": replica_results}).to_string();
        }
        // Dedupe per session, keep the best (first = highest rank) hit.
        let mut by_session: Vec<(String, usize)> = Vec::new(); // (session_id, hit index)
        for (i, h) in hits.iter().enumerate() {
            if !by_session.iter().any(|(s, _)| *s == h.session_id) {
                by_session.push((h.session_id.clone(), i));
            }
        }
        by_session.truncate(limit as usize);
        // The rows of exactly these sessions — not a scan of the newest
        // 1,000, which gave older sessions no position and no date.
        let ids: Vec<String> = by_session.iter().map(|(s, _)| s.clone()).collect();
        let rows = store.session_rows(&ids).unwrap_or_default();
        let row_of = |s: &str| rows.iter().find(|r| r.id == s);
        if sort == "oldest" {
            by_session
                .sort_by_key(|(s, _)| row_of(s).map(|r| r.updated_at.clone()).unwrap_or_default());
        }
        let mut results = Vec::new();
        for (session_id, hit_idx) in by_session {
            let hit = &hits[hit_idx];
            let title = store.title(&session_id).ok().flatten().unwrap_or_default();
            let created = row_of(&session_id)
                .map(|r| r.created_at.clone())
                .unwrap_or_default();
            let (bookend_start, bookend_end) =
                store.session_bookends(&session_id, 3).unwrap_or_default();
            let (window, before, after) = store
                .messages_around(&session_id, hit.msg_id, 5)
                .unwrap_or_default();
            results.push(json!({
                "session_id": session_id,
                "link": format!("@session:{session_id}"),
                "title": title,
                "when": created,
                "snippet": hit.snippet,
                "match_message_id": hit.msg_id,
                "messages_before": before,
                "messages_after": after,
                "bookend_start": bookend_start.iter().map(msg_json).collect::<Vec<_>>(),
                "messages": window.iter().map(msg_json).collect::<Vec<_>>(),
                "bookend_end": bookend_end.iter().map(msg_json).collect::<Vec<_>>(),
            }));
        }
        // B5 union: append hits from the REPLICA files this node holds —
        // sessions owned by peers, searchable while their owner is down
        // (the read surface, wired into the tool the way the CLI's
        // `replicas search` already was). Tagged with the source so the
        // reader knows where the row lives and who owns it. A session
        // that exists BOTH locally and in a replica is one session —
        // shown once, local first (local is fresher; a replica is at
        // best as new as its last apply).
        let local_ids: Vec<String> = results
            .iter()
            .filter_map(|r| r["session_id"].as_str().map(|s| s.to_string()))
            .collect();
        let replica_results = replica_hits_json(query, limit, &local_ids);
        results.extend(replica_results);
        json!({"results": results}).to_string()
    }

    fn scroll(&self, session_id: &str, around: i64, window: u32) -> String {
        let store = self.store.lock_ok();
        let (msgs, before, after) = match store.messages_around(session_id, around, window) {
            Ok(t) => t,
            Err(e) => return format!("error: {e}"),
        };
        if msgs.is_empty() {
            // B5: not local — a replica may hold it (the owner asleep,
            // history still readable). Windowed read on the replica.
            if let Some(v) = scroll_from_replica(session_id, around, window) {
                return v.to_string();
            }
            return format!("error: no message {around} in session {session_id}");
        }
        json!({
            "session_id": session_id,
            "around_message_id": around,
            "messages_before": before,
            "messages_after": after,
            "messages": msgs.iter().map(msg_json).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn read(&self, session_id: &str, role_filter: Option<&str>) -> String {
        let store = self.store.lock_ok();
        let (msgs, total) = match store.session_messages(session_id, role_filter) {
            Ok(t) => t,
            Err(e) => return format!("error: {e}"),
        };
        if msgs.is_empty() && total == 0 {
            // B5: not local — the first replica that holds it serves a
            // read-only copy (the owner's row stays the write target).
            if let Some(v) = read_from_replica(session_id) {
                return v.to_string();
            }
            return format!("error: session {session_id} not found or empty");
        }
        let title = store.title(session_id).ok().flatten().unwrap_or_default();
        json!({
            "session_id": session_id,
            "title": title,
            "total_messages": total,
            "returned": msgs.len(),
            "note": if (msgs.len() as u64) < total {
                "large session: first 20 + last 10 messages; use the scroll shape (session_id + around_message_id) to page"
            } else { "" },
            "messages": msgs.iter().map(msg_json).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn browse(&self) -> String {
        let store = self.store.lock_ok();
        let metas = match store.session_meta(20) {
            Ok(m) => m,
            Err(e) => return format!("error: {e}"),
        };
        let mut items = Vec::new();
        for m in &metas {
            let preview = store.preview(&m.id).unwrap_or_default();
            items.push(json!({
                "session_id": m.id,
                "link": format!("@session:{}", m.id),
                "title": m.title,
                "preview": preview,
                "created_at": m.created_at,
                "messages": m.message_count,
            }));
        }
        json!({"recent_sessions": items}).to_string()
    }
}

/// Raw FTS5 query first; on a syntax error fall back to a quoted phrase.
fn search_tolerant(
    store: &Store,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<crate::store::SearchHit>> {
    match store.search_raw(query, limit) {
        Ok(h) => Ok(h),
        Err(_) => store.search_full(query, limit),
    }
}

#[async_trait]
impl crate::tools::Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }
    fn description(&self) -> &str {
        "Search veles' own conversation history (SQLite FTS5). FOUR SHAPES: \
         (1) DISCOVERY — pass `query` (+limit ≤10, sort 'newest'/'oldest'): top sessions with \
         bookends, a ±5 window around the match, and messages_before/after. \
         (2) SCROLL — `session_id` + `around_message_id` (+window 1-20): page forward/back by \
         re-passing the boundary message id. \
         (3) READ — `session_id` only (+role_filter 'user,assistant'/'tool'): whole session, \
         first 20 + last 10 when large. \
         (4) BROWSE — no args: recent sessions. \
         FTS5 syntax supported: AND (default), OR, \"quoted phrases\", prefix*; punctuation-heavy \
         literal strings fall back to phrase matching automatically. \
         This is veles' OWN history, not the hermes DB — for external sources use their tools."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "FTS query (discovery shape)"},
                "limit": {"type": "integer", "description": "Max sessions, default 3, max 10"},
                "sort": {"type": "string", "enum": ["newest", "oldest"], "description": "Temporal bias (discovery only)"},
                "session_id": {"type": "string", "description": "Scroll/read shape"},
                "around_message_id": {"type": "integer", "description": "Scroll shape: window center"},
                "window": {"type": "integer", "description": "Scroll shape: messages per side, default 5"},
                "role_filter": {"type": "string", "description": "Comma-separated roles for READ shape"}
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        let session_id = args["session_id"].as_str().map(|s| s.trim().to_string());
        let around = args["around_message_id"].as_i64();
        match (session_id, around) {
            (Some(sid), Some(anchor)) => {
                let window = args["window"].as_u64().unwrap_or(5).clamp(1, 20) as u32;
                self.scroll(&sid, anchor, window)
            }
            (Some(sid), None) => {
                let rf = args["role_filter"].as_str();
                self.read(&sid, rf)
            }
            (None, _) => {
                let Some(query) = args["query"].as_str() else {
                    return self.browse();
                };
                let limit = args["limit"].as_u64().unwrap_or(3).clamp(1, 10) as u32;
                let sort = args["sort"].as_str().unwrap_or("");
                self.discovery(query, limit, sort)
            }
        }
    }
}
