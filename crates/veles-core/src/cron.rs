//! Cron scheduler — durable jobs + ticker + runner (M2 tail).
//!
//! Design notes (clean-room; hermes' cron module is the parity target but
//! its implementation is not the model):
//!
//!   - **Normalized schedules.** The tool surface accepts hermes-shaped
//!     input ('30m', 'every 2h', '0 9 * * *', ISO timestamps) but the DB
//!     stores a self-describing normalized form:
//!     `once:<rfc3339-utc> | every:<seconds> | cron:<spec>`.
//!     The scheduler never re-parses user syntax.
//!   - **Exactly-once per slot.** Every due instant gets a slot key
//!     (`cron:<job_id>:<slot>`) claimed through the SAME durable
//!     `inbound_ledger` mechanism as message dedup (`Store::claim_inbound`).
//!     Two gateway processes, a crash-and-restart, or an overlapping tick
//!     can never run the same slot twice. Keys TTL out after 30 days.
//!   - **Catch-up once.** Across a restart/downtime a missed schedule runs
//!     exactly ONE catch-up execution (the first missed instant), then
//!     reschedules forward from now. No burst replay.
//!   - **Recursion guard by construction.** Job sessions get the base tool
//!     registry, which does NOT contain the `cronjob` tool — a cron job
//!     literally cannot schedule more cron jobs (hermes enforces this with
//!     a prompt instruction only).
//!   - **Fresh session per run** (hermes parity: no current-chat context).
//!     `no_agent` script jobs skip the LLM entirely: stdout delivered
//!     verbatim, empty stdout = silent (watchdog semantics), non-zero
//!     exit = error alert.
//!   - **Delivery.** 'origin' (creation chat, captured at register time),
//!     'local' (log only), or explicit 'platform:chat_id'. When no adapter
//!     is listening (e.g. standalone `veles cron run`), output goes to
//!     stdout and the run detail says so — never silently dropped.

use crate::agent::Agent;
use crate::bus::{GatewayBus, MessageKind, OutboundMessage};
use crate::config::VelesConfig;
use crate::hooks::HookBus;
use crate::lockutil::{LockOk, RwLockOk};
use crate::store::{CronJob, Store};
use crate::tools;
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, NaiveDateTime, TimeZone, Timelike, Utc};
use croner::Cron;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

/// Hard cap for one job run (agent or script). hermes has no real cap on
/// agent jobs; a wedged provider must not hold the scheduler hostage.
const AGENT_JOB_TIMEOUT_SECS: u64 = 600;
const SCRIPT_JOB_TIMEOUT_SECS: u64 = 300;
/// Max jobs executing concurrently.
const MAX_CONCURRENT_RUNS: usize = 3;
/// Ticker granularity. One-shot jobs fire within this window.
const TICK_SECS: u64 = 15;

/// Store meta key holding the last dream() run (RFC3339). The anchor
/// for the built-in schedule's next fire, and the reason a restart does
/// not re-reflect immediately.
const DREAM_LAST_RUN: &str = "dream.last_run_at";

// ── the session-wakeup serialization seam (Stage 5) ─────────────────
//
// A wakeup runs a turn IN a session a human may be using, and the
// scheduler is not that session's surface: the gateway serializes its
// turns behind a per-chat lock, the desktop behind a per-session one,
// and the scheduler can see neither. Its `runs_live` check is a
// snapshot — an inbound message that takes the owner's lock a
// millisecond later starts a SECOND concurrent turn in the same
// session, interleaving two transcripts into one store. "Never a
// second concurrent turn" is this codebase's stated invariant (the
// gateway has a test named for it), so the scheduler borrows the
// owner's own lock across the turn — the same `try_lock`, stand-down-
// if-held shape the gateway's background-wake path uses.
//
// The turn itself still runs HERE, under the job's own privileges
// (USER decision, Stage 5 c2: scheduler/owner-class regardless of
// whose session this is — matching how fresh-session cron jobs have
// always run) and delivers via `job.deliver`. Delegating the turn to
// the owner surface instead would run it under the CHAT's principal,
// which is the c1 answer the user explicitly did not pick.
//
// One residual the lock does not cover, closed by `kick_queue`: a
// message that arrives DURING the scheduler's turn fails the owner's
// `try_lock` and parks, and the owner only drains its queue at a turn
// boundary — but the boundary just passed (ours, and we are not the
// owner). Without a kick the user was told "queued, runs next" and
// then waits for the next inbound. So after the turn, the scheduler
// hands any parked rows back to the owner's normal pipeline (the
// gateway re-injects one row through the bus exactly like its wake
// drain does — the handle path then runs it with the RIGHT principal;
// the desktop spawns its drain loop).
//
// Ownership mirrors session_wake's split: the gateway owns chat-backed
// sessions, the desktop the ones it holds that the gateway does not,
// and `owns_session` on each answers for itself. No live surface owns
// REPL/TUI/CLI sessions — there the store's `runs_live` truth is the
// only serializer, and the wakeup path consults it before running.

/// One turn-owning surface, as the scheduler sees it.
pub trait SessionRunner: Send + Sync + 'static {
    /// Do I hold this session? (The gateway: it has a chat for it; the
    /// desktop: a cached agent, minus the gateway's.)
    fn owns_session(&self, session_id: &str) -> bool;

    /// Take my turn lock for this session WITHOUT waiting. `Some` =
    /// held guard: the session is mine and free, run your turn under
    /// it. `None` = mine and a turn is running — defer; my boundary
    /// drain covers anything parked.
    fn try_lock_session(&self, session_id: &str) -> Option<SessionLockGuard>;

    /// Hand parked queue rows back to my normal pipeline after a
    /// foreign turn ended under my lock (see the seam's comment —
    /// without this, a message parked during a cron turn waits for the
    /// next inbound). Best-effort; never blocks.
    fn kick_queue(&self, session_id: &str);
}

/// Held across the woken turn. Boxed: the concrete guard type belongs
/// to the owner surface (tokio's `OwnedMutexGuard`). The field is
/// never READ — its whole purpose is the `Drop` that releases the
/// owner's lock when the wakeup turn ends.
pub struct SessionLockGuard(#[allow(dead_code)] Box<dyn Send + 'static>);

impl SessionLockGuard {
    pub fn new(g: impl Send + 'static) -> Self {
        Self(Box::new(g))
    }
}

/// The session-runner registry. OWNER-TRACKED (M4/R2): each runner
/// records the registering thread's owner, so a module surface's runner
/// is purged with it on reload. Keyed by a registration sequence (the
/// runners are a SET iterated for a first `owns_session` match; surfaces
/// own disjoint sessions, so iteration order does not matter).
static SESSION_RUNNERS: std::sync::OnceLock<
    crate::owned::OwnedRegistry<u64, Arc<dyn SessionRunner>>,
> = std::sync::OnceLock::new();
static RUNNER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn session_runners() -> &'static crate::owned::OwnedRegistry<u64, Arc<dyn SessionRunner>> {
    SESSION_RUNNERS.get_or_init(crate::owned::OwnedRegistry::default)
}

/// A surface registers itself when it starts serving turns (the gateway
/// in `serve`, the desktop server alongside its wake loop). Per
/// process; a stale registration whose sessions map is empty answers
/// "not mine" and is inert. Tests reset via `clear_session_runners`.
/// The owner is read from the calling thread (M4).
pub fn register_session_runner(r: Arc<dyn SessionRunner>) {
    let k = RUNNER_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    session_runners().insert(k, r);
}

/// Remove every runner `owner` registered (R3 teardown). `None` (core)
/// removes nothing. Returns the count removed.
pub fn purge_owner_session_runners(owner: Option<&'static str>) -> usize {
    session_runners().purge(owner)
}

#[cfg(test)]
pub fn clear_session_runners() {
    for k in session_runners().keys() {
        session_runners().remove(&k);
    }
}

/// Ask the registered surfaces for this session's turn lock.
/// `(owner_found, guard)`:
///   - `(true, Some(g))` — owned and FREE: hold `g` across the turn;
///   - `(true, None)` — owned and mid-turn: defer (park the prompt;
///     the running turn's boundary drain will run it);
///   - `(false, None)` — no live surface owns it (REPL/TUI/CLI,
///     standalone): the scheduler runs directly, serialized only by
///     the store's `runs_live` truth.
pub fn try_lock_session_turn(session_id: &str) -> (bool, Option<SessionLockGuard>) {
    let mut out = (false, None);
    session_runners().for_each(|_, r| {
        if out.0 {
            return;
        }
        if r.value.owns_session(session_id) {
            out = (true, r.value.try_lock_session(session_id));
        }
    });
    out
}

/// Post-turn queue kick for an owned session; no-op when unowned.
pub fn kick_session_queue(session_id: &str) {
    let mut done = false;
    session_runners().for_each(|_, r| {
        if done {
            return;
        }
        if r.value.owns_session(session_id) {
            r.value.kick_queue(session_id);
            done = true;
        }
    });
}

pub struct Scheduler {
    store: Arc<std::sync::Mutex<Store>>,
    cfg: Arc<VelesConfig>,
    hooks: Arc<HookBus>,
    /// None for standalone runs (`veles cron run`) — delivery then falls
    /// back to stdout.
    bus: Option<GatewayBus>,
    running: Arc<AsyncMutex<()>>,
    /// The B3a singleton lease guard. None = fence not armed
    /// (single-node mode); a standalone `veles cron run` also runs
    /// ungated, but its slot claims are exactly-once in the shared
    /// store, so a double standalone run duplicates work only in the
    /// sense one of the two loses every claim race.
    lease: std::sync::Mutex<Option<Arc<crate::arbiter::LeaseGuard>>>,
    /// What the last tick found, so a STANDBY node says so once per
    /// change instead of every 15 s forever.
    lease_was_held: std::sync::atomic::AtomicBool,
}

