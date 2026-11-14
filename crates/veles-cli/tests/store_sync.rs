//! The `veles` binary opens its session store with async durability: a
//! commit is answered when SQLite has it in the write-ahead log, and a
//! worker syncs it to the disk (`veles_core::durable`). `VELES_STORE_SYNC=full`
//! keeps the fsync on every commit. `veles doctor` says which it is.
//!
//! Before this every commit was an fsync made under the store's locks,
//! and under disk load the desktop stalled and chats beside the gateway
//! died of "database is locked" (2026-11-14).
//!
//! Hermetic: a scratch VELES_HOME, no gateway, no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        .env_remove("VELES_STORE_SYNC")
        .stdin(Stdio::null());
    c
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "veles-store-sync-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn store_line(home: &Path, sync: Option<&str>) -> String {
    let mut c = veles(home);
    if let Some(v) = sync {
        c.env("VELES_STORE_SYNC", v);
    }
    let out = c.args(["doctor"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    text.lines()
        .find(|l| l.trim_start().starts_with("store:"))
        .unwrap_or_else(|| panic!("doctor printed no store line:\n{text}"))
        .trim()
        .to_string()
}

#[test]
fn the_binary_syncs_its_store_in_the_background() {
    let h = home("async");
    let line = store_line(&h, None);
    assert!(line.starts_with("store: OK"), "{line}");
    assert!(
        line.contains("commits are synced in the background"),
        "{line}"
    );
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn the_environment_keeps_the_sync_on_every_commit() {
    let h = home("full");
    let line = store_line(&h, Some("full"));
    assert!(line.starts_with("store: OK"), "{line}");
    assert!(line.contains("every commit is synced"), "{line}");
    let _ = std::fs::remove_dir_all(&h);
}

/// The node's own `.env` is where its other node-local switches live.
#[test]
fn the_nodes_env_file_keeps_the_sync_on_every_commit_too() {
    let h = home("dotenv");
    std::fs::write(h.join(".env"), "VELES_STORE_SYNC=full\n").unwrap();
    let line = store_line(&h, None);
    assert!(line.contains("every commit is synced"), "{line}");
    let _ = std::fs::remove_dir_all(&h);
}
