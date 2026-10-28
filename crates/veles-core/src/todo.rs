//! `todo` — the agent's own task list for a session.
//!
//! hermes keeps this list in memory on the AIAgent object, so it dies
//! with the process and a resumed session comes back plan-less. veles
//! persists it in SQLite (`todos`, keyed by session), which is what
//! makes `--resume`, gateway sessions and desktop reloads keep the plan.
//!
//! The other half of the feature is context compression: after a
//! compaction the open items are re-injected as a synthetic message
//! (`format_for_injection`) so the agent doesn't forget what it was
//! doing when the history it was doing it in gets summarized away.

use crate::store::{Store, TodoItem};
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub const MAX_ITEMS: usize = 256;
pub const MAX_CONTENT_CHARS: usize = 4_000;

const STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];

pub struct TodoTool {
    pub store: Arc<Mutex<Store>>,
    pub session_id: String,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        "Track the task list for the current piece of work. Call with no arguments to read \
the list. Send `todos` to write it: the full list replaces what's stored, or set merge=true \
to update/add items by id and leave the rest alone. Keep exactly one item in_progress. \
Use it for multi-step work so progress survives context compaction and session resume."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The task list. Omit to read the current one.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Stable short id"},
                            "content": {"type": "string", "description": "What needs doing"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["id", "content", "status"]
                    }
                },
                "merge": {
                    "type": "boolean",
                    "description": "Update by id instead of replacing the whole list (default false)"
                }
            }
        })
    }

    async fn call(&self, args: Value) -> String {
        let merge = args.get("merge").and_then(Value::as_bool).unwrap_or(false);
        match args.get("todos") {
            None | Some(Value::Null) => self.render(),
            Some(Value::Array(items)) => {
                if items.len() > MAX_ITEMS {
                    return format!("error: too many items ({} > {MAX_ITEMS})", items.len());
                }
                let mut parsed = Vec::with_capacity(items.len());
                for (i, it) in items.iter().enumerate() {
                    match parse_item(it, i) {
                        Ok(t) => parsed.push(t),
                        Err(e) => return format!("error: {e}"),
                    }
                }
                let res = {
                    let Ok(store) = self.store.lock() else {
                        return "error: store unavailable".into();
                    };
                    if merge {
                        store.todos_merge(&self.session_id, &parsed)
                    } else {
                        store.todos_replace(&self.session_id, &parsed)
                    }
                };
                match res {
                    Ok(()) => self.render(),
                    Err(e) => format!("error: {e}"),
                }
            }
            Some(other) => format!("error: todos must be an array, got {other}"),
        }
    }
}

impl TodoTool {
    fn render(&self) -> String {
        let items = match self.store.lock() {
            Ok(s) => s.todos_list(&self.session_id).unwrap_or_default(),
            Err(_) => return "error: store unavailable".into(),
        };
        render_list(&items)
    }
}

fn parse_item(v: &Value, idx: usize) -> Result<TodoItem, String> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("t{}", idx + 1));
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("item {} has no content", idx + 1))?;
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(format!(
            "item {} content exceeds {MAX_CONTENT_CHARS} chars",
            idx + 1
        ));
    }
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
        .to_ascii_lowercase();
    if !STATUSES.contains(&status.as_str()) {
        return Err(format!(
            "item {} has unknown status '{status}' (expected {})",
            idx + 1,
            STATUSES.join("|")
        ));
    }
    Ok(TodoItem {
        id,
        content: content.to_string(),
        status,
    })
}

/// Model-facing rendering: the list plus a one-line count summary.
pub fn render_list(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo list is empty".into();
    }
    let mut out = String::new();
    for it in items {
        let mark = match it.status.as_str() {
            "completed" => "x",
            "in_progress" => ">",
            "cancelled" => "-",
            _ => " ",
        };
        out.push_str(&format!("[{mark}] {} {}\n", it.id, it.content));
    }
    let count = |s: &str| items.iter().filter(|i| i.status == s).count();
    out.push_str(&format!(
        "— {} pending · {} in progress · {} completed · {} cancelled",
        count("pending"),
        count("in_progress"),
        count("completed"),
        count("cancelled")
    ));
    out
}

/// Open items only, phrased for re-injection after a compaction.
/// Completed/cancelled work is left out on purpose — repeating it
/// invites the model to redo it.
pub fn format_for_injection(items: &[TodoItem]) -> Option<String> {
    let open: Vec<&TodoItem> = items
        .iter()
        .filter(|i| i.status == "pending" || i.status == "in_progress")
        .collect();
    if open.is_empty() {
        return None;
    }
    let mut s =
        String::from("[Your task list survived context compaction — these items are still open]\n");
    for it in open {
        let mark = if it.status == "in_progress" { ">" } else { " " };
        s.push_str(&format!("[{mark}] {} {}\n", it.id, it.content));
    }
    Some(s.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Arc<Mutex<Store>>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "veles-todo-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("state.db")).unwrap();
        (Arc::new(Mutex::new(s)), dir)
    }

    #[tokio::test]
    async fn replace_and_merge_semantics() {
        let (store, dir) = store();
        store.lock().unwrap().create_session("s1").unwrap();
        let t = TodoTool {
            store: store.clone(),
            session_id: "s1".into(),
        };

        assert!(t.call(json!({})).await.contains("empty"));

        let out = t
            .call(json!({"todos":[
                {"id":"a","content":"write the parser","status":"in_progress"},
                {"id":"b","content":"write the tests","status":"pending"}
            ]}))
            .await;
        assert!(out.contains("[>] a write the parser"), "{out}");
        assert!(out.contains("1 pending · 1 in progress"), "{out}");

        // merge touches only the named item
        let out = t
            .call(json!({"merge": true, "todos":[
                {"id":"a","content":"write the parser","status":"completed"}
            ]}))
            .await;
        assert!(out.contains("[x] a"), "{out}");
        assert!(out.contains("[ ] b write the tests"), "{out}");

        // replace drops what isn't sent
        let out = t
            .call(json!({"todos":[{"id":"c","content":"ship","status":"pending"}]}))
            .await;
        assert!(!out.contains("write the tests"), "{out}");
        assert!(out.contains("[ ] c ship"), "{out}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bad_input_is_reported_not_swallowed() {
        let (store, dir) = store();
        store.lock().unwrap().create_session("s1").unwrap();
        let t = TodoTool {
            store: store.clone(),
            session_id: "s1".into(),
        };
        assert!(t.call(json!({"todos": "nope"})).await.starts_with("error:"));
        assert!(t
            .call(json!({"todos":[{"id":"a","content":"x","status":"maybe"}]}))
            .await
            .contains("unknown status"));
        assert!(t
            .call(json!({"todos":[{"id":"a","status":"pending"}]}))
            .await
            .contains("no content"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn injection_keeps_only_open_work() {
        let items = vec![
            TodoItem {
                id: "a".into(),
                content: "done thing".into(),
                status: "completed".into(),
            },
            TodoItem {
                id: "b".into(),
                content: "current thing".into(),
                status: "in_progress".into(),
            },
        ];
        let s = format_for_injection(&items).unwrap();
        assert!(s.contains("current thing"));
        assert!(!s.contains("done thing"));

        let all_done = vec![TodoItem {
            id: "a".into(),
            content: "x".into(),
            status: "completed".into(),
        }];
        assert!(format_for_injection(&all_done).is_none());
    }
}