impl Scheduler {
    pub fn new(
        store: Arc<std::sync::Mutex<Store>>,
        cfg: Arc<VelesConfig>,
        hooks: Arc<HookBus>,
        bus: Option<GatewayBus>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            cfg,
            hooks,
            bus,
            running: Arc::new(AsyncMutex::new(())),
            lease: std::sync::Mutex::new(None),
            lease_was_held: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Attach the singleton lease guard (B3a). The gateway calls this
    /// after acquiring `singleton:cron`; the guard's expiry-aware
    /// `held()` is what every tick re-validates against — the plan's
    /// "re-validate immediately before every side effect".
    pub fn set_lease(&self, guard: Arc<crate::arbiter::LeaseGuard>) {
        *self.lease.lock().unwrap_or_else(|p| p.into_inner()) = Some(guard);
    }

    /// May this scheduler fire? True when no fence is armed (single-node
    /// mode) or the lease guard still holds. False = fail closed: the
    /// tick skips claiming and firing entirely (a lost lease must never
    /// fire a slot on the strength of a remembered check).
    fn lease_held(&self) -> bool {
        let g = self.lease.lock().unwrap_or_else(|p| p.into_inner());
        match g.as_ref() {
            None => true, // fence not armed
            Some(guard) => guard.held(),
        }
    }

    /// Run until cancelled. Safe to call once per process.
    pub async fn run_loop(self: &Arc<Self>) {
        // Single-flight guard: two `run_loop`s in one process would double
        // every slot claim attempt (harmless thanks to the ledger, but
        // wasteful and confusing in logs).
        if self.running.try_lock().is_err() {
            tracing::warn!("scheduler run_loop already active; second call ignored");
            return;
        }
        tracing::info!("cron scheduler started ({}s tick)", TICK_SECS);
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS));
        loop {
            tick.tick().await;
            if let Err(e) = self.tick(&sem).await {
                tracing::error!("cron tick failed: {e:#}");
            }
        }
    }

    /// One scheduler pass: find due jobs, claim their slots, spawn runs.
    /// Public so tests/examples (and `veles cron` smoke checks) can drive
    /// a single deterministic pass without the interval loop.
    pub async fn tick(self: &Arc<Self>, sem: &Arc<Semaphore>) -> Result<()> {
        // B3a fence: re-validate the lease BEFORE any side effect —
        // never "I checked when the loop started". A lost lease skips
        // Lease gate (2026-10-22 rework): a job pinned to THIS node
        // (`node = <me>`) is a node-local duty and fires whenever this
        // process is up — it must NOT wait for a global lease, or a
        // laptop job would never run while the always-on VPS holds it
        // (observed live: two daily update jobs sat enabled for 30h and
        // never fired). Only LEGACY floating jobs (node IS NULL) need the
        // lease, because those are the ones that would double-fire if two
        // nodes ran them. So: load jobs first, split by pin, and consult
        // the lease only for the floating half.
        let now = Utc::now();
        let jobs = self.store.lock_ok().cron_list(true)?;
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())
            .map(|i| i.id)
            .unwrap_or_default();
        let has_floating = jobs.iter().any(|j| j.node.is_none());
        if has_floating {
            let held = self.lease_held();
            if held
                != self
                    .lease_was_held
                    .swap(held, std::sync::atomic::Ordering::Relaxed)
            {
                if held {
                    tracing::info!(
                        "cron: singleton lease held — this node fires floating cron now"
                    );
                } else {
                    tracing::warn!(
                        "cron: singleton lease not held — floating jobs pause (pinned jobs unaffected)"
                    );
                }
            }
            if !held {
                // Drop the floating jobs from this pass; keep the pinned ones.
                let jobs = jobs
                    .into_iter()
                    .filter(|j| j.node.as_deref() == Some(me.as_str()))
                    .collect::<Vec<_>>();
                self.fire_due(jobs, sem, now).await?;
                self.maybe_dream(now).await;
                return Ok(());
            }
        }
        // Lease held (or no floating jobs at all): pinned jobs are
        // already limited to this node by construction — a pinned job
        // naming ANOTHER node is someone else's duty; skip it here.
        let jobs = jobs
            .into_iter()
            .filter(|j| j.node.as_deref().is_none_or(|n| n == me))
            .collect::<Vec<_>>();
        self.fire_due(jobs, sem, now).await?;
        self.maybe_dream(now).await;
        Ok(())
    }

    /// The firing half of a tick, split out so the lease paths above
    /// share it. Claims each due slot and spawns its run; a slot claimed
    /// is a promise — whoever takes the claim key owns the run.
    async fn fire_due(
        self: &Arc<Self>,
        jobs: Vec<CronJob>,
        sem: &Arc<Semaphore>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        // Keep the display column fresh for pattern/interval jobs.
        for job in &jobs {
            if let Ok(Some(next)) = next_fire(&job.schedule, job, now) {
                let _ = self
                    .store
                    .lock()
                    .unwrap()
                    .cron_set_next(&job.id, Some(&next.to_rfc3339()));
            }
        }
        for job in jobs {
            let Some(slot_time) = due_slot(&job, now)? else {
                continue;
            };
            let key = format!("cron:{}:{}", job.id, slot_time);
            let fresh = self.store.lock_ok().claim_inbound(&key)?;
            if !fresh {
                continue; // another process/tick already took this slot
            }
            let permit = sem.clone().acquire_owned().await;
            let Ok(permit) = permit else { break };
            let sched = self.clone();
            let job2 = job.clone();
            crate::rt::spawn(async move {
                let _permit = permit;
                sched.run_job(&job2, slot_time).await;
            });
        }
        Ok(())
    }

    /// Store the last dream() run so the next fire anchors on it.
    /// SQLite-UTC text, the format `next_fire` parses out of
    /// `last_run_at` — an RFC3339 string here would silently fail to
    /// parse and fall through to `created_at`, refiring on every tick.
    fn record_dream_run(store: &Store) {
        let stamp = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let _ = store.meta_set(DREAM_LAST_RUN, &stamp);
    }

    /// The built-in `dream()` schedule, config-gated.
    ///
    /// dream() is the only automatic memory writer, and before this it
    /// had exactly one caller — `veles dream` on a command line nobody
    /// typed. Rather than teach the cron_jobs table about a builtin row
    /// (or shell out to the `veles` binary from a script job, which
    /// depends on PATH inside the gateway's environment), the scheduler
    /// checks the config knob directly and reuses the SAME schedule
    /// math and exactly-once claim ledger as every other job.
    async fn maybe_dream(self: &Arc<Self>, now: DateTime<Utc>) {
        let Some(raw) = self.cfg.memory.dream_schedule.as_deref() else {
            return;
        };
        let last = self.store.lock_ok().meta_get(DREAM_LAST_RUN);
        let slot = match dream_slot(raw, last.as_deref(), now) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                tracing::error!("memory.dream_schedule '{raw}' is unusable: {e:#}");
                return;
            }
        };
        let key = format!("dream:{slot}");
        let fresh = self.store.lock_ok().claim_inbound(&key).unwrap_or(false);
        if !fresh {
            return;
        }
        // Record the slot BEFORE the run, not after: if dream takes
        // longer than the tick interval (it runs a whole LLM turn), a
        // post-run write would leave the anchor stale and the next tick
        // would compute the same slot and spawn a second reflection.
        // The claim ledger already guards the slot itself; this guards
        // the anchor.
        if let Ok(store) = self.store.lock() {
            Self::record_dream_run(&store);
        }
        tracing::info!("dream firing (schedule {raw}, slot {slot})");
        let cfg = self.cfg.clone();
        crate::rt::spawn(async move {
            match crate::dream::dream(cfg).await {
                Ok(text) => {
                    let first = text.lines().next().unwrap_or("").trim();
                    tracing::info!("dream finished: {first}");
                }
                Err(e) => tracing::error!("dream failed: {e:#}"),
            }
        });
    }

    /// Execute one claimed slot end-to-end and record bookkeeping.
    async fn run_job(self: &Arc<Self>, job: &CronJob, slot: String) {
        let name = job.name.clone().unwrap_or_else(|| job.id.clone());
        tracing::info!("cron job '{name}' firing (schedule {})", job.schedule);
        let outcome = self.run_body(job).await;
        let summary = self.finish_run(job, outcome, &slot.clone()).await;
        tracing::info!("cron job '{name}' finished: {summary}");
    }

    /// Which body does this job run? One place decides, so the tick
    /// path and the manual-fire path cannot drift: script jobs run the
    /// script; agent jobs with a named `session` RESUME it (Stage 5
    /// wakeup); every other agent job mints a fresh `cron_*` session.
    async fn run_body(self: &Arc<Self>, job: &CronJob) -> JobOutcome {
        if job.no_agent {
            return self.run_script_job(job).await;
        }
        match job
            .session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(sid) => self.run_session_wakeup_job(job, sid).await,
            None => self.run_agent_job(job).await,
        }
    }

    /// Manual fire (`cronjob action=run` / `veles cron run`): execute the
    /// job body + delivery WITHOUT consuming a schedule slot or advancing
    /// run bookkeeping. Returns a human-readable summary.
    pub async fn run_job_manual(self: &Arc<Self>, job: &CronJob) -> String {
        let outcome = self.run_body(job).await;
        // Record the manual run so /api/cron/:id/history (and the CLI
        // `cron history`) see it. Scheduled ticks record via finish_run;
        // the manual path never went through that bookkeeping before.
        // A wakeup job records WHICH session it ran in — the run row's
        // session column exists exactly for this, and "the job woke my
        // session at 09:00" is a fact the history should carry.
        let (status, detail) = match &outcome {
            JobOutcome::Delivered { text, .. } => ("ok", format!("{} bytes output", text.len())),
            JobOutcome::Silent => ("silent", "no output".to_string()),
            JobOutcome::Queued { detail } => ("queued", detail.clone()),
            JobOutcome::Failed(e) => ("error", format!("failed: {e:#}")),
        };
        let next = next_fire(&job.schedule, job, chrono::Utc::now())
            .ok()
            .flatten()
            .map(|dt| dt.to_rfc3339());
        if let Ok(store) = self.store.lock() {
            let _ = store.cron_record_run(
                &job.id,
                status,
                job.session.as_deref(),
                Some(&detail),
                next.as_deref(),
            );
        }
        match &outcome {
            JobOutcome::Delivered { text, detail, .. } => {
                format!("{} ({} bytes output)", detail, text.len())
            }
            JobOutcome::Silent => "script produced no output (silent)".into(),
            JobOutcome::Queued { detail } => detail.clone(),
            JobOutcome::Failed(e) => format!("failed: {e:#}"),
        }
    }

    /// Common post-run bookkeeping for scheduled executions. `slot` is the
    /// claimed slot identity — next fire is computed FORWARD from it (never
    /// from the stored next_run_at, which still points at the slot we just
    /// consumed and would refire immediately).
    async fn finish_run(
        self: &Arc<Self>,
        job: &CronJob,
        outcome: JobOutcome,
        slot: &str,
    ) -> String {
        let name = job.name.clone().unwrap_or_else(|| job.id.clone());

        // Bookkeeping: status, run history, next fire, repeat cap.
        let now = Utc::now();
        let (status, detail, text) = match outcome {
            JobOutcome::Delivered { text, detail, .. } => ("ok", detail, Some(text)),
            JobOutcome::Silent => (
                "silent",
                "script stdout empty — no delivery".to_string(),
                None,
            ),
            // A wakeup parked on a busy session's queue: the slot IS
            // consumed (the fire happened and did something), but no
            // reply exists yet — the target session's drainer produces
            // it under the target's own delivery. Not an error, not a
            // delivery.
            JobOutcome::Queued { detail } => ("queued", detail, None),
            JobOutcome::Failed(e) => {
                // A failed job still alerts through the delivery route —
                // silent failure is exactly what watchdog jobs must not do.
                let alert = format!("[veles cron] job '{name}' failed: {e}");
                let detail = self.deliver(job, &alert, "", "");
                ("error", format!("{e} | {detail}"), None)
            }
        };

        // Next fire + repeat cap. Next is computed forward from the slot
        // we just consumed (probe job: stored next_run_at cleared, anchor
        // = slot time), so an `every:` job never refires its own slot.
        let runs_done = job.run_count + 1;
        let capped = job.repeat_max.is_some_and(|max| runs_done >= max);
        let once = job.schedule.starts_with("once:");
        let (next, disable) = if once || capped {
            (None, true)
        } else {
            let fired_at = chrono::NaiveDateTime::parse_from_str(slot, "%Y%m%dT%H%M%SZ")
                .ok()
                .map(|n| Utc.from_utc_datetime(&n))
                .unwrap_or(now);
            let mut probe = job.clone();
            probe.next_run_at = None;
            probe.last_run_at = Some(fired_at.format("%Y-%m-%d %H:%M:%S").to_string());
            match next_fire(&probe.schedule, &probe, now) {
                Ok(n) => (n.map(|t| t.to_rfc3339()), false),
                Err(e) => {
                    tracing::error!("cron job '{name}': cannot compute next fire: {e}");
                    (None, true)
                }
            }
        };
        {
            let store = self.store.lock_ok();
            let _ = store.cron_record_run(
                &job.id,
                status,
                job.session.as_deref(),
                Some(&detail),
                next.as_deref(),
            );
            if disable {
                let _ = store.cron_set_enabled(&job.id, false);
            }
        }
        if disable {
            let why = if once {
                "one-shot consumed"
            } else {
                "repeat cap reached"
            };
            tracing::info!("cron job '{name}' disabled ({why})");
        }
        let summary = if let Some(t) = &text {
            format!("status={status}, delivered {} bytes", t.len())
        } else {
            format!("status={status}")
        };
        summary
    }

    /// no_agent job: run the script, deliver stdout verbatim.
    async fn run_script_job(self: &Arc<Self>, job: &CronJob) -> JobOutcome {
        let Some(script) = job.script.as_deref() else {
            return JobOutcome::Failed(anyhow!("no_agent job has no script path"));
        };
        let home = crate::config::veles_home();
        let path = if script.starts_with('/') {
            std::path::PathBuf::from(script)
        } else {
            home.join("scripts").join(script)
        };
        if !path.is_file() {
            return JobOutcome::Failed(anyhow!("script not found: {}", path.display()));
        }
        let (prog, args): (String, Vec<String>) = match path.extension().and_then(|e| e.to_str()) {
            Some("sh") | Some("bash") => {
                if job.sandbox {
                    // The safe-interp sandbox is a Tcl-layer gate; a bash
                    // script has no such thing. Refusing is the only
                    // honest answer for a job that asked for one.
                    return JobOutcome::Failed(anyhow!(
                        "sandbox:true is for .tcl scripts only — bash cannot be \
                         interp-gated; rewrite {script} or drop the flag"
                    ));
                }
                ("bash".to_string(), vec![path.display().to_string()])
            }
            Some("tcl") => {
                if job.sandbox {
                    return self.run_script_sandboxed(job, &path).await;
                }
                (
                    crate::executecode::tclsh_path(),
                    vec![path.display().to_string()],
                )
            }
            Some("py") => {
                return JobOutcome::Failed(anyhow!(
                    "python scripts are not supported (user policy: no python) — \
                     rewrite {} as .sh or .tcl",
                    path.display()
                ));
            }
            // Anything else gets a shell — matches the old default intent
            // without dragging in an interpreter nobody wants.
            _ => (
                crate::shell::shell_binary().to_string(),
                vec![path.display().to_string()],
            ),
        };
        let fut = tokio::process::Command::new(&prog).args(&args).output();
        let out =
            match tokio::time::timeout(Duration::from_secs(SCRIPT_JOB_TIMEOUT_SECS), fut).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return JobOutcome::Failed(anyhow!("script spawn failed: {e}")),
                Err(_) => {
                    return JobOutcome::Failed(anyhow!(
                        "script timed out after {SCRIPT_JOB_TIMEOUT_SECS}s"
                    ))
                }
            };
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: String = stderr
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            return JobOutcome::Failed(anyhow!(
                "exit code {:?}: {}",
                out.status.code(),
                tail.trim()
            ));
        }
        if stdout.is_empty() {
            return JobOutcome::Silent;
        }
        let detail = self.deliver(job, &stdout, "", "");
        JobOutcome::Delivered {
            text: stdout,
            detail,
        }
    }

    /// Sandboxed no_agent run (.tcl with sandbox:true): safe interp,
    /// scratch workspace, memory cap — the slopdrop-style execution
    /// half. Same delivery semantics as the plain path: stdout verbatim,
    /// EMPTY stdout = silent, failure = error alert.
    async fn run_script_sandboxed(
        self: &Arc<Self>,
        job: &CronJob,
        path: &std::path::Path,
    ) -> JobOutcome {
        let timeout = crate::sandbox::SANDBOX_DEFAULT_TIMEOUT_SECS.min(SCRIPT_JOB_TIMEOUT_SECS);
        let out = match crate::sandbox::execute(path, timeout).await {
            Ok(o) => o,
            Err(e) => return JobOutcome::Failed(anyhow!("sandbox: {e}")),
        };
        if out.timed_out {
            return JobOutcome::Failed(anyhow!("sandboxed script timed out after {timeout}s"));
        }
        let stdout = out.stdout.trim().to_string();
        if !out.success() {
            let stderr = out.stderr.trim().to_string();
            let tail: String = stderr.chars().rev().take(500).collect::<String>();
            let tail: String = tail.chars().rev().collect();
            return JobOutcome::Failed(anyhow!("sandbox error (exit {}): {}", out.exit_code, tail));
        }
        if stdout.is_empty() {
            return JobOutcome::Silent;
        }
        let detail = self.deliver(job, &stdout, "", "");
        JobOutcome::Delivered {
            text: stdout,
            detail,
        }
    }

    /// LLM job, fresh session: mints `cron_<job>_<stamp>`, no history,
    /// optional skills + model overrides, one turn. The hermes-parity
    /// default — every job prompt must be self-contained because the
    /// session starts empty.
    async fn run_agent_job(self: &Arc<Self>, job: &CronJob) -> JobOutcome {
        let name = job.name.clone().unwrap_or_else(|| job.id.clone());
        let sid = format!("cron_{}_{}", job.id, Utc::now().format("%Y%m%d_%H%M%S"));
        {
            let store = self.store.lock_ok();
            if let Err(e) = store.create_session(&sid) {
                return JobOutcome::Failed(anyhow!("session create failed: {e}"));
            }
            let _ = store.set_title(&sid, &format!("cron:{name}"));
        }
        // Base registry only — no cronjob tool → jobs can't schedule jobs.
        let registry = Arc::new(tools::base_registry(self.store.clone(), &sid));
        let agent = match Agent::new(
            self.cfg.clone(),
            self.hooks.clone(),
            registry,
            self.store.clone(),
            sid.clone(),
        )
        .map(|a| a.with_surface("cron"))
        {
            Ok(a) => {
                if let Some(p) = &job.provider {
                    let _ = a.set_model(&format!("custom:{p}"));
                }
                if let Some(m) = &job.model {
                    let _ = a.set_model(m);
                }
                if let Some(r) = &job.reasoning {
                    a.overrides.write_ok().reasoning = Some(r.clone());
                }
                a
            }
            Err(e) => return JobOutcome::Failed(anyhow!("agent boot failed: {e}")),
        };

        let mut prompt = skills_block(job.skills.as_deref());
        prompt.push_str(&job.prompt);

        let mut history = Vec::new();
        self.turn_and_deliver(job, &agent, &prompt, &mut history, None)
            .await
    }

    /// Session-wakeup job (Stage 5, USER taxonomy 2026-09-17: crons
    /// are "either standalone scheduled scripts OR wakeups for
    /// specific session(s)"): run the prompt as a turn IN a named
    /// session, with that session's whole history as context.
    ///
    /// Concurrency, the crux of this path — serialized THREE ways,
    /// outermost first (see the `SessionRunner` seam's comment for the
    /// full argument):
    ///
    ///   1. If a live surface OWNS the session (gateway chat, desktop
    ///      session), its own turn lock is taken across the wakeup
    ///      turn — an inbound message either ran before us (store says
    ///      busy → park) or waits. Owned-but-busy → park; the running
    ///      turn's boundary drain runs the row. After the turn,
    ///      `kick_queue` hands anything parked during it back to the
    ///      owner's pipeline — the lock does not cover that residual.
    ///   2. The store's cross-process `runs_live` truth: busy under
    ///      ANY surface (including a TUI in another process) → park,
    ///      never race. Checked again under the wake claim.
    ///   3. The process-wide wake claim: serializes against the
    ///      completion-wakers, so a background event and a cron fire
    ///      cannot both start a turn in one session.
    ///
    /// Only when no surface owns the session and the store says idle
    /// does the scheduler run the turn itself — loading the history
    /// (that is the feature) and delivering via `job.deliver`.
    ///
    /// Privileges: owner-class, exactly like every other cron job
    /// (USER decision, Stage 5 c2: scheduler privileges regardless of
    /// whose session this is — the fresh-session job always ran this
    /// way, and job creation is gated behind Cap::Cron = Admin+).
    /// The turn runs with NO principal, so the tool gate is the
    /// pre-ACL full-access owner surface.
    ///
    /// Model overrides apply TRANSIENTLY (`set_model_transient`): a
    /// daily job pinning its own model must not rewrite the model the
    /// user chose in their session.
    async fn run_session_wakeup_job(self: &Arc<Self>, job: &CronJob, sid: &str) -> JobOutcome {
        {
            let store = self.store.lock_ok();
            match store.session_exists(sid) {
                Ok(true) => {}
                Ok(false) => {
                    return JobOutcome::Failed(anyhow!(
                        "session '{sid}' does not exist (deleted since the job was created?)"
                    ))
                }
                Err(e) => return JobOutcome::Failed(anyhow!("session lookup failed: {e}")),
            }
            // Busy → park, do not race. runs_live reaps stale rows on
            // the way (a crashed surface leaves a row behind; 30s of
            // heartbeat silence is the difference between "running"
            // and "was running") — the same reader every wake path
            // uses, so the answer converges without a sweeper.
            let busy = store
                .runs_live(30)
                .unwrap_or_default()
                .iter()
                .any(|r| r.session_id == sid);
            if busy {
                let mut prompt = skills_block(job.skills.as_deref());
                prompt.push_str(&job.prompt);
                match store.queue_push(sid, &prompt) {
                    Ok(seq) => {
                        return JobOutcome::Queued {
                            detail: format!(
                                "session busy — prompt parked as queue item #{seq}, runs at the turn boundary"
                            ),
                        }
                    }
                    Err(e) => return JobOutcome::Failed(anyhow!("queue push failed: {e}")),
                }
            }
        }
        // Idle. The claim is taken BEFORE the turn and dropped after:
        // a background completion waking this same session meanwhile
        // must see the claim and stand down (and vice versa — if a
        // completion-waker already holds it, THIS firing parks its
        // prompt on the queue rather than starting a second turn).
        let _claim = match crate::delegate::try_claim_wake(sid) {
            Some(c) => c,
            None => {
                let mut prompt = skills_block(job.skills.as_deref());
                prompt.push_str(&job.prompt);
                let store = self.store.lock_ok();
                match store.queue_push(sid, &prompt) {
                    Ok(seq) => {
                        return JobOutcome::Queued {
                            detail: format!(
                                "session is being woken by a completion — prompt parked as queue item #{seq}"
                            ),
                        }
                    }
                    Err(e) => return JobOutcome::Failed(anyhow!("queue push failed: {e}")),
                }
            }
        };
        // Re-check liveness UNDER the claim: the window between the
        // busy check above and the claim is where a user message could
        // have started a turn; runs_live is cheap and the race is
        // real. (The claim itself does not stop an inbound user turn —
        // it serializes WAKERS, which is all two schedulers need.)
        {
            let store = self.store.lock_ok();
            let busy = store
                .runs_live(30)
                .unwrap_or_default()
                .iter()
                .any(|r| r.session_id == sid);
            if busy {
                let mut prompt = skills_block(job.skills.as_deref());
                prompt.push_str(&job.prompt);
                match store.queue_push(sid, &prompt) {
                    Ok(seq) => {
                        return JobOutcome::Queued {
                            detail: format!(
                                "session turned busy — prompt parked as queue item #{seq}"
                            ),
                        }
                    }
                    Err(e) => return JobOutcome::Failed(anyhow!("queue push failed: {e}")),
                }
            }
        }
        // The OWNER SURFACE's turn lock, when a live surface owns this
        // session. This is the seam's whole point: the gateway/desktop
        // hold their own lock across every turn THEY run, so holding
        // it here too means an inbound message either already ran
        // (runs_live above would have shown busy → parked) or blocks
        // until this turn finishes. Owned-but-busy parks for the
        // running turn's boundary drain. Unowned (REPL/TUI/CLI,
        // standalone) → no in-memory lock exists; the runs_live checks
        // are the only truth, which is exactly what a headless run
        // has.
        let (owned, guard) = try_lock_session_turn(sid);
        if owned && guard.is_none() {
            let mut prompt = skills_block(job.skills.as_deref());
            prompt.push_str(&job.prompt);
            let store = self.store.lock_ok();
            return match store.queue_push(sid, &prompt) {
                Ok(seq) => JobOutcome::Queued {
                    detail: format!(
                        "session's surface is mid-turn — prompt parked as queue item #{seq}"
                    ),
                },
                Err(e) => return JobOutcome::Failed(anyhow!("queue push failed: {e}")),
            };
        }
        // Held across the turn below; dropped when this fn returns.
        let _session_guard = guard;

        // Base registry: the recursion guard applies to wakeup jobs
        // too — a job cannot schedule more jobs, whichever session it
        // runs in. (The session's OWN registry — if its surface
        // rebuilds an agent later — is untouched; this one is the
        // scheduler's, for this turn only.)
        let registry = Arc::new(tools::base_registry(self.store.clone(), sid));
        let agent = match Agent::new(
            self.cfg.clone(),
            self.hooks.clone(),
            registry,
            self.store.clone(),
            sid.to_string(),
        )
        .map(|a| a.with_surface("cron"))
        {
            Ok(a) => {
                if let Some(p) = &job.provider {
                    let _ = a.set_model_transient(&format!("custom:{p}"));
                }
                if let Some(m) = &job.model {
                    let _ = a.set_model_transient(m);
                }
                if let Some(r) = &job.reasoning {
                    a.overrides.write_ok().reasoning = Some(r.clone());
                }
                a
            }
            Err(e) => return JobOutcome::Failed(anyhow!("agent boot failed: {e}")),
        };

        let mut prompt = skills_block(job.skills.as_deref());
        prompt.push_str(&job.prompt);

        // The whole history is the context — that is the feature.
        let mut history = {
            let store = self.store.lock_ok();
            store.load_history(sid).unwrap_or_default()
        };
        let outcome = self
            .turn_and_deliver(job, &agent, &prompt, &mut history, Some(sid))
            .await;
        // The turn is over: release the owner's lock BEFORE kicking,
        // because both kick implementations need it free. The gateway
        // re-injects a parked row as an inbound message whose handler
        // `try_lock`s this same chat lock — kick while we hold it and
        // the row re-parks behind us, stranded until the next inbound,
        // which is the exact failure the kick exists to prevent. The
        // desktop's drain task `.lock().await`s it outright.
        drop(_session_guard);
        // A foreign turn just ended in the owner's session: anything
        // that parked during it is the owner's to run, and its own
        // turn boundary already passed (that boundary was ours). Hand
        // the rows back so a user told "queued, runs next" does not
        // wait for the next inbound. No-op for unowned sessions.
        if owned {
            kick_session_queue(sid);
        }
        outcome
    }

    /// One timed turn + delivery, shared by the fresh-session and
    /// session-wakeup paths so the two cannot drift on the timeout,
    /// the empty-reply rule or the delivery route. `session` is the
    /// resumed id for run-history bookkeeping (None = fresh job;
    /// `finish_run` records it either way via the caller's outcome).
    async fn turn_and_deliver(
        self: &Arc<Self>,
        job: &CronJob,
        agent: &Agent,
        prompt: &str,
        history: &mut Vec<crate::providers::ChatMessage>,
        session: Option<&str>,
    ) -> JobOutcome {
        let turn = tokio::time::timeout(
            Duration::from_secs(AGENT_JOB_TIMEOUT_SECS),
            agent.run_turn(history, prompt, None, |_ev| {}),
        )
        .await;
        let tr = match turn {
            Ok(Ok(tr)) => tr,
            Ok(Err(e)) => return JobOutcome::Failed(anyhow!("turn failed: {e:#}")),
            Err(_) => {
                return JobOutcome::Failed(anyhow!(
                    "agent job timed out after {AGENT_JOB_TIMEOUT_SECS}s"
                ))
            }
        };
        if tr.reply.trim().is_empty() {
            return JobOutcome::Failed(anyhow!("agent produced an empty reply"));
        }
        let mut detail = self.deliver(job, &tr.reply, &tr.provider, &tr.model);
        if let Some(sid) = session {
            detail = format!("in session {sid} | {detail}");
        }
        JobOutcome::Delivered {
            text: tr.reply,
            detail,
        }
    }

    /// Route a finished run to its delivery target. Returns a human-readable
    /// detail for the run history. Never panics, never drops silently:
    /// when no adapter listens, the text goes to stdout instead.
    fn deliver(&self, job: &CronJob, text: &str, provider: &str, model: &str) -> String {
        let route = job.deliver.as_str();
        if route == "local" {
            println!("[veles cron] local-only job output:\n{text}");
            return "local (not delivered)".to_string();
        }
        // 'origin' and 'all' resolve to the captured origin chat ('all'
        // fans out once the gateway has multi-platform homes; today there
        // is one adapter, so they coincide — said honestly, not silently).
        // `home`: the job's platform's home chat — the ops chat (USER
        // 2026-10-21, veles_core::home). Without one for that platform the
        // output goes where the job was made, and the detail says so.
        let mut note = String::new();
        let target: Option<String> = if route == "origin" || route == "all" {
            job.origin.clone()
        } else if route == "home" {
            let platform = job
                .origin
                .as_deref()
                .and_then(|o| o.split_once(':'))
                .map(|(p, _)| p);
            match platform.and_then(crate::home::key) {
                Some(key) => Some(key),
                None => {
                    note = " (no home chat for this platform; the origin instead)".to_string();
                    job.origin.clone()
                }
            }
        } else {
            Some(route.to_string())
        };
        let Some(target) = target else {
            println!("[veles cron] no delivery target recorded; output:\n{text}");
            return "no target (logged to stdout)".to_string();
        };
        let Some((platform, chat_id)) = target.split_once(':') else {
            println!("[veles cron] malformed delivery target '{target}'; output:\n{text}");
            return format!("malformed target '{target}' (logged to stdout)");
        };
        match &self.bus {
            Some(bus) => {
                let sent = bus.send_outbound(
                    platform,
                    OutboundMessage {
                        chat_id: chat_id.to_string(),
                        reply_to: None,
                        text: text.to_string(),
                        reasoning: None,
                        provider: provider.to_string(),
                        model: model.to_string(),
                        kind: MessageKind::Reply,
                        ask: None,
                        meta: Default::default(),
                    },
                );
                if sent {
                    format!("delivered to {target}{note}")
                } else {
                    println!("[veles cron] no adapter for '{platform}'; output:\n{text}");
                    format!("adapter '{platform}' offline (logged to stdout)")
                }
            }
            None => {
                println!("[veles cron] standalone run, output for {target}:\n{text}");
                "standalone (logged to stdout)".to_string()
            }
        }
    }
}

