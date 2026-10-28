//! Dreaming — background reflection that consolidates recent sessions
//! into durable memory (S2, Letta sleep-time-compute inspired, no cloud).
//!
//! What it does: gather the last N days' assistant/user turns across
//! sessions, feed a digest to the LLM with a reflection prompt ("what
//! durable lessons, preferences, or project facts emerged that belong
//! in memory?"), and write the output into
//! the memory file `reference/lessons/YYYY-MM-DD.md` (one per day, a
//! revision each). NOT silently merged into core
//! memory: it lands as a signposted reference file the owner (or a
//! future wake-up) decides to fold in. This is the Letta "dream but
//! agent reviews before applying" posture, minus auto-apply.
//!
//! Surfaces:
//!   - `veles dream [--days N]` — one reflection pass, print result
//!   - `/dream` CLI/slash — same
//!   - the gateway scheduler, when `memory.dream_schedule` is set — the
//!     automatic path. It was the only caller nobody ever invoked until
//!     that knob existed; see `cron::Scheduler::maybe_dream`.
//!
//! Runs as its own job (never a chat hook), so no surface-gating needed
//! beyond "don't run it from inside a turn" — it's invoked as a CLI job.

use crate::agent::Agent;
use crate::lockutil::LockOk;
use crate::memory::MemoryStore;
use anyhow::Result;
use std::sync::Arc;

/// Gather a digest of recent sessions' user/assistant turns, newest
/// first, until `budget` chars. Returns (draft, sesn_count).
fn digest(store: &std::sync::Mutex<crate::store::Store>, budget: usize) -> (String, usize) {
    let g = store.lock_ok();
    let sessions = match g.list_sessions(200) {
        Ok(s) => s,
        Err(_) => return (String::new(), 0),
    };
    let mut out = String::new();
    let mut count = 0usize;
    for (sid, title) in sessions {
        if out.len() >= budget {
            break;
        }
        let Ok(hist) = g.load_history(&sid) else {
            continue;
        };
        let name = if title.trim().is_empty() {
            sid.clone()
        } else {
            title
        };
        out.push_str(&format!("## {name}\n"));
        for m in hist.iter() {
            if m.role != "user" && m.role != "assistant" {
                continue;
            }
            let line = if m.role == "user" {
                format!("U: {}", m.content)
            } else {
                format!("A: {}", m.content)
            };
            if line.trim().is_empty() {
                continue;
            }
            out.push_str(&line);
            out.push('\n');
            if out.len() >= budget {
                out.push_str("… [digest truncated]");
                break;
            }
        }
        if out.len() < budget && !hist.is_empty() {
            count += 1;
        }
        out.push('\n');
    }
    (out, count)
}

const REFLECTION_PROMPT: &str = "\
You are reflecting on recent agent work sessions to improve the owner's \
long-term memory. From the DRAFT below, identify durable, reusable \
lessons that are NOT already obvious: the owner's stable preferences, \
project conventions, environment facts, and pitfalls worth remembering. \
Do NOT record one-off task details. Do NOT invent facts not in the draft.

Write your output as Markdown under a `## Lessons` heading, one bullet per \
lesson — each a one-line headline plus a short justification. If there is \
nothing durable, respond with exactly: No new durable lessons.

DRAFT:
";

const NO_LESSONS: &str = "No new durable lessons";

/// Run a dreaming pass: digest → reflect → write lessons file.
/// Returns the lessons text (or the no-lessons marker).
pub async fn dream(cfg: Arc<crate::config::VelesConfig>) -> Result<String> {
    let home = crate::config::veles_home();
    let store = Arc::new(std::sync::Mutex::new(crate::store::Store::open(
        &home.join("state.db"),
    )?));
    let (draft, _n) = digest(&store, 60_000);
    if draft.trim().is_empty() {
        return Ok("no recent sessions to reflect on".into());
    }

    // Build an agent for the reflection turn (default route config).
    let tcl = crate::tclrt::spawn(crate::config::config_dir())?;
    let hooks = crate::hooks::HookBus::new(Some(tcl));
    let sid = crate::store::new_session_id();
    store.lock_ok().create_session(&sid)?;
    let registry = Arc::new(crate::tools::base_registry(store.clone(), &sid));
    let agent = Agent::new(cfg, hooks, registry, store, sid.clone())?.with_surface("cron");

    let mut hist = Vec::new();
    let turn = agent
        .run_turn(
            &mut hist,
            &format!("{REFLECTION_PROMPT}\n{draft}"),
            None,
            |_| {},
        )
        .await?;
    let lessons = turn.reply.trim().to_string();
    // Marker detection must tolerate the model prefixing the exact
    // phrase (e.g. a "## Lessons" heading). Case- and prefix-robust.
    if lessons.to_lowercase().contains("no new durable lessons") {
        return Ok(NO_LESSONS.into());
    }

    // Append to today's reference/lessons/ file — one revision.
    let mem = MemoryStore::new();
    let _ = mem.ensure_hierarchy();
    let rel = format!("reference/lessons/{}.md", today_file());
    let existing = mem.read_file(&rel).unwrap_or_default();
    let combined = format!("{existing}\n{lessons}\n");
    mem.write_file(&rel, &combined)?;
    Ok(lessons)
}

fn today_file() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_file_is_iso_date() {
        let f = today_file();
        assert_eq!(f.len(), 10, "YYYY-MM-DD, got {f}");
        assert_eq!(f.matches('-').count(), 2, "{f}");
    }

    #[test]
    fn no_lessons_marker_matches_prompt_instruction() {
        // The marker the model is told to emit must be the exact string
        // we compare against — a drift here breaks the skip-write path.
        assert!(REFLECTION_PROMPT.contains(NO_LESSONS));
    }

    #[test]
    fn digest_bounds_and_counts() {
        // Empty store → no draft.
        let dir = std::env::temp_dir().join(format!(
            "veles-dream-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = std::sync::Mutex::new(crate::store::Store::open(&dir.join("db")).unwrap());
        let (draft, n) = digest(&store, 10_000);
        assert!(draft.is_empty());
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
