//! Foreign-DB guard test — regression for the 2026-08-09 poisoning
//! incident (Demon's root cause): opening a NON-veles SQLite db (e.g.
//! Hermes' ~/.hermes/state.db) must NOT create the messages_ai trigger.
//!
//! Run: cargo run --example foreign_db_guard

use veles_core::store::Store;

fn main() {
    let hermes_db = std::path::Path::new("/home/user/.hermes/state.db");
    if !hermes_db.exists() {
        println!("SKIP: ~/.hermes/state.db not present");
        return;
    }
    let dir = std::env::temp_dir().join("veles-foreign-guard");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("state.db");
    std::fs::copy(hermes_db, &copy).unwrap();

    let store = Store::open(&copy).expect("open foreign db");
    let conn = store.conn();

    // 1. no trigger may exist in the foreign db
    let triggers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='trigger' AND name='messages_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        triggers, 0,
        "messages_ai trigger must NOT be created in a foreign db"
    );
    println!("1) no messages_ai trigger created in foreign db ✓");

    // 2. hermes' own schema must be untouched (hermes messages table has
    //    no msg_id column; our schema probe must not have created one)
    let has_msg_id: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages')
             WHERE name='msg_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_msg_id, 0, "foreign messages table must stay untouched");
    println!("2) foreign messages table untouched ✓");

    // 2b) the inbound_ledger (exactly-once dedup) must NOT be created in
    //     the foreign db either — same ownership gate as the trigger
    let has_ledger: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='table' AND name='inbound_ledger'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        has_ledger, 0,
        "inbound_ledger must NOT be created in a foreign db"
    );
    println!("2b) no inbound_ledger created in foreign db ✓");

    // 3. a REAL veles db still gets the trigger (no regression)
    let veles_dir = dir.join("velesdb");
    std::fs::create_dir_all(&veles_dir).unwrap();
    let veles_db = veles_dir.join("state.db");
    let vstore = Store::open(&veles_db).unwrap();
    let triggers: i64 = vstore
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='trigger' AND name='messages_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(triggers, 1, "fresh veles db MUST have the trigger");
    println!("3) fresh veles db still gets trigger ✓");

    // 4. and it actually works end-to-end (insert → FTS)
    vstore
        .append_message(
            "s1",
            &veles_core::providers::ChatMessage::user("hello world"),
        )
        .unwrap();
    let hits = vstore.search("hello", 5).unwrap();
    assert_eq!(hits.len(), 1);
    println!("4) veles db insert + FTS works ✓");

    println!("\nFOREIGN DB GUARD PASSED");
}