enum JobOutcome {
    Delivered {
        text: String,
        detail: String,
    },
    Silent,
    /// A session-wakeup that found its target busy: the prompt parked
    /// on the session queue for the running turn's drainer. The fire
    /// did real work (nothing is lost), but produced no reply of its
    /// own — a distinct status in the run history, not "ok" and not
    /// "error".
    Queued {
        detail: String,
    },
    Failed(anyhow::Error),
}

impl std::fmt::Debug for JobOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The variants carry Strings/anyhow, but a hand-rolled Debug
        // keeps the shape one-line and stable for test panics.
        match self {
            Self::Delivered { text, detail } => {
                write!(f, "Delivered({} bytes, {detail})", text.len())
            }
            Self::Silent => write!(f, "Silent"),
            Self::Queued { detail } => write!(f, "Queued({detail})"),
            Self::Failed(e) => write!(f, "Failed({e:#})"),
        }
    }
}

// ── schedule parsing & computation ────────────────────────────────────

/// Parse user-facing schedule syntax into the normalized stored form.
///
/// Accepted (hermes cronjob parity):
///   '30m' | '90s' | '2h' | '1d'            → every:<seconds>
///   'every 2h' | 'every 30 minutes'        → every:<seconds>
///   '0 9 * * *' (5 fields, 6 with seconds) → cron:<spec>
///   '2026-06-01T09:00:00' (ISO, RFC3339)   → once:<rfc3339-utc>
pub fn normalize_schedule(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty schedule");
    }
    // Interval with optional 'every' prefix.
    let body = s.strip_prefix("every ").unwrap_or(s);
    if let Some(secs) = parse_interval(body) {
        anyhow::ensure!(
            secs >= 60,
            "interval must be at least 60 seconds (got {secs}s)"
        );
        return Ok(format!("every:{secs}"));
    }
    // Cron pattern: 5 fields (min hour dom mon dow) or 6 (seconds first).
    let fields = s.split_whitespace().count();
    if fields == 5 || fields == 6 {
        Cron::new(s)
            .parse()
            .map_err(|e| anyhow!("invalid cron pattern '{s}': {e}"))?;
        return Ok(format!("cron:{s}"));
    }
    // One-shot timestamp.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        let utc = dt.with_timezone(&Utc);
        anyhow::ensure!(utc > Utc::now(), "one-shot timestamp is in the past");
        return Ok(format!("once:{}", utc.to_rfc3339()));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        let local = chrono::Local
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| anyhow!("ambiguous local time '{s}'"))?;
        let utc = local.with_timezone(&Utc);
        anyhow::ensure!(utc > Utc::now(), "one-shot timestamp is in the past");
        return Ok(format!("once:{}", utc.to_rfc3339()));
    }
    bail!(
        "unrecognized schedule '{s}' — use '30m'/'every 2h', a cron pattern \
         like '0 9 * * *', or an ISO timestamp"
    );
}

