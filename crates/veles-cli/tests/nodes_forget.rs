//! `veles nodes forget` goes through the same guards as the Instances
//! tab and `/fleet forget` (audit FLEET-6): never this node, never a node
//! that still heartbeats. The CLI used to forget either, fleet-wide: this
//! node's own tombstone was never dropped (it gossiped it for 90 days and
//! listed itself as forgotten), and a live peer dropped out of every
//! registry until its next heartbeat.
//!
//! Hermetic: a scratch VELES_HOME, no gateway, no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ME: &str = "eeeeeeee-0000-4000-8000-00000000000e";

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        .stdin(Stdio::null());
    c
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn stamp(offset_secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(offset_secs))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn home() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-nodes-forget-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("node.toml"),
        format!("id = \"{ME}\"\nname = \"me\"\n"),
    )
    .unwrap();
    d
}

fn seed(home: &Path) {
    let store = veles_core::store::Store::open(&home.join("state.db")).unwrap();
    store
        .node_register(ME, "me", "h", "0.2.0", "ephemeral", None, None, None, None)
        .unwrap();
    for (id, heartbeat) in [("live-peer", stamp(-5)), ("retired-peer", stamp(-7200))] {
        let row: veles_core::store::GossipNodeRow = serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "host": "h", "version": "0.2.0",
            "uptime_class": "ephemeral",
            "registered_at": "2026-11-01 00:00:00",
            "heartbeat_at": heartbeat,
        }))
        .unwrap();
        store.node_merge_from_gossip(&row).unwrap();
    }
}

#[test]
fn the_cli_forget_refuses_this_node_and_a_live_one() {
    let home = home();
    seed(&home);
    let forget = |id: &str| {
        veles(&home)
            .args(["nodes", "forget", id])
            .output()
            .expect("run veles nodes forget")
    };
    let own = forget(ME);
    assert!(!own.status.success(), "{}", text(&own));
    assert!(text(&own).contains("this node"), "{}", text(&own));
    let live = forget("live-peer");
    assert!(!live.status.success(), "{}", text(&live));
    assert!(
        text(&live).contains("still heartbeating"),
        "{}",
        text(&live)
    );
    let retired = forget("retired-peer");
    assert!(retired.status.success(), "{}", text(&retired));

    let store = veles_core::store::Store::open(&home.join("state.db")).unwrap();
    assert!(store.node_get(ME).unwrap().is_some());
    assert!(store.node_get("live-peer").unwrap().is_some());
    assert!(store.node_get("retired-peer").unwrap().is_none());
    let tombs: Vec<String> = store
        .node_tombstones()
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(tombs, vec!["retired-peer".to_string()]);
    let _ = std::fs::remove_dir_all(&home);
}
