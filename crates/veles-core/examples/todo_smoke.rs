//! todo_smoke — M12: the task list persists and survives compaction.
//!
//! The interesting parts are the two things hermes' in-memory list can't
//! do: come back after the process dies (resume), and reappear when the
//! history that described the plan is summarized away.
//!
//! Run: cargo run -p veles-core --example todo_smoke

use serde_json::json;
use std::sync::{Arc, Mutex};
use veles_core::store::{Store, TodoItem};
use veles_core::todo::{format_for_injection, TodoTool};
use veles_core::tools::Tool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("veles-todo-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let db = dir.join("state.db");
    let sid = "todo-smoke-session";
    let mut passed = 0;

    // 1. Write a list through the tool the model actually calls.
    {
        let store = Arc::new(Mutex::new(Store::open(&db)?));
        store.lock().unwrap().create_session(sid)?;
        let t = TodoTool {
            store: store.clone(),
            session_id: sid.into(),
        };
        let out = t
            .call(json!({"todos": [
                {"id": "1", "content": "port the approval gate", "status": "completed"},
                {"id": "2", "content": "wire the ask channel", "status": "in_progress"},
                {"id": "3", "content": "write the docs", "status": "pending"}
            ]}))
            .await;
        assert!(out.contains("[x] 1"), "{out}");
        assert!(out.contains("[>] 2"), "{out}");
        assert!(
            out.contains("1 pending · 1 in progress · 1 completed"),
            "{out}"
        );
        println!("1. tool write → rendered list with status marks ✓");
        passed += 1;
    }

    // 2. A NEW store handle (i.e. a restarted process) still has it.
    {
        let store = Arc::new(Mutex::new(Store::open(&db)?));
        let items = store.lock().unwrap().todos_list(sid)?;
        assert_eq!(items.len(), 3, "list survived the reopen");
        assert_eq!(items[1].status, "in_progress");
        println!(
            "2. reopen (resume) → list still there ✓ ({} items)",
            items.len()
        );
        passed += 1;
    }

    // 3. Merge updates one item and leaves the rest alone; replace
    //    drops what isn't sent.
    {
        let store = Arc::new(Mutex::new(Store::open(&db)?));
        let t = TodoTool {
            store: store.clone(),
            session_id: sid.into(),
        };
        let out = t
            .call(json!({"merge": true, "todos": [
                {"id": "2", "content": "wire the ask channel", "status": "completed"},
                {"id": "4", "content": "checkpoints", "status": "pending"}
            ]}))
            .await;
        assert!(out.contains("[x] 2"), "{out}");
        assert!(
            out.contains("[ ] 3 write the docs"),
            "merge kept item 3: {out}"
        );
        assert!(
            out.contains("[ ] 4 checkpoints"),
            "merge added item 4: {out}"
        );
        println!("3. merge → updates by id, keeps and appends ✓");
        passed += 1;
    }

    // 4. What compaction re-injects: open work only.
    {
        let store = Arc::new(Mutex::new(Store::open(&db)?));
        let items = store.lock().unwrap().todos_list(sid)?;
        let inj = format_for_injection(&items).expect("open items exist");
        assert!(inj.contains("write the docs"));
        assert!(inj.contains("checkpoints"));
        assert!(
            !inj.contains("port the approval gate"),
            "completed work must not be re-injected: {inj}"
        );
        assert!(inj.starts_with("[Your task list survived context compaction"));
        println!("4. compaction injection → open items only ✓");
        passed += 1;
    }

    // 5. Everything done → nothing to inject (no noise in the prompt).
    {
        let store = Arc::new(Mutex::new(Store::open(&db)?));
        let done: Vec<TodoItem> = store
            .lock()
            .unwrap()
            .todos_list(sid)?
            .into_iter()
            .map(|mut i| {
                i.status = "completed".into();
                i
            })
            .collect();
        store.lock().unwrap().todos_replace(sid, &done)?;
        let items = store.lock().unwrap().todos_list(sid)?;
        assert!(format_for_injection(&items).is_none());
        println!("5. all complete → nothing re-injected ✓");
        passed += 1;
    }

    // 6. Lists are per session.
    {
        let store = Arc::new(Mutex::new(Store::open(&db)?));
        store.lock().unwrap().create_session("other-session")?;
        assert!(store
            .lock()
            .unwrap()
            .todos_list("other-session")?
            .is_empty());
        println!("6. per-session isolation ✓");
        passed += 1;
    }

    println!("\ntodo_smoke: {passed}/6 PASS");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