fn parse_interval(s: &str) -> Option<u64> {
    let s = s.trim();
    let cut = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = (&s[..cut], s[cut..].trim());
    let n: u64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    let mult: u64 = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    };
    Some(n * mult)
}

/// Parse the sqlite `datetime('now')` UTC shape used by created_at/last_run_at.
fn parse_sqlite_utc(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|n| Utc.from_utc_datetime(&n))
}

/// The earliest time an enabled job runs next, past or future (a job
/// whose time has come and gone is due now). A phone's gateway that
/// stops when idle hands this to the service, which starts it again
/// then (`device_events::write_wake`): nothing else can wake a stopped
/// engine for its cron.
pub fn next_wake(store: &Store) -> Option<DateTime<Utc>> {
    let now = Utc::now();
    store
        .cron_list(true)
        .ok()?
        .iter()
        .filter_map(|j| next_fire(&j.schedule, j, now).ok().flatten())
        .min()
}

/// Who fires this gateway's cron jobs (B3a, and the phone's rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronDuty {
    /// This gateway fires its own jobs, ungated: a single node, or a pool
    /// member that stops when idle. Cron state is node-local — nothing
    /// else ever fires a job made here — and a member that stops when
    /// idle never asks for the pool's singleton lease (a holder that stops
    /// five minutes after its last use is no holder), so it doubles
    /// nobody. Until 2026-10-21 such a member stood by for nothing: its
    /// jobs were dead letters, and the alarm its service arms for the
    /// next one (`device_events::write_wake`) was handed no time.
    Own,
    /// A pool member that stays up: a standby of the pool's cron, firing
    /// only while it holds the singleton lease.
    Leased,
}

