//! hermes → veles migration for the things that are not config and not
//! transcripts: skills, cron jobs, and usage history.
//!
//! `veles migrate` moves config.yaml; `veles import-hermes` moves the
//! transcripts. Between them sat everything else, and one item on that
//! list is not a nicety:
//!
//! **veles reads `~/.hermes/skills` AT RUNTIME.** `skills::skill_roots`
//! lists it as a root for migration compatibility, so a skill that
//! exists only there is in every session's prompt index — and deleting
//! the hermes install silently removes it. Until the skills are copied
//! across, the migration is not finished no matter what else has moved,
//! because `~/.hermes` can never be deleted.
//!
//! Everything here is:
//!
//!   - **Idempotent.** Re-running after a partial run is safe and says
//!     what it skipped. A skill, job or usage row already present is
//!     left exactly as it is.
//!   - **Non-destructive to hermes.** Nothing is written to
//!     `~/.hermes`, and its state.db is opened READ-ONLY — the
//!     2026-08-09 poisoning incident (a veles trigger created inside
//!     hermes' FTS schema) is why that is structural rather than a
//!     promise.
//!   - **Veles-wins.** Where both sides have something under one name,
//!     the veles copy stands. A migration that overwrote local edits
//!     with older upstream ones would be a data-loss bug wearing a
//!     helpful hat.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// What one migration step did, in the words the CLI prints.
#[derive(Debug, Default, Clone)]
pub struct StepReport {
    pub what: String,
    pub moved: usize,
    pub skipped: usize,
    /// Anything the user has to know to trust the number above.
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

impl StepReport {
    fn new(what: &str) -> Self {
        Self {
            what: what.to_string(),
            ..Default::default()
        }
    }
}

fn hermes_home() -> Option<PathBuf> {
    let home = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf())?;
    let h = home.join(".hermes");
    h.is_dir().then_some(h)
}

// ── skills ──────────────────────────────────────────────────────────

/// Copy every hermes skill veles does not already own.
///
/// This is the step that lets `~/.hermes` be deleted. `SkillSet::load`
/// de-duplicates by name with veles winning, so a name that exists on
/// both sides is already resolved in veles' favour at runtime — copying
/// it would change nothing and could clobber a local edit. Only the
/// names veles does not have move.
pub fn migrate_skills(dry_run: bool) -> Result<StepReport> {
    let mut r = StepReport::new("skills");
    let Some(hermes) = hermes_home() else {
        r.notes.push("no ~/.hermes install found".into());
        return Ok(r);
    };
    let src_root = hermes.join("skills");
    if !src_root.is_dir() {
        r.notes.push("hermes has no skills directory".into());
        return Ok(r);
    }
    let dest_root = crate::skills::veles_skills_root();

    // The names veles already answers for, whatever root they came from.
    let existing: std::collections::HashSet<String> = crate::skills::SkillSet::load()
        .skills
        .iter()
        .filter(|s| s.root == "veles")
        .map(|s| s.name.clone())
        .collect();

    let mut found = Vec::new();
    collect_skill_dirs(&src_root, &mut found);
    for dir in found {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        if existing.contains(&name) {
            r.skipped += 1;
            continue;
        }
        // Keep the category directory the skill sits in, so an imported
        // skill lands where the index expects it.
        let rel = dir.strip_prefix(&src_root).unwrap_or(&dir);
        let dest = dest_root.join(rel);
        if dest.exists() {
            r.skipped += 1;
            continue;
        }
        if dry_run {
            r.moved += 1;
            continue;
        }
        match copy_tree(&dir, &dest) {
            Ok(()) => r.moved += 1,
            Err(e) => r.problems.push(format!("{name}: {e}")),
        }
    }
    if r.moved > 0 || r.skipped > 0 {
        r.notes
            .push("veles keeps its own copy where a name exists on both sides".into());
    }
    Ok(r)
}

