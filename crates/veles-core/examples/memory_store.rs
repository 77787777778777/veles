//! Memory store logic smoke (no tokens spent).
//! Run: VELES_HOME=/tmp/veles-mem-smoke cargo run --example memory_store

fn main() {
    std::env::var("VELES_HOME").expect("set VELES_HOME to a scratch dir");
    let store = veles_core::memory::MemoryStore::new();
    use veles_core::memory::StoreKind;

    // add → replace → remove on the memory store
    let r = store_apply(
        "memory",
        &[serde_json::json!({
            "action": "add",
            "content": "veles prefers the stable fenix toolchain (nightly breaks clib)"
        })],
    );
    println!("add:      {r}");
    assert!(r.contains("added"));

    let r = store_apply(
        "memory",
        &[serde_json::json!({
            "action": "replace",
            "old_text": "stable fenix toolchain",
            "content": "veles pins the STABLE fenix toolchain (nightly cargo breaks clib/inwelling)"
        })],
    );
    println!("replace:  {r}");
    assert!(r.contains("replaced"));

    // substring match must be unique
    let r = store_apply(
        "user",
        &[serde_json::json!({
            "action": "add",
            "content": "user drives agents from a phone over mosh"
        })],
    );
    println!("user add: {r}");
    // Memory is in the database — read it back through the store.
    assert!(store.read(StoreKind::User).contains("phone over mosh"));

    // remove
    let r = store_apply(
        "memory",
        &[serde_json::json!({
            "action": "remove",
            "old_text": "STABLE fenix toolchain"
        })],
    );
    println!("remove:   {r}");
    assert!(r.contains("removed"));
    assert!(
        !store.read(StoreKind::Memory).contains("fenix"),
        "entry should be gone"
    );

    // ambiguous substring → rejected
    store_apply(
        "memory",
        &[
            serde_json::json!({"action": "add", "content": "alpha note one"}),
            serde_json::json!({"action": "add", "content": "alpha note two"}),
        ],
    );
    let r = store_apply(
        "memory",
        &[serde_json::json!({
            "action": "remove",
            "old_text": "alpha note"
        })],
    );
    println!("ambiguous: {r}");
    assert!(r.contains("matches 2"), "must refuse ambiguous old_text");

    println!("\nMEMORY STORE SMOKE PASSED");
}

fn store_apply(target: &str, ops: &[serde_json::Value]) -> String {
    // call the tool directly (no agent needed)
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tool = veles_core::memory::MemoryTool {
        session_id: "memory-store-smoke".to_string(),
    };
    rt.block_on(veles_core::tools::Tool::call(
        &tool,
        serde_json::json!({
            "target": target,
            "operations": ops,
        }),
    ))
}