/// `in_pool`: an arbiter is set or the pool is declared;
/// `stops_when_idle`: `--exit-when-idle` (the phone's gateway).
pub fn cron_duty(in_pool: bool, stops_when_idle: bool) -> CronDuty {
    if !in_pool || stops_when_idle {
        CronDuty::Own
    } else {
        CronDuty::Leased
    }
}

/// Next fire time for a normalized schedule, relative to `now`.
/// None = the job has no future fire (consumed one-shot).
pub fn next_fire(
    schedule: &str,
    job: &CronJob,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    if let Some(ts) = schedule.strip_prefix("once:") {
        let dt = DateTime::parse_from_rfc3339(ts)
            .map_err(|e| anyhow!("bad once timestamp '{ts}': {e}"))?
            .with_timezone(&Utc);
        return Ok(Some(dt));
    }
    if let Some(secs) = schedule.strip_prefix("every:") {
        let secs: u64 = secs.parse().map_err(|_| anyhow!("bad interval '{secs}'"))?;
        // Durable anchor: the stored next_run_at survives restarts.
        if let Some(stored) = &job.next_run_at {
            if let Ok(dt) = DateTime::parse_from_rfc3339(stored) {
                return Ok(Some(dt.with_timezone(&Utc)));
            }
        }
        let anchor = job
            .last_run_at
            .as_deref()
            .and_then(parse_sqlite_utc)
            .or_else(|| parse_sqlite_utc(&job.created_at))
            .unwrap_or(now);
        return Ok(Some(anchor + chrono::Duration::seconds(secs as i64)));
    }
    if let Some(spec) = schedule.strip_prefix("cron:") {
        let cron = Cron::new(spec)
            .parse()
            .map_err(|e| anyhow!("bad cron pattern '{spec}': {e}"))?;
        // Anchor: last actual run, else creation. First-ever fire is the
        // first match AFTER creation — a brand-new '0 9 * * *' job created
        // at 10:00 fires tomorrow 09:00, never retroactively.
        let anchor = job
            .last_run_at
            .as_deref()
            .and_then(parse_sqlite_utc)
            .or_else(|| parse_sqlite_utc(&job.created_at))
            .unwrap_or(now);
        let anchor = if anchor > now { now } else { anchor };
        return Ok(cron
            .find_next_occurrence(&anchor, false)
            .ok()
            .map(|d| d.with_timezone(&Utc)));
    }
    bail!("unknown schedule form: {schedule}");
}

/// Decide whether the built-in dream schedule is due, reusing the same
/// normalized-schedule + `due_slot` math as every cron job. `last_run`
/// is the stored anchor in sqlite-UTC text; `None` means never run,
/// which anchors on `now` so the first reflection comes one full
/// interval out rather than the instant the knob is set.
fn dream_slot(
    schedule_raw: &str,
    last_run: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    let schedule = normalize_schedule(schedule_raw)?;
    let anchor = last_run
        .map(|s| s.to_string())
        .unwrap_or_else(|| now.format("%Y-%m-%d %H:%M:%S").to_string());
    let job = CronJob {
        id: "builtin-dream".into(),
        name: Some("veles-dream".into()),
        schedule,
        last_run_at: last_run.map(|s| s.to_string()),
        created_at: anchor,
        ..Default::default()
    };
    due_slot(&job, now)
}

/// When a job is due, returns the slot identity (used as the exactly-once
/// claim key). The slot is the scheduled instant itself, so a catch-up run
/// and a double-tick claim the SAME key and only one executes.
fn due_slot(job: &CronJob, now: DateTime<Utc>) -> Result<Option<String>> {
    if job.schedule.starts_with("once:") {
        // One-shots are claimed by a constant slot: the ledger entry
        // outlives any re-enable shenanigans (30-day TTL).
        if let Some(dt) = next_fire(&job.schedule, job, now)? {
            if now >= dt {
                return Ok(Some("once".to_string()));
            }
        }
        return Ok(None);
    }
    let Some(next) = next_fire(&job.schedule, job, now)? else {
        return Ok(None);
    };
    if now >= next {
        // Minute-granular slot identity (second-level jitter in 'now'
        // must not fork the key).
        let floored = next
            .with_second(0)
            .unwrap_or(next)
            .format("%Y%m%dT%H%M%SZ")
            .to_string();
        Ok(Some(floored))
    } else {
        Ok(None)
    }
}

/// Inline requested skill bodies into the job prompt (hermes loads the
/// skills, then runs the prompt as the task instruction).
fn skills_block(skills_json: Option<&str>) -> String {
    let Some(raw) = skills_json else {
        return String::new();
    };
    let Ok(names) = serde_json::from_str::<Vec<String>>(raw) else {
        return String::new();
    };
    if names.is_empty() {
        return String::new();
    }
    let set = crate::skills::SkillSet::load();
    let mut out = String::new();
    for name in &names {
        match set.get(name) {
            Some(meta) => {
                let body = std::fs::read_to_string(meta.dir.join("SKILL.md")).unwrap_or_default();
                out.push_str(&format!("═══ SKILL: {name} ═══\n{body}\n\n"));
            }
            None => out.push_str(&format!("(skill '{name}' not found at load time)\n\n")),
        }
    }
    if !out.is_empty() {
        out.push_str("═══ TASK ═══\n");
    }
    out
}

/// Human-readable preview for list output.
/// The cron listing, for every surface that shows one.
///
/// WHY THIS SAYS WHETHER A GATEWAY IS RUNNING
///
/// `Scheduler::run_loop` — the only thing in the process that fires a
/// job — is spawned in exactly ONE place, `frontends/gateway.rs`. The
/// CLI builds a Scheduler solely for `veles cron run <id>` (a manual
/// one-shot) and the desktop API receives one only from the gateway that
/// serves it. `run_gui` starts a gateway if none is up, so the desktop is
/// covered; a bare REPL or TUI is not.
///
/// So "next run: 09:00" is a promise this process may have no way to
/// keep, and a listing that prints it without saying so is the same
/// class of thing as a cancel that only flips a state string. The
/// dependency goes in the output, where someone will see it, rather than
/// in a comment, where it sat unnoticed until the 2026-09-03 audit.
pub fn listing(store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>) -> String {
    let jobs = match store.lock() {
        Ok(s) => s.cron_list(false).unwrap_or_default(),
        Err(_) => return "cron: store unavailable".to_string(),
    };
    let mut out = String::new();
    match crate::lifecycle::read_pid() {
        Some(pid) if crate::lifecycle::is_running(pid) => {
            out.push_str(&format!("scheduler: running (gateway pid {pid})\n"));
        }
        Some(pid) => out.push_str(&format!(
            "scheduler: NOT running (stale gateway pidfile {pid}) — \
             nothing below will fire until a gateway is up\n"
        )),
        None => out.push_str(
            "scheduler: NOT running — jobs fire only while a gateway is up \
             (`veles gateway run`, or `veles gui`, which starts one)\n",
        ),
    }
    if jobs.is_empty() {
        out.push_str("no cron jobs (/crons add \"<schedule>\" <prompt>; /cron works too)");
        return out;
    }
    for j in &jobs {
        out.push_str(&format!(
            "{} {:<14} {:<24} runs={} next={}\n",
            if j.enabled { "✓" } else { "·" },
            j.id,
            describe_schedule(&j.schedule),
            j.run_count,
            j.next_run_at.as_deref().unwrap_or("-"),
        ));
        if let Some(name) = j.name.as_deref().filter(|n| !n.is_empty()) {
            out.push_str(&format!("    name: {name}\n"));
        }
        // Stage 5: a wakeup job names its target — the difference
        // between "runs somewhere anonymous" and "resumes MY session"
        // must be visible in the listing, not only in the run history.
        if let Some(sid) = j.session.as_deref() {
            out.push_str(&format!("    wakes session: {sid}\n"));
        }
        if let Some(s) = &j.last_status {
            out.push_str(&format!(
                "    last: {s} @ {}\n",
                j.last_run_at.as_deref().unwrap_or("?")
            ));
        }
    }
    out.trim_end().to_string()
}

pub fn describe_schedule(schedule: &str) -> String {
    if let Some(ts) = schedule.strip_prefix("once:") {
        return format!("once at {ts}");
    }
    if let Some(secs) = schedule.strip_prefix("every:") {
        let secs: u64 = secs.parse().unwrap_or(0);
        let (n, unit) = if secs.is_multiple_of(86_400) {
            (secs / 86_400, "day")
        } else if secs.is_multiple_of(3_600) {
            (secs / 3_600, "hour")
        } else if secs.is_multiple_of(60) {
            (secs / 60, "min")
        } else {
            (secs, "sec")
        };
        return format!("every {n} {unit}{}", if n == 1 { "" } else { "s" });
    }
    if let Some(spec) = schedule.strip_prefix("cron:") {
        return format!("cron '{spec}'");
    }
    schedule.to_string()
}

/// Resolve a user-typed session reference (full id or unique prefix,
/// the /resume spelling) to a real session id — at CREATE time, so a
/// typo is refused while a human is looking, not at fire time.
/// Refuses honestly: unknown, or a prefix matching more than one
/// session (ambiguity resolved by picking one would be a guess about
/// WHOSE conversation a scheduled prompt enters).
pub fn resolve_session(store: &Store, raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("session reference is empty");
    }
    if store.session_exists(raw)? {
        return Ok(raw.to_string());
    }
    // Every session with the prefix (two are enough to call it
    // ambiguous) — not just those among the newest 1,000.
    let matches: Vec<String> = store.sessions_with_prefix(raw, 2)?;
    match matches.len() {
        1 => {
            let mut m = matches;
            Ok(m.remove(0))
        }
        0 => bail!("no session matching '{raw}'"),
        _ => bail!(
            "'{raw}' matches {} sessions ({}…) — use the full id",
            matches.len(),
            matches[0]
        ),
    }
}

