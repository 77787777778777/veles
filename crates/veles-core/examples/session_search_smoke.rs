//! session_search durable test — no network.
//!
//! Exercises all four calling shapes against a scratch VELES_HOME DB:
//!   1. DISCOVERY — FTS query returns sessions with bookends + window
//!   2. SCROLL    — around_message_id window paging
//!   3. READ      — whole session dump
//!   4. BROWSE    — recent session list
//!
//! Plus: tolerant fallback (punctuation-laden literal string),
//! and the recursion-guard surface (base registry lacks cronjob).
//!
//! Run: cargo run --example session_search_smoke

use std::sync::{Arc, Mutex};
use veles_core::providers::ChatMessage;
use veles_core::store::Store;

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join("veles-sessionsearch-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("VELES_HOME", &dir);

    let store = Arc::new(Mutex::new(
        Store::open(&dir.join("state.db")).expect("open"),
    ));
    {
        let s = store.lock().unwrap();
        s.create_session("s_alpha").unwrap();
        s.set_title("s_alpha", "the purple hippo session").unwrap();
        for (role, text) in [
            ("user", "remember the passphrase purple-hippo-42"),
            ("assistant", "noted: purple-hippo-42"),
            ("user", "what about the second word, brass-anvil?"),
            ("assistant", "second word is brass-anvil"),
            ("user", "final check: does the ledger hold?"),
            ("assistant", "the inbound_ledger dedups by UNIQUE key"),
        ] {
            s.append_message(
                "s_alpha",
                &ChatMessage {
                    role: role.into(),
                    content: text.into(),
                    reasoning: None,
                    display_reasoning: None,
                    tool_calls: None,
                    tool_meta: None,
                    attachments: Vec::new(),
                    injection: None,
                    msg_id: None,
                },
            )
            .unwrap();
        }
        s.create_session("s_beta").unwrap();
        s.append_message("s_beta", &ChatMessage::user("unrelated chat about weather"))
            .unwrap();
    }

    let tool = Arc::new(veles_core::session_search::SessionSearchTool {
        store: store.clone(),
    });
    use veles_core::tools::Tool;

    // 1) DISCOVERY
    let out = tool
        .call(serde_json::json!({"query": "purple-hippo-42", "limit": 3}))
        .await;
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    let results = v["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "discovery found nothing: {out}");
    let first = &results[0];
    assert_eq!(first["session_id"], "s_alpha");
    assert!(!first["bookend_start"].as_array().unwrap().is_empty());
    assert!(!first["messages"].as_array().unwrap().is_empty());
    assert!(first["match_message_id"].is_i64());
    assert!(first["snippet"].as_str().unwrap().contains(">>>"));
    println!(
        "1) DISCOVERY: {} session(s), bookends + window ✓",
        results.len()
    );

    // 2) SCROLL around the match
    let anchor = first["match_message_id"].as_i64().unwrap();
    let out = tool
        .call(
            serde_json::json!({"session_id": "s_alpha", "around_message_id": anchor, "window": 2}),
        )
        .await;
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    let msgs = v["messages"].as_array().expect("messages");
    assert!(
        msgs.len() >= 3 && msgs.len() <= 5,
        "window size wrong: {}",
        msgs.len()
    );
    assert!(
        msgs.iter().any(|m| m["id"] == anchor),
        "anchor must be inside its window"
    );
    println!("2) SCROLL: {} messages around #{anchor} ✓", msgs.len());

    // 3) READ
    let out = tool
        .call(serde_json::json!({"session_id": "s_alpha"}))
        .await;
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["total_messages"], 6);
    assert_eq!(v["messages"].as_array().unwrap().len(), 6);
    println!("3) READ: full 6-message session ✓");

    // 3b) READ with role_filter
    let out = tool
        .call(serde_json::json!({"session_id": "s_alpha", "role_filter": "user"}))
        .await;
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        v["total_messages"], 3,
        "role filter must count user rows only"
    );
    println!("3b) READ role_filter=user ✓");

    // 4) BROWSE
    let out = tool.call(serde_json::json!({})).await;
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    let recent = v["recent_sessions"].as_array().expect("recent");
    assert_eq!(recent.len(), 2, "both sessions listed");
    assert!(recent
        .iter()
        .any(|s| s["title"] == "the purple hippo session"));
    println!("4) BROWSE: {} sessions ✓", recent.len());

    // 5) tolerant fallback: literal with punctuation that FTS5 raw parses
    let out = tool
        .call(serde_json::json!({"query": "does the ledger hold?"}))
        .await;
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert!(
        !v["results"].as_array().unwrap().is_empty(),
        "phrase fallback failed: {out}"
    );
    println!("5) tolerant phrase fallback ✓");

    // 6) recursion guard surface: base registry must NOT expose cronjob,
    //    builtin registry MUST (that's the tool's home).
    let base = veles_core::tools::base_registry(store.clone(), "smoke");
    assert!(
        !base.names().contains(&"cronjob".to_string()),
        "cron job sessions must not get cronjob"
    );
    assert!(base.names().contains(&"session_search".to_string()));
    let full =
        veles_core::tools::builtin_registry(store.clone(), Some("telegram:123".into()), "smoke");
    assert!(full.names().contains(&"cronjob".to_string()));
    println!("6) registry split (recursion guard) ✓");

    let _ = std::fs::remove_dir_all(&dir);
    println!("\nSESSION SEARCH SMOKE PASSED");
}
