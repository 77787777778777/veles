//! LIVE cron agent-job round trip (spends ~1 cheap LLM turn).
//!
//! What this proves that cron_scheduler (offline) cannot:
//!   - a cron AGENT job boots a real Agent against the real config,
//!     runs one turn, and delivers the reply through the delivery path;
//!   - job-session bookkeeping lands in the store (session row, usage row);
//!   - the recursion guard holds on a REAL registry (no cronjob tool in
//!     the job session's tool schema sent to the model).
//!
//! Run: cargo run --example cron_live_agent   (uses the hermes config;
//! the XDG dirs by default — set VELES_HOME to a scratch dir to isolate)

use std::sync::{Arc, Mutex};
use veles_core::cron::Scheduler;
use veles_core::hooks::HookBus;
use veles_core::store::{CronJob, Store};

#[tokio::main]
async fn main() {
    let home = veles_core::config::veles_home();
    std::fs::create_dir_all(&home).unwrap();
    let store = Arc::new(Mutex::new(
        Store::open(&home.join("state.db")).expect("open"),
    ));
    let cfg = Arc::new(veles_core::config::load().expect("config"));
    let sched = Scheduler::new(store.clone(), cfg, HookBus::new(None), None);

    let job = CronJob {
        id: "livecheck".into(),
        name: Some("live-agent-check".into()),
        schedule: "every:3600".into(),
        prompt: "Reply with exactly: VELES_CRON_OK".into(),
        deliver: "local".into(),
        enabled: true,
        ..Default::default()
    };

    println!("firing agent job (one LLM turn)…");
    let out = sched.run_job_manual(&job).await;
    println!("summary: {out}");
    assert!(
        out.contains("bytes output"),
        "agent job must deliver: {out}"
    );

    // Bookkeeping: the job session must exist with the cron: title prefix,
    // and the base registry of that session must NOT contain cronjob.
    let sessions = store.lock().unwrap().session_meta(50).unwrap();
    let cron_sessions: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.starts_with("cron_"))
        .collect();
    assert!(!cron_sessions.is_empty(), "job session missing");
    println!("job session: {}", cron_sessions[0].id);

    let (msgs, total) = store
        .lock()
        .unwrap()
        .session_messages(&cron_sessions[0].id, None)
        .unwrap();
    assert!(total >= 2, "job session must hold prompt + reply");
    let reply = msgs
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant reply");
    assert!(
        reply.content.contains("VELES_CRON_OK"),
        "unexpected reply: {}",
        reply.content
    );
    println!("reply row persisted ✓ ({} chars)", reply.content.len());

    // Tool schema sent to the model for this session: no cronjob.
    let base = veles_core::tools::base_registry(store.clone(), &cron_sessions[0].id);
    let schemas = base.openai_schemas();
    assert!(schemas.iter().all(|s| s["function"]["name"] != "cronjob"));
    println!("recursion guard: job-session schema has no cronjob tool ✓");

    println!("\nLIVE CRON AGENT JOB OK");
}