/// Create a LOCAL-delivery job from a text surface — the shared body
/// of the in-chat `/crons add "<schedule>" <prompt>` and the CLI's
/// `veles cron add`. Normalizes the schedule, inserts the job, and
/// computes the first fire. `session` (Stage 5) names a session to
/// RESUME as a wakeup instead of minting a fresh one; it is resolved
/// here so a typo is refused at creation. Returns the human summary.
pub fn add_local_job(
    store: &crate::store::Store,
    raw: &str,
    prompt: &str,
    session: Option<&str>,
) -> Result<String> {
    let schedule = normalize_schedule(raw)?;
    let resolved = match session.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(resolve_session(store, s)?),
        None => None,
    };
    let job = crate::store::CronJob {
        id: crate::cronjob::short_id(),
        schedule,
        prompt: prompt.to_string(),
        // Local: a job created from a text surface has no platform chat
        // to deliver into, and claiming otherwise is how you get output
        // routed at nothing. The cronjob TOOL still offers the rest. A
        // wakeup job's reply is persisted into the resumed session's
        // transcript regardless — `deliver` is the EXTRA copy, and a
        // text-surface job has nowhere extra to send it.
        deliver: "local".into(),
        enabled: true,
        session: resolved,
        ..Default::default()
    };
    store.cron_upsert(&job)?;
    let next = next_fire(&job.schedule, &job, chrono::Utc::now())
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());
    let _ = store.cron_set_next(&job.id, next.as_deref());
    let target = match &job.session {
        Some(sid) => format!(" · wakes session {sid}"),
        None => String::new(),
    };
    Ok(format!(
        "job {} created · {}{target} · next {}",
        job.id,
        describe_schedule(&job.schedule),
        next.as_deref().unwrap_or("-")
    ))
}

#[cfg(test)]
mod tests {
    /// USER 2026-10-21 ("Same bot, dedicated ops chat"): a job whose
    /// delivery is `home` reports to its platform's home chat (the ops
    /// chat), not the chat it was made in. Without a home chat for that
    /// platform it falls back to the origin and says so.
    #[tokio::test]
    async fn a_home_route_delivers_to_the_platforms_home_chat() {
        use std::sync::Arc;
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let (tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let (otx, mut orx) = tokio::sync::mpsc::unbounded_channel();
        let bus = crate::bus::GatewayBus {
            inbound: tx,
            outbound: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            last_chat: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            state_path: dir.path().join("gateway-state.json"),
        };
        bus.register_outbound("tghome", otx);
        crate::home::set("tghome", "-100777");
        let sched = super::Scheduler::new(
            store,
            Arc::new(crate::config::VelesConfig::default()),
            crate::hooks::HookBus::new(None),
            Some(bus),
        );
        let job = crate::store::CronJob {
            id: "homejob".into(),
            prompt: "report".into(),
            deliver: "home".into(),
            origin: Some("tghome:555".into()),
            enabled: true,
            ..Default::default()
        };
        let detail = sched.deliver(&job, "the report", "p", "m");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), orx.recv())
            .await
            .expect("a home delivery reaches the bus")
            .expect("the channel is open");
        assert_eq!(msg.chat_id, "-100777", "the home chat, not the origin");
        assert_eq!(msg.text, "the report");
        assert!(detail.contains("tghome:-100777"), "{detail}");

