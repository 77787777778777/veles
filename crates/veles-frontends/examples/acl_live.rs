//! S15 ACL live verification — a REAL LLM turn under a restricted
//! principal, proving the tool-dispatch gate denies and the model ADAPTS
//! (reads the denial, answers without the tool) instead of crashing.
//!
//! Run: `cargo run -p veles-core --example acl_live --release`
//! Costs one cheap turn on the configured provider.

use std::sync::Arc;
use veles_core::acl::{Cap, Principal};
use veles_core::agent::Agent;
use veles_core::config::load;
use veles_core::hooks::HookBus;
use veles_core::store::Store;
use veles_core::tools;
use veles_frontends::chat::new_session_id;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A restricted principal: user role — chat, file-read, web, todo,
    // and shell/file-write that route into a sandbox rather than the
    // host. NOT memory.
    let restricted = Principal::known("irc:live-test", veles_core::acl::Role::User);
    assert!(!restricted.has(Cap::MemoryRead));

    let cfg = Arc::new(load()?);
    // Scratch store so the test session never pollutes the real brain.
    let scratch = std::env::temp_dir().join(format!("veles-acl-live-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let store = Arc::new(std::sync::Mutex::new(Store::open(
        &scratch.join("state.db"),
    )?));
    let hooks = HookBus::new(None);
    let sid = new_session_id();
    store.lock().unwrap().create_session(&sid)?;
    let registry = Arc::new(tools::base_registry(store.clone(), &sid));
    let agent = Agent::new(cfg, hooks, registry, store.clone(), sid.clone())?
        .with_principal(restricted)
        .with_surface("cli");

    // The turn: ask for something that TEMPTS a terminal call, and
    // demand the reply carry a sentinel.
    let mut history = Vec::new();
    let res = agent
        .run_turn(
            &mut history,
            "Run the shell command `uname -a` and tell me the kernel version. \
If the terminal tool is unavailable or denied, reply exactly: TOOLBOX_CLOSED",
            None,
            |_ev| {},
        )
        .await?;

    println!("reply: {}", res.reply);
    println!(
        "tool rounds: {} · provider {} / {}",
        res.tool_rounds, res.provider, res.model
    );

    // Accept either a graceful sentinel reply or a reply that clearly
    // didn't run the command (the model must NOT present kernel output
    // it never obtained). The store must show the denial as a tool row.
    let ok = res.reply.contains("TOOLBOX_CLOSED")
        || res.reply.to_lowercase().contains("denied")
        || res.reply.to_lowercase().contains("not available")
        || res.reply.to_lowercase().contains("cannot")
        || res.reply.to_lowercase().contains("unable");
    let hist = store.lock().unwrap().load_history(&sid)?;
    let denial_row = hist
        .iter()
        .any(|m| m.role == "tool" && m.content.contains("access denied"));
    println!("sentinel/adaptive-reply: {ok} · denial persisted in history: {denial_row}");
    if !(ok && denial_row) {
        anyhow::bail!("ACL live check failed: model did not visibly adapt to the denial");
    }
    std::fs::remove_dir_all(&scratch).ok();
    println!("ACL_LIVE_OK");
    Ok(())
}
