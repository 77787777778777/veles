//! S3 live check — background subagents: spawn with background=true,
//! get the child id IMMEDIATELY, then the next turn boundary injects the
//! completed child's summary as [BACKGROUND CHILD COMPLETE]. Proves the
//! async path without a chat platform.
//!
//! Run: `cargo run -p veles-core --example bg_live --release`
//! Uses the configured provider; spawns one quick child.

use std::sync::Arc;
use veles_core::agent::Agent;
use veles_core::config::load;
use veles_core::hooks::HookBus;
use veles_core::store::Store;
use veles_core::tools;
use veles_frontends::chat::new_session_id;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Background children spawn the `veles` binary; point them at the
    // release build (this example's own exe is NOT the runtime).
    std::env::set_var("VELES_BINARY", "./target/release/veles");
    let cfg = Arc::new(load()?);
    let scratch = std::env::temp_dir().join(format!("veles-bg-live-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let store = Arc::new(std::sync::Mutex::new(Store::open(
        &scratch.join("state.db"),
    )?));
    let hooks = HookBus::new(None);
    let sid = new_session_id();
    store.lock().unwrap().create_session(&sid)?;
    let registry = Arc::new(tools::builtin_registry(store.clone(), None, &sid));
    let agent = Arc::new(tokio::sync::Mutex::new(
        Agent::new(cfg.clone(), hooks, registry, store.clone(), sid.clone())?.with_surface("cli"),
    ));

    // Turn 1: ask the model to fire a background child and return. The
    // child does trivial work (echo a sentinel), so it finishes fast.
    let mut t1 = Vec::new();
    let r1 = agent
        .lock()
        .await
        .run_turn(
            &mut t1,
            "Use delegate_task with background=true: goal = 'Reply with exactly BG_SENTINEL_OK'. \
             After dispatching, reply with exactly: BG_STARTED.",
            None,
            |_| {},
        )
        .await?;
    println!("turn1 reply: {}", r1.reply);
    if !r1.reply.contains("BG_STARTED") {
        anyhow::bail!("agent did not confirm the background dispatch");
    }

    // Wait briefly for the child to finish (it's a same-model -q -Q run).
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;

    // Turn 2: the completion event must be injected at turn start.
    let mut t2 = Vec::new();
    let r2 = agent
        .lock()
        .await
        .run_turn(
            &mut t2,
            "Did any background child finish? If you see a [BACKGROUND CHILD COMPLETE] message, \
             reply with exactly: BG_DELIVERED",
            None,
            |_| {},
        )
        .await?;
    println!("turn2 reply: {}", r2.reply);
    if !r2.reply.contains("BG_DELIVERED") {
        anyhow::bail!("completion was not injected into the turn boundary");
    }
    std::fs::remove_dir_all(&scratch).ok();
    println!("BG_LIVE_OK");
    Ok(())
}