        // No home chat on that platform: the origin, and the detail says
        // the route had nowhere else to go.
        let elsewhere = crate::store::CronJob {
            origin: Some("tgnohome:1".into()),
            ..job.clone()
        };
        let (otx2, mut orx2) = tokio::sync::mpsc::unbounded_channel();
        sched
            .bus
            .as_ref()
            .unwrap()
            .register_outbound("tgnohome", otx2);
        let detail = sched.deliver(&elsewhere, "the report", "p", "m");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), orx2.recv())
            .await
            .expect("falls back to the origin")
            .expect("open");
        assert_eq!(msg.chat_id, "1");
        assert!(detail.contains("no home chat"), "{detail}");
    }

    #[test]
    fn a_member_that_stops_when_idle_fires_its_own_cron() {
        use super::{cron_duty, CronDuty};
        // A single node: ungated.
        assert_eq!(cron_duty(false, false), CronDuty::Own);
        // A pool member that stays up stands by for the pool's lease.
        assert_eq!(cron_duty(true, false), CronDuty::Leased);
        // The phone: nothing else ever fires its node-local jobs, and it
        // never asks for the lease — it fires its own, and its idle exit
        // hands the service the next job's time.
        assert_eq!(cron_duty(true, true), CronDuty::Own);
    }

    use super::*;

    #[test]
    fn interval_normalization() {
        assert_eq!(normalize_schedule("30m").unwrap(), "every:1800");
        assert_eq!(normalize_schedule("every 2h").unwrap(), "every:7200");
        assert_eq!(normalize_schedule("every 90 seconds").unwrap(), "every:90");
        assert_eq!(normalize_schedule("1d").unwrap(), "every:86400");
        assert!(
            normalize_schedule("30s").is_err(),
            "sub-minute interval rejected"
        );
        assert!(normalize_schedule("banana").is_err());
    }

    #[test]
    fn cron_pattern_normalization() {
        assert_eq!(normalize_schedule("0 9 * * *").unwrap(), "cron:0 9 * * *");
        assert_eq!(
            normalize_schedule("*/5 * * * *").unwrap(),
            "cron:*/5 * * * *"
        );
        assert!(normalize_schedule("99 * * * *").is_err());
    }

    #[test]
    fn once_normalization() {
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(normalize_schedule(&future).unwrap().starts_with("once:"));
        assert!(
            normalize_schedule("2020-01-01T00:00:00Z").is_err(),
            "past ts rejected"
        );
    }

    fn job(schedule: &str, created: &str, last: Option<&str>, next: Option<&str>) -> CronJob {
        CronJob {
            id: "j".into(),
            schedule: schedule.into(),
            prompt: "p".into(),
            deliver: "local".into(),
            enabled: true,
            created_at: created.into(),
            updated_at: created.into(),
            last_run_at: last.map(String::from),
            next_run_at: next.map(String::from),
            ..Default::default()
        }
    }

    /// What a phone's stopped engine is started again for
    /// (`device_events::write_wake`): the earliest ENABLED job.
    #[test]
    fn next_wake_is_the_earliest_enabled_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        assert!(next_wake(&store).is_none(), "no jobs, no wake");
        let at = |d: chrono::Duration| (Utc::now() + d).with_nanosecond(0).unwrap();
        let (soon, later) = (
            at(chrono::Duration::minutes(10)),
            at(chrono::Duration::hours(5)),
        );
        let mut a = job(
            &format!("once:{}", later.to_rfc3339()),
            "2026-01-01 00:00:00",
            None,
            None,
        );
        a.id = "a".into();
        let mut b = job(
            &format!("once:{}", soon.to_rfc3339()),
            "2026-01-01 00:00:00",
            None,
            None,
        );
        b.id = "b".into();
        b.enabled = false;
        store.cron_upsert(&a).unwrap();
        store.cron_upsert(&b).unwrap();
        assert_eq!(
            next_wake(&store),
            Some(later),
            "a disabled job wakes nothing"
        );
        store.cron_set_enabled("b", true).unwrap();
        assert_eq!(next_wake(&store), Some(soon));
    }

    #[test]
    fn every_next_fire_uses_stored_anchor() {
        let now = Utc::now();
        let stored = (now + chrono::Duration::minutes(5)).to_rfc3339();
        let j = job("every:3600", "2026-01-01 00:00:00", None, Some(&stored));
        let next = next_fire(&j.schedule, &j, now).unwrap().unwrap();
        assert_eq!(
            next,
            DateTime::parse_from_rfc3339(&stored)
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    /// The built-in dream schedule reuses the cron math, and the three
    /// cases that matter are: never run (not due until one interval
    /// out), run long ago (due now), and an unusable knob (an error the
    /// scheduler logs rather than a panic or a silent no-fire).
    #[test]
    fn dream_slot_anchors_on_the_last_run() {
        let now = Utc::now();
        // Never run: anchored on now, so the first fire is one full
        // interval away — enabling the knob does not reflect instantly.
        assert!(dream_slot("60s", None, now).unwrap().is_none());
        // Last run two hours ago on an hourly schedule: due.
        let old = (now - chrono::Duration::hours(2))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(dream_slot("1h", Some(&old), now).unwrap().is_some());
        // Last run one minute ago on an hourly schedule: not due.
        let recent = (now - chrono::Duration::minutes(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(dream_slot("1h", Some(&recent), now).unwrap().is_none());
        // A cron pattern works too: '0 4 * * *' with a last run two
        // days ago is due (the next 04:00 has passed).
        let two_days = (now - chrono::Duration::days(2))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(dream_slot("0 4 * * *", Some(&two_days), now)
            .unwrap()
            .is_some());
        // Garbage is an error, not a silent never-fire.
        assert!(dream_slot("whenever", None, now).is_err());
    }

    #[test]
    fn cron_first_fire_is_after_creation_not_now() {
        // Created at 10:00 UTC; '0 9 * * *' must fire NEXT day 09:00,
        // never retroactively at today's 09:00.
        let created = "2026-08-09 10:00:00";
        let j = job("cron:0 9 * * *", created, None, None);
        let now = Utc.with_ymd_and_hms(2026, 8, 9, 10, 30, 0).unwrap();
        let next = next_fire(&j.schedule, &j, now).unwrap().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap());
        assert!(due_slot(&j, now).unwrap().is_none(), "not due yet");
        // At tomorrow 09:00 it becomes due with a minute-floored slot key.
        let at = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 5).unwrap();
        assert_eq!(due_slot(&j, at).unwrap(), Some("20260810T090000Z".into()));
    }

    #[test]
    fn catch_up_after_downtime_fires_once() {
        // Last ran Aug 1 09:00; pattern daily 09:00; now Aug 9. The first
        // missed instant (Aug 2 09:00) is due exactly once.
        let j = job(
            "cron:0 9 * * *",
            "2026-07-01 00:00:00",
            Some("2026-08-01 09:00:05"),
            None,
        );
        let now = Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap();
        let slot = due_slot(&j, now).unwrap();
        assert_eq!(slot, Some("20260802T090000Z".into()));
    }

    #[test]
    fn once_due_and_slot() {
        let past = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let j = job(&format!("once:{past}"), "2026-01-01 00:00:00", None, None);
        assert_eq!(due_slot(&j, Utc::now()).unwrap(), Some("once".into()));
        let future = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let j2 = job(&format!("once:{future}"), "2026-01-01 00:00:00", None, None);
        assert!(due_slot(&j2, Utc::now()).unwrap().is_none());
    }

    #[test]
    fn describe_schedule_shapes() {
        assert_eq!(describe_schedule("every:1800"), "every 30 mins");
        assert_eq!(describe_schedule("every:7200"), "every 2 hours");
        assert_eq!(describe_schedule("cron:0 9 * * *"), "cron '0 9 * * *'");
        assert!(describe_schedule("once:2026-06-01T09:00:00Z").starts_with("once at"));
    }

    /// The shared create path behind `/crons add` and `veles cron add`.
    #[test]
    fn add_local_job_inserts_and_schedules() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        let summary = add_local_job(&store, "30m", "reflect on the day", None).unwrap();
        assert!(summary.contains("job "), "names the job: {summary}");
        assert!(summary.contains("every 30 mins"), "{summary}");
        let jobs = store.cron_list(false).unwrap();
        assert_eq!(jobs.len(), 1);
        let j = &jobs[0];
        assert_eq!(j.schedule, "every:1800");
        assert_eq!(j.prompt, "reflect on the day");
        assert_eq!(j.deliver, "local", "text surfaces must not invent a chat");
        assert!(j.session.is_none(), "no session = fresh-session run");
        assert!(j.enabled);
        assert!(
            store
                .cron_get(&j.id)
                .unwrap()
                .unwrap()
                .next_run_at
                .is_some(),
            "first fire computed at creation"
        );
        assert!(add_local_job(&store, "banana", "x", None).is_err());
    }

    /// `resolve_session`: the /resume spelling (full id or unique
    /// prefix), resolved at CREATE time so a typo is refused by a
    /// human-present surface, not discovered at fire time.
    #[test]
    fn resolve_session_matches_full_prefix_and_refuses_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        store.create_session("20260917_alpha").unwrap();
        store.create_session("20260917_beta").unwrap();

        // Full id.
        assert_eq!(
            resolve_session(&store, "20260917_alpha").unwrap(),
            "20260917_alpha"
        );
        // Unique prefix.
        assert_eq!(
            resolve_session(&store, "20260917_b").unwrap(),
            "20260917_beta"
        );
        // A prefix matching BOTH is refused, not guessed — WHICH
        // conversation a scheduled prompt enters is not the scheduler's
        // call to make.
        let err = resolve_session(&store, "20260917").unwrap_err().to_string();
        assert!(
            err.contains("matches 2 sessions"),
            "ambiguity is named: {err}"
        );
        // Unknown.
        assert!(resolve_session(&store, "nope").is_err());
        // Empty.
        assert!(resolve_session(&store, "  ").is_err());
    }

    /// A wakeup job stores the RESOLVED session id and the summary says
    /// which session it wakes — the creation surface must confirm the
    /// target, not silently accept it.
    #[test]
    fn add_local_job_with_session_resolves_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        store.create_session("wake-target-1").unwrap();
        let summary =
            add_local_job(&store, "0 9 * * *", "morning check-in", Some("wake-target")).unwrap();
        assert!(
            summary.contains("wakes session wake-target-1"),
            "confirms the resolved target: {summary}"
        );
        let j = store.cron_list(false).unwrap().remove(0);
        assert_eq!(j.session.as_deref(), Some("wake-target-1"));
        // A bad target is refused at creation, leaving NO job behind.
        assert!(add_local_job(&store, "0 9 * * *", "x", Some("ghost")).is_err());
        assert_eq!(
            store.cron_list(false).unwrap().len(),
            1,
            "refused, not stored"
        );
    }

    /// `listing` is the text every surface shows (the shared
    /// dispatcher returns it verbatim), so its empty-state hint is a
    /// USER-VISIBLE string that names the command. `/crons` is
    /// canonical and `/cron` is the alias (2026-09-19), and the hint
    /// named the alias — the wrong way round, and nothing held it.
    /// This pins the canonical spelling in the hint and keeps the
    /// alias mentioned, because a reader who learned `/cron` should
    /// not be told their spelling does not exist.
    #[test]
    fn the_empty_listing_names_the_canonical_spelling() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let out = listing(&store);
        assert!(
            out.contains("/crons add"),
            "the hint must name the canonical spelling: {out}"
        );
        assert!(
            out.contains("/cron works too"),
            "and must not orphan a reader who learned the alias: {out}"
        );
        assert!(
            !out.contains("(/cron add"),
            "the alias must never be the one the hint teaches: {out}"
        );
    }

    /// The busy half of the wakeup's concurrency contract, and the one
    /// that needs no provider: a session-wakeup that finds its target
    /// running a turn PARKS the prompt on the session queue (the
    /// target's own drainer runs it at the boundary) instead of
    /// starting a second concurrent turn. `Queued`, not `ok`/`error` —
    /// the run history must say the fire deferred rather than lied
    /// about a reply.
    #[tokio::test]
    async fn session_wakeup_defers_to_the_queue_when_the_target_is_busy() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session("busy-sess").unwrap();
        // A live turn row is the cross-process truth every wake path
        // reads — register one so the session looks busy.
        store
            .lock_ok()
            .run_start("busy-sess", 999, "box", "tui")
            .unwrap();

        let cfg = Arc::new(crate::config::VelesConfig::default());
        let sched = Scheduler::new(store.clone(), cfg, crate::hooks::HookBus::new(None), None);
        let job = CronJob {
            id: "wakejob".into(),
            prompt: "are we done yet?".into(),
            deliver: "local".into(),
            session: Some("busy-sess".into()),
            ..Default::default()
        };
        let outcome = sched.run_body(&job).await;
        match outcome {
            JobOutcome::Queued { detail } => {
                assert!(detail.contains("busy"), "says why it deferred: {detail}");
            }
            other => panic!("a busy target must defer, got {other:?}"),
        }
        // The prompt is ON the queue for the target's drainer — nothing
        // was dropped on the floor.
        let item = store.lock_ok().queue_take_next("busy-sess").unwrap();
        assert!(
            item.is_some_and(|i| i.body.contains("are we done yet")),
            "the parked prompt is the job's"
        );
        // No turn was started by the scheduler for the busy session.
        assert!(
            !crate::agent::turn_is_live("busy-sess"),
            "the scheduler must not race the running turn"
        );
        store.lock_ok().run_end("busy-sess").unwrap();
    }

    /// The idle half of the wakeup contract, end-to-end against the
    /// stub provider: the turn runs IN the named session (not a fresh
    /// `cron_*`), the session's HISTORY is loaded into the provider
    /// request — that is the whole point of a wakeup — and the turn's
    /// rows append to the session's own transcript. It also proves the
    /// model override does NOT persist: a daily job pinning its own
    /// model must not rewrite the model the user chose in their
    /// session (`set_model_transient`, not `set_model`).
    #[tokio::test]
    async fn session_wakeup_runs_in_the_named_session_with_its_history() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session("daily-sess").unwrap();
        store
            .lock_ok()
            .append_message(
                "daily-sess",
                &crate::providers::ChatMessage::user("remember the codeword BANANA-STAND"),
            )
            .unwrap();
        store
            .lock_ok()
            .append_message(
                "daily-sess",
                &crate::providers::ChatMessage::assistant(
                    "noted, the codeword is BANANA-STAND",
                    None,
                ),
            )
            .unwrap();
        // The user's own route choice, which the job's model override
        // must not clobber in the store.
        store
            .lock_ok()
            .override_replace("daily-sess", None, Some("user-pinned-model"), None)
            .unwrap();

        let reply = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"woken and aware\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, captured) =
            crate::providers::tests::sse_server_seq_capture(vec![reply.to_string()]).await;
        let cfg = Arc::new(crate::config::VelesConfig {
            model: crate::config::ModelSection {
                provider: Some("mock".into()),
                default: Some("mock-model".into()),
                ..Default::default()
            },
            custom_providers: vec![crate::config::ProviderEntry {
                name: "mock".into(),
                base_url: url,
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let sched = Scheduler::new(store.clone(), cfg, crate::hooks::HookBus::new(None), None);
        let job = CronJob {
            id: "wake".into(),
            prompt: "what was the codeword?".into(),
            deliver: "local".into(),
            session: Some("daily-sess".into()),
            model: Some("job-model".into()),
            ..Default::default()
        };
        match sched.run_body(&job).await {
            JobOutcome::Delivered { text, detail } => {
                assert_eq!(text, "woken and aware");
                assert!(
                    detail.contains("in session daily-sess"),
                    "the run detail names the session: {detail}"
                );
            }
            other => panic!("an idle wakeup must deliver, got {other:?}"),
        }

        // The provider request carried the session's HISTORY — a fresh
        // session would have sent only the prompt.
        let req = captured.lock().unwrap().remove(0);
        assert!(
            req.contains("BANANA-STAND"),
            "the woken turn sees the session's context"
        );
        assert!(req.contains("what was the codeword?"), "and the job prompt");
        assert!(
            req.contains("job-model"),
            "the job's model override applies to THIS turn"
        );

        // The transcript GREW in place: prompt + reply rows in the
        // named session, and no fresh cron_* session minted.
        let hist = store.lock_ok().load_history("daily-sess").unwrap();
        let all: String = hist
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("what was the codeword?"), "{all}");
        assert!(all.contains("woken and aware"), "{all}");
        let sessions: Vec<String> = store
            .lock_ok()
            .list_sessions(100)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            sessions,
            vec!["daily-sess".to_string()],
            "no cron_* session was minted: {sessions:?}"
        );

        // The user's stored route survived the job's override — the
        // transient set is what makes a daily job safe to point at a
        // session a human uses.
        let ov = store
            .lock_ok()
            .override_get("daily-sess")
            .unwrap()
            .expect("override row");
        assert_eq!(
            ov.model.as_deref(),
            Some("user-pinned-model"),
            "a job's model pin is transient, never a rewrite of the user's choice"
        );
    }

    /// A wakeup whose target session is GONE (deleted since creation)
    /// fails honestly with the reason — it does not silently fall back
    /// to a fresh session and pretend it woke something.
    #[tokio::test]
    async fn session_wakeup_for_a_deleted_session_fails_honestly() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let cfg = Arc::new(crate::config::VelesConfig::default());
        let sched = Scheduler::new(store, cfg, crate::hooks::HookBus::new(None), None);
        let job = CronJob {
            id: "orphan".into(),
            prompt: "knock knock".into(),
            deliver: "local".into(),
            session: Some("gone-sess".into()),
            ..Default::default()
        };
        match sched.run_body(&job).await {
            JobOutcome::Failed(e) => assert!(
                e.to_string().contains("does not exist"),
                "names the missing session: {e:#}"
            ),
            other => panic!("a deleted target must fail, got {other:?}"),
        }
    }

    // ── the SessionRunner seam ─────────────────────────────────────
    //
    // A fake turn-owning surface. The registry is process-wide and
    // tests run in parallel, so every fake owns exactly ONE unique
    // session id and answers "not mine" to everything else — no test
    // may clear the registry out from under another.

    struct FakeRunner {
        owns: String,
        lock: Arc<tokio::sync::Mutex<()>>,
        /// Set when kick_queue fired.
        kicked: Arc<std::sync::atomic::AtomicBool>,
        /// True when the scheduler had already DROPPED the lock guard
        /// by the time the kick arrived — the deadlock-ordering pin:
        /// both real kick implementations need the lock free (the
        /// gateway re-injects an inbound that try_locks it; the
        /// desktop drain awaits it), so kick-under-held-lock would
        /// strand the parked row this kick exists to rescue.
        guard_dropped_at_kick: Arc<std::sync::atomic::AtomicBool>,
        /// True while the woken turn is running UNDER the borrowed
        /// lock: sampled by the stub provider's response... sampled
        /// instead by the outcome check below via this flag set in
        /// try_lock and cleared on guard drop is not observable from
        /// here — the turn-body observation lives in the test that
        /// drives a real stub provider (below).
        lock_held_during_turn: Arc<std::sync::atomic::AtomicBool>,
    }

    impl SessionRunner for FakeRunner {
        fn owns_session(&self, session_id: &str) -> bool {
            session_id == self.owns
        }
        fn try_lock_session(&self, session_id: &str) -> Option<SessionLockGuard> {
            if session_id != self.owns {
                return None;
            }
            match self.lock.clone().try_lock_owned() {
                Ok(g) => {
                    self.lock_held_during_turn
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    Some(SessionLockGuard::new(LockSpy {
                        _guard: g,
                        flag: self.lock_held_during_turn.clone(),
                    }))
                }
                Err(_) => None,
            }
        }
        fn kick_queue(&self, _session_id: &str) {
            self.kicked.store(true, std::sync::atomic::Ordering::SeqCst);
            // The scheduler must have dropped the guard BEFORE
            // kicking; if it had not, this try_lock would fail.
            if self.lock.try_lock().is_ok() {
                self.guard_dropped_at_kick
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Clears the held flag when the scheduler drops the guard.
    struct LockSpy {
        _guard: tokio::sync::OwnedMutexGuard<()>,
        flag: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for LockSpy {
        fn drop(&mut self) {
            self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn fake_runner(owns: &str) -> (Arc<FakeRunner>, Arc<tokio::sync::Mutex<()>>) {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let r = Arc::new(FakeRunner {
            owns: owns.to_string(),
            lock: lock.clone(),
            kicked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            guard_dropped_at_kick: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lock_held_during_turn: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        register_session_runner(r.clone());
        (r, lock)
    }

    /// Owned + owner mid-turn → the wakeup PARKS, honestly, and does
    /// not run a second concurrent turn in the session. Removing the
    /// `owned && guard.is_none()` park from run_session_wakeup_job
    /// turns this red (the scheduler would try to run the turn — and
    /// in production two turns would interleave one transcript).
    #[tokio::test]
    async fn session_wakeup_parks_when_the_owner_surface_is_mid_turn() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session("owned-busy-sess").unwrap();
        let (runner, lock) = fake_runner("owned-busy-sess");
        // The owner is mid-turn: its lock is held by someone else.
        let _owner_turn = lock.lock().await;

        let cfg = Arc::new(crate::config::VelesConfig::default());
        let sched = Scheduler::new(store.clone(), cfg, crate::hooks::HookBus::new(None), None);
        let job = CronJob {
            id: "wake-busy".into(),
            prompt: "daily nudge".into(),
            deliver: "local".into(),
            session: Some("owned-busy-sess".into()),
            ..Default::default()
        };
        match sched.run_body(&job).await {
            JobOutcome::Queued { detail } => {
                assert!(
                    detail.contains("mid-turn"),
                    "the run row says why it parked: {detail}"
                );
            }
            other => panic!("an owned busy session must park, got {other:?}"),
        }
        // The prompt is a queue row — the running turn's boundary
        // drain will run it. Nothing was lost.
        let queued: Vec<String> = store
            .lock_ok()
            .queue_items("owned-busy-sess")
            .unwrap()
            .into_iter()
            .map(|i| i.body)
            .collect();
        assert_eq!(queued, vec!["daily nudge".to_string()]);
        assert!(
            !runner.kicked.load(std::sync::atomic::Ordering::SeqCst),
            "no kick on the parked path — the OWNER's drain covers it"
        );
    }

    /// Owned + free → the turn runs UNDER the owner's lock (that is
    /// the race the seam closes: an inbound message now blocks on the
    /// same lock instead of interleaving), and after the turn the
    /// scheduler drops the guard BEFORE kicking — both real kick
    /// implementations need the lock free, and kick-under-held-lock
    /// would re-strand the very row the kick rescues.
    #[tokio::test]
    async fn session_wakeup_runs_under_the_owner_lock_and_kicks_after_dropping_it() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        store.lock_ok().create_session("owned-free-sess").unwrap();
        let (runner, lock) = fake_runner("owned-free-sess");

        let reply = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"woken under lock\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, _captured) =
            crate::providers::tests::sse_server_seq_capture(vec![reply.to_string()]).await;
        let cfg = Arc::new(crate::config::VelesConfig {
            model: crate::config::ModelSection {
                provider: Some("mock".into()),
                default: Some("mock-model".into()),
                ..Default::default()
            },
            custom_providers: vec![crate::config::ProviderEntry {
                name: "mock".into(),
                base_url: url,
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let sched = Scheduler::new(store.clone(), cfg, crate::hooks::HookBus::new(None), None);
        let job = CronJob {
            id: "wake-owned".into(),
            prompt: "morning check".into(),
            deliver: "local".into(),
            session: Some("owned-free-sess".into()),
            ..Default::default()
        };
        match sched.run_body(&job).await {
            JobOutcome::Delivered { text, .. } => assert_eq!(text, "woken under lock"),
            other => panic!("an owned idle session must run the turn, got {other:?}"),
        }

        // The lock was held across the turn and released after it:
        // the spy flag is clear, and the mutex itself is takeable.
        assert!(
            !runner
                .lock_held_during_turn
                .load(std::sync::atomic::Ordering::SeqCst),
            "the guard was dropped when run_body returned"
        );
        assert!(
            lock.try_lock().is_ok(),
            "the owner's lock is free again after the wakeup"
        );
        // The kick fired, and the guard was ALREADY dropped when it
        // did — the ordering the two real kick implementations
        // depend on. Removing `drop(_session_guard)` before the kick
        // in run_session_wakeup_job turns this red.
        assert!(
            runner.kicked.load(std::sync::atomic::Ordering::SeqCst),
            "the post-turn kick fired for the owned session"
        );
        assert!(
            runner
                .guard_dropped_at_kick
                .load(std::sync::atomic::Ordering::SeqCst),
            "the kick ran with the lock FREE — drop must precede kick"
        );
        // The turn really ran in the session: transcript grew.
        let hist = store.lock_ok().load_history("owned-free-sess").unwrap();
        let all: String = hist
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("morning check") && all.contains("woken under lock"),
            "{all}"
        );
    }

    /// A FRESH cron job (no target session) runs a turn nobody asked
    /// for, and nobody is watching. The transcript must say so when it
    /// is later opened — the Schedule note (agent.rs, surface gate
    /// `surface == "cron"`) is that announcement, and the fresh-job
    /// agent NEVER set the surface, so standalone scheduled jobs ran
    /// invisible. (PENDING 2026-09-19 "Schedule notes don't survive the
    /// turn" — the persistence half was already true; the EMIT half of
    /// the fresh-session path was the hole.)
    #[tokio::test]
    async fn a_fresh_job_turn_carries_the_surface_that_announces_it() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let reply = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"standalone result\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, _captured) =
            crate::providers::tests::sse_server_seq_capture(vec![reply.to_string()]).await;
        let cfg = Arc::new(crate::config::VelesConfig {
            model: crate::config::ModelSection {
                provider: Some("mock".into()),
                default: Some("mock-model".into()),
                ..Default::default()
            },
            custom_providers: vec![crate::config::ProviderEntry {
                name: "mock".into(),
                base_url: url,
                api_mode: "chat_completions".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let sched = Scheduler::new(store.clone(), cfg, crate::hooks::HookBus::new(None), None);
        let job = CronJob {
            id: "nightly".into(),
            prompt: "standalone report".into(),
            deliver: "local".into(),
            session: None,
            ..Default::default()
        };
        match sched.run_body(&job).await {
            JobOutcome::Delivered { .. } => {}
            other => panic!("a fresh job must deliver, got {other:?}"),
        }

        // The turn's own session (cron_<job>_<stamp>, title cron:…)
        // carries the Schedule note that says nobody asked for it.
        let sessions = store.lock_ok().list_sessions(100).unwrap();
        let (sid, _) = sessions
            .iter()
            .find(|(_, title)| title.starts_with("cron:"))
            .expect("the fresh job minted a titled cron session");
        let notes = store.lock_ok().notes_for(sid).unwrap();
        let schedule_notes: Vec<&crate::store::StoredNote> =
            notes.iter().filter(|n| n.kind == "schedule").collect();
        assert_eq!(
            schedule_notes.len(),
            1,
            "an unattended turn announces itself: {notes:?}"
        );
        assert!(
            schedule_notes[0].summary.contains("ran on a schedule"),
            "{}",
            schedule_notes[0].summary
        );
    }
}

#[cfg(test)]
mod b3a_cron_pins {
    use super::*;

    /// B3a: a lost lease skips the tick entirely — no claims, no fires.
    /// A guard past its expiry is the suspended-laptop case.
    #[tokio::test]
    async fn cron_tick_skips_without_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir.path().join("c.db")).unwrap(),
        ));
        let cfg = Arc::new(VelesConfig::default());
        let sched = Scheduler::new(store.clone(), cfg, crate::hooks::HookBus::new(None), None);
        // A job due NOW, so an ungated tick would claim it.
        {
            let s = store.lock().unwrap();
            s.cron_upsert(&CronJob {
                id: "j1".into(),
                name: Some("every-5".into()),
                schedule: "every:5".into(),
                prompt: "ping".into(),
                enabled: true,
                created_at: (Utc::now() - chrono::Duration::minutes(10))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
                ..Default::default()
            })
            .expect("cron_upsert");
        }
        // A standby guard (never granted): held() is false.
        let guard = std::sync::Arc::new(crate::arbiter::LeaseGuard::standby("singleton:cron", "n"));
        sched.set_lease(guard);
        let sem = Arc::new(Semaphore::new(3));
        sched.tick(&sem).await.expect("tick");
        // The slot was NOT claimed: the ledger has no cron: row.
        let claimed = {
            let c = rusqlite::Connection::open(dir.path().join("c.db")).unwrap();
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM inbound_ledger WHERE dedup_key LIKE 'cron:%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            n
        };
        assert_eq!(claimed, 0, "a lost lease must not claim slots");
        // And the ungated scheduler (no guard) DOES claim — the gate is
        // the difference, not a broken tick.
        let sched2 = Scheduler::new(
            store.clone(),
            Arc::new(VelesConfig::default()),
            crate::hooks::HookBus::new(None),
            None,
        );
        sched2.tick(&sem).await.expect("tick");
        let claimed2 = {
            let c = rusqlite::Connection::open(dir.path().join("c.db")).unwrap();
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM inbound_ledger WHERE dedup_key LIKE 'cron:%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            n
        };
        assert!(claimed2 > 0, "ungated claims the due slot (sanity)");
    }
}
