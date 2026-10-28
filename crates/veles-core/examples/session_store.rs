//! Session persistence + resume smoke test (no network).
//! Verifies: session create, message append (user/assistant/tool shapes),
//! FTS5 indexing trigger, history reload round-trip, and FTS search.
//!
//! Run: cargo run --example session_store

use veles_core::providers::ChatMessage;
use veles_core::store::Store;

fn main() {
    let dir = std::env::temp_dir().join("veles-store-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir.join("state.db")).expect("open");

    let sid = "20260809_000000_abc123";
    store.create_session(sid).expect("create session");

    // user message
    store
        .append_message(sid, &ChatMessage::user("remember the word purple-hippo-42"))
        .expect("append user");

    // assistant with reasoning, declaring the call the tool row answers
    // (a bare tool row is an orphan, which load_history repairs away)
    let mut assistant = ChatMessage::assistant(
        "the word is purple-hippo-42",
        Some("chain-of-thought".into()),
    );
    assistant.tool_calls = Some(serde_json::json!([{
        "id": "call_1",
        "type": "function",
        "function": {"name": "terminal", "arguments": "{}"}
    }]));
    store
        .append_message(sid, &assistant)
        .expect("append assistant");

    // tool result with metadata
    store
        .append_message(
            sid,
            &ChatMessage::tool_result("call_1", "terminal", "ls output here".into()),
        )
        .expect("append tool");

    // reload round-trip
    let hist = store.load_history(sid).expect("load");
    assert_eq!(hist.len(), 3, "expected 3 messages, got {}", hist.len());
    assert_eq!(hist[0].role, "user");
    assert_eq!(hist[1].role, "assistant");
    assert_eq!(hist[1].reasoning.as_deref(), Some("chain-of-thought"));
    assert_eq!(hist[2].role, "tool");
    let meta = hist[2].tool_meta.as_ref().expect("tool meta");
    assert_eq!(meta.tool_call_id, "call_1");
    assert_eq!(meta.name, "terminal");
    println!("1) round-trip: 3 messages, roles+reasoning+tool_meta intact");

    // latest_session finds it
    let latest = store.latest_session().expect("latest");
    assert_eq!(latest.as_deref(), Some(sid));
    println!("2) latest_session → {sid}");

    // FTS search hits the indexed content
    let hits = store.search("purple-hippo-42", 5).expect("search");
    assert!(!hits.is_empty(), "FTS search found nothing");
    println!("3) FTS search: {} hit(s) for 'purple-hippo-42'", hits.len());
    for (s, role, snip) in &hits {
        println!("   [{role}] {s}: {snip}");
    }

    // empty session is not returned as latest (needs messages)
    store.create_session("empty_session").expect("create empty");
    let latest = store.latest_session().expect("latest2");
    assert_eq!(latest.as_deref(), Some(sid), "empty session must not win");
    println!("4) empty sessions excluded from latest");

    println!("\nSESSION STORE SMOKE PASSED");
}