/// Walk a skills root the way `skills::discover_in` does: a directory
/// holding SKILL.md is a skill, otherwise recurse one level for
/// categories.
fn collect_skill_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("SKILL.md").is_file() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && p.file_name().map(|n| n != ".git").unwrap_or(false) {
            collect_skill_dirs(&p, out);
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok(())
}

// ── cron ────────────────────────────────────────────────────────────

/// Translate `~/.hermes/cron/jobs.json` into veles cron jobs.
///
/// The schedule is the part that has to be translated rather than
/// copied: hermes stores a shape (`{kind: interval, minutes: 15}`),
/// veles a normalized string (`every:900`). A job whose schedule cannot
/// be translated is REPORTED AND SKIPPED, never imported disabled with
/// a broken schedule — a job that silently never fires is worse than
/// one you were told about.
pub fn migrate_cron(
    store: &std::sync::Mutex<crate::store::Store>,
    dry_run: bool,
) -> Result<StepReport> {
    let mut r = StepReport::new("cron jobs");
    let Some(hermes) = hermes_home() else {
        r.notes.push("no ~/.hermes install found".into());
        return Ok(r);
    };
    let path = hermes.join("cron").join("jobs.json");
    if !path.exists() {
        r.notes.push("hermes has no cron/jobs.json".into());
        return Ok(r);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let jobs = doc["jobs"].as_array().cloned().unwrap_or_default();

    for j in jobs {
        let name = j["name"].as_str().unwrap_or("").to_string();
        let id = j["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            r.problems.push("a job with no id".into());
            continue;
        }
        let label = if name.is_empty() {
            id.clone()
        } else {
            name.clone()
        };

        let schedule = match hermes_schedule(&j["schedule"]) {
            Some(s) => match crate::cron::normalize_schedule(&s) {
                Ok(n) => n,
                Err(e) => {
                    r.problems.push(format!("{label}: schedule `{s}` — {e}"));
                    continue;
                }
            },
            None => {
                r.problems
                    .push(format!("{label}: unrecognized schedule shape"));
                continue;
            }
        };

        {
            let store = store.lock().map_err(|_| anyhow::anyhow!("store lock"))?;
            if store.cron_get(&id)?.is_some() {
                r.skipped += 1;
                continue;
            }
        }
        if dry_run {
            r.moved += 1;
            continue;
        }

        let job = crate::store::CronJob {
            id: id.clone(),
            name: (!name.is_empty()).then_some(name),
            schedule,
            prompt: j["prompt"].as_str().unwrap_or("").to_string(),
            // A hermes `deliver` target names a platform chat veles may
            // not have an adapter for. `local` is the honest landing
            // place: the job runs and its output goes to the log, rather
            // than being addressed somewhere that will never receive it.
            deliver: "local".into(),
            enabled: j["enabled"].as_bool().unwrap_or(false),
            ..Default::default()
        };
        let deliver_was = j["deliver"].as_str().unwrap_or("");
        if !deliver_was.is_empty() && deliver_was != "local" {
            r.notes.push(format!(
                "{label}: delivery was `{deliver_was}`, imported as `local`"
            ));
        }
        let store = store.lock().map_err(|_| anyhow::anyhow!("store lock"))?;
        match store.cron_upsert(&job) {
            Ok(()) => r.moved += 1,
            Err(e) => r.problems.push(format!("{label}: {e}")),
        }
    }
    Ok(r)
}

/// A hermes schedule object → a string `cron::normalize_schedule` reads.
fn hermes_schedule(v: &serde_json::Value) -> Option<String> {
    match v["kind"].as_str()? {
        "once" => v["run_at"].as_str().map(str::to_string),
        "interval" => {
            // hermes spells the period in whichever unit it was given.
            for (field, suffix) in [("seconds", "s"), ("minutes", "m"), ("hours", "h")] {
                if let Some(n) = v[field].as_u64() {
                    return Some(format!("{n}{suffix}"));
                }
            }
            None
        }
        "cron" => v["spec"]
            .as_str()
            .or_else(|| v["cron"].as_str())
            .map(str::to_string),
        _ => None,
    }
}

// ── usage ───────────────────────────────────────────────────────────

/// Carry hermes' per-session token accounting across.
///
/// Only for sessions that EXIST in the veles store, so this is run after
/// `veles import-hermes` and cannot leave usage rows orphaned from the
/// transcripts they describe.
///
/// Tokens are exact. The CALL COUNT is not: hermes records
/// `api_call_count` per (session, model, provider) group, and veles'
/// `usage` table has no such column — `usage_totals` counts rows. So an
/// imported group contributes one "call". The report says so rather
/// than letting a wrong number look authoritative.
pub fn migrate_usage(
    store: &std::sync::Mutex<crate::store::Store>,
    db: Option<&Path>,
    dry_run: bool,
) -> Result<StepReport> {
    use rusqlite::OpenFlags;
    let mut r = StepReport::new("usage history");

    let default_db = hermes_home().map(|h| h.join("state.db"));
    let Some(path) = db.map(Path::to_path_buf).or(default_db) else {
        r.notes.push("no ~/.hermes install found".into());
        return Ok(r);
    };
    if !path.exists() {
        r.notes.push(format!("{} does not exist", path.display()));
        return Ok(r);
    }

    // READ-ONLY, structurally. See the module header.
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;

    let has_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='session_model_usage'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_table == 0 {
        r.notes
            .push("this hermes DB has no session_model_usage table".into());
        return Ok(r);
    }

    let mut stmt = conn.prepare(
        "SELECT session_id, billing_provider, model, input_tokens, output_tokens, api_call_count
         FROM session_model_usage",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;

    let mut calls_recorded: i64 = 0;
    let mut no_session = 0usize;
    for row in rows {
        let (sid, provider, model, input, output, calls) = row?;
        let store = store.lock().map_err(|_| anyhow::anyhow!("store lock"))?;
        if !store.session_exists(&sid).unwrap_or(false) {
            no_session += 1;
            continue;
        }
        if store.usage_exists(&sid, &provider, &model).unwrap_or(false) {
            r.skipped += 1;
            continue;
        }
        if dry_run {
            r.moved += 1;
            calls_recorded += calls;
            continue;
        }
        // hermes' usage history has no cache accounting; carry 0/0.
        match store.record_usage(
            &sid,
            &provider,
            &model,
            crate::store::CallTokens {
                prompt_tokens: input as u64,
                completion_tokens: output as u64,
                ..Default::default()
            },
        ) {
            Ok(()) => {
                r.moved += 1;
                calls_recorded += calls;
            }
            Err(e) => r.problems.push(format!("{sid}/{model}: {e}")),
        }
    }
    if no_session > 0 {
        r.notes.push(format!(
            "{no_session} row(s) skipped — their session is not in the veles store (run `veles import-hermes` first)"
        ));
    }
    if r.moved > 0 {
        r.notes.push(format!(
            "tokens are exact; hermes recorded {calls_recorded} API calls across these {} group(s), and veles counts one call per group",
            r.moved
        ));
    }
    Ok(r)
}

// ── the question the whole command exists to answer ─────────────────

/// Skill names that exist ONLY under `~/.hermes/skills`.
///
/// This is the migration's actual finish line, and it is checked rather
/// than assumed: while this list is non-empty, `~/.hermes` is
/// load-bearing — `skills::skill_roots` reads it live, so those skills
/// are in every session's prompt index and deleting the hermes install
/// would remove them without a word.
pub fn skills_only_in_hermes() -> Vec<String> {
    let set = crate::skills::SkillSet::load();
    let mut out: Vec<String> = set
        .skills
        .iter()
        .filter(|s| s.root != "veles")
        .map(|s| s.name.clone())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn sched(json: &str) -> Option<String> {
        hermes_schedule(&serde_json::from_str(json).expect("fixture"))
    }

    /// The schedule is the one field that must be TRANSLATED rather than
    /// copied, and getting the unit wrong is silent: a job that never
    /// fires, or one that fires constantly.
    #[test]
    fn hermes_schedules_translate_to_the_veles_grammar() {
        assert_eq!(
            sched(r#"{"kind":"once","run_at":"2026-10-01T09:00:00+02:00"}"#).as_deref(),
            Some("2026-10-01T09:00:00+02:00")
        );
        assert_eq!(
            sched(r#"{"kind":"interval","minutes":15}"#).as_deref(),
            Some("15m")
        );
        assert_eq!(
            sched(r#"{"kind":"interval","hours":6}"#).as_deref(),
            Some("6h")
        );
        assert_eq!(
            sched(r#"{"kind":"interval","seconds":90}"#).as_deref(),
            Some("90s")
        );
        assert_eq!(
            sched(r#"{"kind":"cron","spec":"0 9 * * 1"}"#).as_deref(),
            Some("0 9 * * 1")
        );
        // Unrecognized shapes produce None so the caller can REPORT and
        // skip, rather than importing a job with a broken schedule.
        assert_eq!(sched(r#"{"kind":"phase-of-moon"}"#), None);
        assert_eq!(sched(r#"{"kind":"interval"}"#), None);
    }

    /// …and each one the engine will actually accept.
    #[test]
    fn every_translated_schedule_normalizes() {
        // A one-shot is only valid in the FUTURE (normalize_schedule
        // refuses a past one), so the fixture is built from now rather
        // than pinned to a date. This test hardcoded
        // 2026-10-01T09:00:00+02:00 and began failing the moment the
        // wall clock passed it (measured 2026-10-01, on the gate):
        // a date literal in a test that checks validity-against-now is
        // a time bomb, not a fixture.
        let future = (chrono::Utc::now() + chrono::Duration::days(365)).to_rfc3339();
        let once = format!(r#"{{"kind":"once","run_at":"{future}"}}"#);
        for j in [
            once.as_str(),
            r#"{"kind":"interval","minutes":15}"#,
            r#"{"kind":"cron","spec":"0 9 * * 1"}"#,
        ] {
            let s = sched(j).expect("translated");
            let n =
                crate::cron::normalize_schedule(&s).unwrap_or_else(|e| panic!("{s} from {j}: {e}"));
            assert!(
                n.starts_with("once:") || n.starts_with("every:") || n.starts_with("cron:"),
                "{n}"
            );
        }
        // The once arm really produced a once: (not a silent fall-through
        // to another shape).
        let once_normalized = crate::cron::normalize_schedule(&sched(&once).unwrap()).unwrap();
        assert!(once_normalized.starts_with("once:"));
        // 15 minutes is 900 seconds, not 15.
        assert_eq!(
            crate::cron::normalize_schedule(&sched(r#"{"kind":"interval","minutes":15}"#).unwrap())
                .unwrap(),
            "every:900"
        );
    }

    /// A hermes state.db with just the usage table, so the importer can
    /// be driven without the real 355MB one.
    fn hermes_usage_db(dir: &Path, rows: &[(&str, &str, &str, i64, i64, i64)]) -> PathBuf {
        let path = dir.join("hermes.db");
        let c = Connection::open(&path).expect("create");
        c.execute_batch(
            "CREATE TABLE session_model_usage (
                session_id TEXT, model TEXT, billing_provider TEXT,
                input_tokens INTEGER, output_tokens INTEGER, api_call_count INTEGER);",
        )
        .expect("schema");
        for (sid, provider, model, inp, out, calls) in rows {
            c.execute(
                "INSERT INTO session_model_usage
                 (session_id, billing_provider, model, input_tokens, output_tokens, api_call_count)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![sid, provider, model, inp, out, calls],
            )
            .expect("insert");
        }
        path
    }

    #[test]
    fn usage_imports_tokens_only_for_sessions_that_exist() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&home.path().join("state.db")).expect("store");
        store.create_session("kept").expect("session");
        let store = std::sync::Mutex::new(store);

        let db = hermes_usage_db(
            home.path(),
            &[
                ("kept", "prov", "model-a", 100, 20, 7),
                ("gone", "prov", "model-b", 999, 999, 3),
            ],
        );

        let r = migrate_usage(&store, Some(&db), false).expect("migrate");
        assert_eq!(r.moved, 1, "only the session that exists: {r:?}");
        assert!(
            r.notes.iter().any(|n| n.contains("not in the veles store")),
            "and it says why the other was skipped: {r:?}"
        );

        let u = store
            .lock()
            .unwrap()
            .usage_totals(Some("kept"))
            .expect("totals");
        assert_eq!(u.prompt_tokens, 100, "tokens are exact");
        assert_eq!(u.completion_tokens, 20);
        // The honest limitation: veles counts a usage ROW as one call,
        // and hermes recorded 7 for this group.
        assert_eq!(u.calls, 1);
        assert!(
            r.notes.iter().any(|n| n.contains("7 API calls")),
            "the report says so rather than letting the number look right: {r:?}"
        );
    }

    /// Re-running must not double the totals — there is no unique
    /// constraint on `usage` to lean on.
    #[test]
    fn usage_import_is_idempotent() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&home.path().join("state.db")).expect("store");
        store.create_session("s").expect("session");
        let store = std::sync::Mutex::new(store);
        let db = hermes_usage_db(home.path(), &[("s", "p", "m", 50, 5, 2)]);

        let first = migrate_usage(&store, Some(&db), false).expect("first");
        let second = migrate_usage(&store, Some(&db), false).expect("second");
        assert_eq!(first.moved, 1);
        assert_eq!(second.moved, 0, "nothing moves twice: {second:?}");
        assert_eq!(second.skipped, 1);

        let u = store.lock().unwrap().usage_totals(Some("s")).unwrap();
        assert_eq!(u.prompt_tokens, 50, "totals are not doubled");
    }

    /// A dry run must read and report without writing anything.
    #[test]
    fn a_dry_run_writes_nothing() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&home.path().join("state.db")).expect("store");
        store.create_session("s").expect("session");
        let store = std::sync::Mutex::new(store);
        let db = hermes_usage_db(home.path(), &[("s", "p", "m", 50, 5, 2)]);

        let r = migrate_usage(&store, Some(&db), true).expect("dry");
        assert_eq!(r.moved, 1, "it still reports what WOULD move");
        let u = store.lock().unwrap().usage_totals(Some("s")).unwrap();
        assert_eq!(u.prompt_tokens, 0, "…and wrote nothing");
    }

    /// The hermes DB must be opened read-only. The 2026-08-09 incident
    /// was veles creating a trigger inside hermes' schema; making the
    /// open flags structural is the fix, and this is the guard on it.
    #[test]
    fn the_hermes_db_is_never_written() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&home.path().join("state.db")).expect("store");
        store.create_session("s").expect("session");
        let store = std::sync::Mutex::new(store);
        let db = hermes_usage_db(home.path(), &[("s", "p", "m", 1, 1, 1)]);

        let before = std::fs::metadata(&db).expect("stat").len();
        let mtime_before = std::fs::metadata(&db).unwrap().modified().unwrap();
        migrate_usage(&store, Some(&db), false).expect("migrate");
        let after = std::fs::metadata(&db).expect("stat").len();
        assert_eq!(before, after, "the hermes DB must not grow");
        assert_eq!(
            mtime_before,
            std::fs::metadata(&db).unwrap().modified().unwrap(),
            "…or be touched at all"
        );
    }
}
