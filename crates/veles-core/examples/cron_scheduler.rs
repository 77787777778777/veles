//! Cron scheduler durable test — no network, no tokens.
//!
//! Verifies the whole durable path against a scratch VELES_HOME:
//!   1. schedule normalization + job CRUD round-trip
//!   2. no_agent script job: stdout delivered verbatim
//!   3. no_agent empty stdout → silent (watchdog semantics)
//!   4. failing script → error status + alert path
//!   5. exactly-once slot claim: two ticks, one execution
//!   6. one-shot consumed + auto-disabled
//!   7. repeat cap → auto-disabled at N runs
//!   8. catch-up after downtime fires exactly once
//!
//! Run: cargo run --example cron_scheduler
//! (agent/LLM jobs are exercised live by `veles cron run` — see HANDOFF)

use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use veles_core::cron::Scheduler;
use veles_core::hooks::HookBus;
use veles_core::store::{CronJob, Store};

fn job(id: &str, schedule: &str, script: Option<&str>, no_agent: bool, prompt: &str) -> CronJob {
    CronJob {
        id: id.into(),
        name: Some(id.into()),
        schedule: schedule.into(),
        prompt: prompt.into(),
        deliver: "local".into(),
        enabled: true,
        script: script.map(String::from),
        no_agent,
        ..Default::default()
    }
}

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join("veles-cron-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::env::set_var("VELES_HOME", &dir);

    let store = Arc::new(Mutex::new(
        Store::open(&dir.join("state.db")).expect("open"),
    ));
    let cfg = Arc::new(veles_core::config::load().expect("config"));
    let sched = Scheduler::new(store.clone(), cfg, HookBus::new(None), None);

    // ── scripts ────────────────────────────────────────────────────
    std::fs::write(
        dir.join("scripts/say.sh"),
        "#!/usr/bin/env bash\necho WATCHDOG-ALIVE-$$\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("scripts/quiet.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("scripts/broken.sh"),
        "#!/usr/bin/env bash\necho boom >&2\nexit 3\n",
    )
    .unwrap();

    // 1) normalization + CRUD round-trip
    let s = veles_core::cron::normalize_schedule("every 2h").unwrap();
    assert_eq!(s, "every:7200");
    store
        .lock()
        .unwrap()
        .cron_upsert(&job("scripty", "every:60", Some("say.sh"), true, ""))
        .unwrap();
    store
        .lock()
        .unwrap()
        .cron_upsert(&job("silent", "every:60", Some("quiet.sh"), true, ""))
        .unwrap();
    store
        .lock()
        .unwrap()
        .cron_upsert(&job("broken", "every:60", Some("broken.sh"), true, ""))
        .unwrap();
    let got = store.lock().unwrap().cron_get("scripty").unwrap().unwrap();
    assert_eq!(got.script.as_deref(), Some("say.sh"));
    assert!(got.no_agent);
    println!("1) CRUD round-trip ✓");

    // 2-4) run the three script jobs manually (delivery → stdout, local).
    // Bind the job and DROP the store lock BEFORE awaiting —
    // run_job_manual takes the store lock internally (it records the
    // run), and std::sync::Mutex is not reentrant (deadlock otherwise).
    let j = store.lock().unwrap().cron_get("scripty").unwrap().unwrap();
    let out = sched.run_job_manual(&j).await;
    // 'local' delivery prints the body to stdout; the summary carries the size.
    assert!(out.contains("bytes output"), "say.sh must deliver: {out}");
    let j = store.lock().unwrap().cron_get("silent").unwrap().unwrap();
    let out = sched.run_job_manual(&j).await;
    assert!(out.contains("silent"), "empty stdout must be silent: {out}");
    let j = store.lock().unwrap().cron_get("broken").unwrap().unwrap();
    let out = sched.run_job_manual(&j).await;
    assert!(out.contains("failed"), "broken script must fail: {out}");
    println!("2-4) script jobs: delivered / silent / error ✓");

    // 5+6) exactly-once slot claim + one-shot consumption.
    // One-shot due NOW: two ticks, only one run.
    let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    store
        .lock()
        .unwrap()
        .cron_upsert(&job(
            "oneshot",
            &format!("once:{past}"),
            Some("say.sh"),
            true,
            "",
        ))
        .unwrap();
    let sem = Arc::new(Semaphore::new(3));
    sched.tick(&sem).await.unwrap();
    // Wait for the spawned run to land.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    sched.tick(&sem).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let j = store.lock().unwrap().cron_get("oneshot").unwrap().unwrap();
    assert_eq!(j.run_count, 1, "one-shot ran {} times, want 1", j.run_count);
    assert!(!j.enabled, "one-shot must auto-disable");
    assert_eq!(j.last_status.as_deref(), Some("ok"));
    let runs = store
        .lock()
        .unwrap()
        .cron_run_history("oneshot", 10)
        .unwrap();
    assert_eq!(runs.len(), 1, "run history must have exactly 1 row");
    println!("5-6) exactly-once claim + one-shot consumed ✓");

    // 7) repeat cap: every:60 with repeat_max=1, anchor in the past → due.
    let mut capped = job("capped", "every:60", Some("quiet.sh"), true, "");
    capped.repeat_max = Some(1);
    capped.created_at = "2026-01-01 00:00:00".into();
    capped.updated_at = "2026-01-01 00:00:00".into();
    store.lock().unwrap().cron_upsert(&capped).unwrap();
    sched.tick(&sem).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let j = store.lock().unwrap().cron_get("capped").unwrap().unwrap();
    assert_eq!(j.run_count, 1);
    assert!(!j.enabled, "repeat cap must disable the job");
    println!("7) repeat cap → disabled ✓");

    // 8) catch-up after downtime: daily pattern, last ran 8 days ago.
    let mut late = job("latebird", "cron:0 9 * * *", Some("say.sh"), true, "");
    late.created_at = "2026-01-01 00:00:00".into();
    late.updated_at = "2026-01-01 00:00:00".into();
    let eight_days_ago = (chrono::Utc::now() - chrono::Duration::days(8))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    late.last_run_at = Some(eight_days_ago);
    store.lock().unwrap().cron_upsert(&late).unwrap();
    sched.tick(&sem).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    sched.tick(&sem).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let j = store.lock().unwrap().cron_get("latebird").unwrap().unwrap();
    assert_eq!(j.run_count, 1, "catch-up must run exactly once");
    assert!(j.enabled, "pattern job stays enabled after catch-up");
    assert!(
        j.next_run_at.is_some(),
        "next fire must be rescheduled forward"
    );
    println!("8) catch-up once + reschedule forward ✓");

    // 9) disabled jobs never fire. 'scripty' already ran twice via the
    // manual path in steps 2-4 (manual runs are recorded to history —
    // that's the point of the bookkeeping), so assert on the DELTA
    // across a tick, not an absolute count.
    let before = store
        .lock()
        .unwrap()
        .cron_get("scripty")
        .unwrap()
        .unwrap()
        .run_count;
    store
        .lock()
        .unwrap()
        .cron_set_enabled("scripty", false)
        .unwrap();
    sched.tick(&sem).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let j = store.lock().unwrap().cron_get("scripty").unwrap().unwrap();
    assert_eq!(j.run_count, before, "disabled job must not run");
    println!("9) disabled job skipped ✓");

    let _ = std::fs::remove_dir_all(&dir);
    println!("\nCRON SCHEDULER SMOKE PASSED");
}
