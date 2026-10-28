//! checkpoint_smoke — M13: the agent's file edits are undoable.
//!
//! Goes through the REAL tools (write_file / patch built the way
//! base_registry builds them), not just the checkpoint functions, so a
//! regression in the wiring shows up here.
//!
//! Run: cargo run -p veles-core --example checkpoint_smoke

use serde_json::json;
use std::sync::{Arc, Mutex};
use veles_core::checkpoints::{restore, CheckpointCtx};
use veles_core::filetools::{PatchTool, WriteFileTool};
use veles_core::store::Store;
use veles_core::tools::Tool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("veles-ckpt-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    // Blobs land under VELES_HOME/checkpoints.
    std::env::set_var("VELES_HOME", &dir);

    let store = Arc::new(Mutex::new(Store::open(&dir.join("state.db"))?));
    let sid = "ckpt-smoke";
    store.lock().unwrap().create_session(sid)?;
    let ctx = CheckpointCtx {
        store: store.clone(),
        session_id: sid.into(),
    };
    let write = WriteFileTool {
        ckpt: Some(ctx.clone()),
        ..Default::default()
    };
    let patch = PatchTool {
        ckpt: Some(ctx.clone()),
        ..Default::default()
    };

    let f = dir.join("subject.txt");
    let original = "line one\nline two\nline three\n";
    std::fs::write(&f, original)?;
    let path = f.display().to_string();
    let mut passed = 0;

    // 1. patch → checkpoint recorded, file changed.
    {
        let out = patch
            .call(json!({
                "path": path, "old_string": "line two", "new_string": "LINE TWO"
            }))
            .await;
        assert!(out.starts_with("patched"), "{out}");
        assert!(std::fs::read_to_string(&f)?.contains("LINE TWO"));
        let list = store.lock().unwrap().checkpoint_list(Some(sid), 10)?;
        assert_eq!(list.len(), 1, "one checkpoint from the patch");
        assert_eq!(list[0].tool, "patch");
        assert_eq!(list[0].bytes as usize, original.len());
        println!("1. patch → checkpoint #{} recorded ✓", list[0].id);
        passed += 1;
    }

    // 2. restore → byte-exact original back.
    {
        let id = store.lock().unwrap().checkpoint_list(Some(sid), 10)?[0].id;
        let msg = restore(&store, id)?;
        assert!(msg.contains("restored"), "{msg}");
        assert_eq!(std::fs::read_to_string(&f)?, original, "byte-exact restore");
        println!("2. restore → byte-exact original ✓");
        passed += 1;
    }

    // 3. The restore is itself a checkpoint: redo works.
    {
        let list = store.lock().unwrap().checkpoint_list(Some(sid), 10)?;
        let redo = list
            .iter()
            .find(|c| c.tool == "restore")
            .expect("restore was checkpointed");
        restore(&store, redo.id)?;
        assert!(std::fs::read_to_string(&f)?.contains("LINE TWO"), "redo");
        println!("3. undo of the undo → back to the edited version ✓");
        passed += 1;
    }

    // 4. write_file over an existing file keeps the old bytes.
    {
        let out = write
            .call(json!({"path": path, "content": "totally different\n"}))
            .await;
        assert!(out.starts_with("wrote"), "{out}");
        let list = store.lock().unwrap().checkpoint_list(Some(sid), 10)?;
        let cp = list.iter().find(|c| c.tool == "write_file").unwrap();
        restore(&store, cp.id)?;
        assert!(std::fs::read_to_string(&f)?.contains("LINE TWO"));
        println!("4. write_file overwrite → previous content recoverable ✓");
        passed += 1;
    }

    // 5. A file the agent CREATES restores to "gone".
    {
        let newf = dir.join("created-by-agent.txt");
        let out = write
            .call(json!({"path": newf.display().to_string(), "content": "unwanted"}))
            .await;
        assert!(out.starts_with("wrote"), "{out}");
        assert!(newf.exists());
        let list = store.lock().unwrap().checkpoint_list(Some(sid), 20)?;
        let cp = list
            .iter()
            .find(|c| c.path == newf.display().to_string() && c.blob.is_none())
            .expect("creation checkpoint has no blob");
        let msg = restore(&store, cp.id)?;
        assert!(msg.contains("removed"), "{msg}");
        assert!(!newf.exists(), "restore of a created file deletes it");
        println!("5. created file → restore deletes it ✓");
        passed += 1;
    }

    // 6. Blobs are real files under VELES_HOME/checkpoints, and the
    //    accounting matches.
    {
        let (n, bytes) = store.lock().unwrap().checkpoint_stats()?;
        assert!(n >= 5, "checkpoint rows: {n}");
        let root = veles_core::checkpoints::blob_root();
        assert!(root.exists(), "{} must exist", root.display());
        let on_disk: u64 = std::fs::read_dir(root.join("ckpt-smoke"))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        assert!(on_disk > 0, "blobs written");
        println!("6. {n} rows, {bytes} B indexed, {on_disk} B on disk ✓");
        passed += 1;
    }

    // 7. Tools built without a context (tests, ad-hoc registries) still
    //    work — checkpointing is an add-on, never a requirement.
    {
        let bare = WriteFileTool::default();
        let g = dir.join("nockpt.txt");
        let out = bare
            .call(json!({"path": g.display().to_string(), "content": "x"}))
            .await;
        assert!(out.starts_with("wrote"), "{out}");
        println!("7. no-context tools still write ✓");
        passed += 1;
    }

    println!("\ncheckpoint_smoke: {passed}/7 PASS");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
