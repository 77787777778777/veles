//! Subagent definitions from Markdown files (S7, Letta `.letta/agents`
//! parity) — declarative presets for `delegate_task`.
//!
//! Locations (project wins over personal, same precedence as commands):
//!   1. `./.veles/agents/*.md`   — project, shared via the repo
//!   2. `~/.config/veles/agents/*.md` — personal, all projects
//!      (`config_dir()`: a definition is something the user writes)
//!
//! Frontmatter fields:
//!   ---
//!   name: reviewer          (defaults to file stem)
//!   description: Reviews…   (shown in /agents + the delegate prompt hint)
//!   model: glm-5.3          (optional — child model override)
//!   provider: zai           (optional — custom provider name)
//!   reasoning: high         (optional — reasoning effort override)
//!   tools: [terminal]       (optional — advisory; children run one-shot
//!                            `veles chat -q` and cannot have their tool
//!                            registry pruned per-call today, so this is
//!                            recorded and surfaced, not enforced)
//!   ---
//!   Body = default context prepended to every task given through the
//!   preset (persona, conventions, what "done" looks like).
//!
//! The delegate tool resolves `agent: <name>` in its args to a preset;
//! the rendered child prompt is CONTEXT(preset body) + BACKGROUND(user
//! context) + TASK(goal). Surfaces: `/agents` lists on CLI/TUI/gateway;
//! the desktop gets it via the CLI surface contract (same as /commands).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub reasoning: Option<String>,
    /// Advisory tool list (see module docs — recorded, not enforced).
    pub tools: Vec<String>,
    /// Body: default context for tasks run through this preset.
    pub context: String,
    pub source: PathBuf,
}

/// Parse an agent definition file.
fn parse_file(path: &Path) -> Option<AgentDef> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut def = AgentDef {
        name: path.file_stem()?.to_string_lossy().to_string(),
        source: path.to_path_buf(),
        ..Default::default()
    };
    if !def
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || def.name.is_empty()
    {
        return None;
    }
    let mut body_lines: Vec<&str> = Vec::new();
    let mut lines = raw.lines();
    let mut fm_done = false;
    let mut in_fm;
    if lines.next().map(str::trim) == Some("---") {
        in_fm = true;
    } else {
        // No frontmatter: whole file is the body.
        body_lines.push(raw.as_str());
        return Some(finish(def, body_lines));
    }
    'fm: for line in lines.by_ref() {
        if line.trim() == "---" {
            in_fm = false;
            fm_done = true;
            break 'fm;
        }
        let (k, v) = match line.split_once(':') {
            Some(kv) => (kv.0.trim(), kv.1.trim()),
            None => continue,
        };
        match k.to_ascii_lowercase().as_str() {
            "name" => {
                let v = v.trim_matches('"').to_string();
                if !v.is_empty() {
                    def.name = v;
                }
            }
            "description" => def.description = v.trim_matches('"').to_string(),
            "model" => {
                let v = v.trim_matches('"').to_string();
                if !v.is_empty() {
                    def.model = Some(v);
                }
            }
            "provider" => {
                let v = v.trim_matches('"').to_string();
                if !v.is_empty() {
                    def.provider = Some(v);
                }
            }
            "reasoning" => {
                let v = v.trim_matches('"').to_string();
                if !v.is_empty() {
                    def.reasoning = Some(v);
                }
            }
            "tools" => {
                // Inline list [a, b] or comma-separated.
                let inner = v.trim_start_matches('[').trim_end_matches(']');
                def.tools = inner
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    if in_fm || !fm_done {
        // Degenerate frontmatter (no closing fence): keep the name but
        // treat everything as body — don't lose content.
        body_lines.push(raw.as_str());
    } else {
        body_lines.extend(lines);
    }
    Some(finish(def, body_lines))
}

fn finish(mut def: AgentDef, body_lines: Vec<&str>) -> AgentDef {
    def.context = body_lines.join("\n").trim().to_string();
    def
}

/// Discover all agent definitions (project overrides personal).
pub fn discover() -> Vec<AgentDef> {
    let mut by_name: HashMap<String, AgentDef> = HashMap::new();
    for root in roots() {
        let dir = root.join("agents");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(a) = parse_file(&p) {
                by_name.insert(a.name.clone(), a); // later root wins
            }
        }
    }
    let mut v: Vec<AgentDef> = by_name.into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn roots() -> Vec<PathBuf> {
    // personal first, project LAST (insert order makes the last root
    // win — the documented "project overrides personal").
    vec![crate::config::config_dir(), PathBuf::from(".veles")]
}

/// Find one definition by name.
pub fn find(name: &str) -> Option<AgentDef> {
    discover().into_iter().find(|a| a.name == name)
}

/// One-line listing for /agents surfaces.
pub fn listing() -> String {
    let defs = discover();
    if defs.is_empty() {
        return crate::i18n::lookup("agents_none", &[]);
    }
    let mut out = crate::i18n::lookup("agents_header", &[]);
    out.push('\n');
    for a in &defs {
        out.push_str(&format!("  {}", a.name));
        if !a.description.is_empty() {
            out.push_str(&format!(" — {}", a.description));
        }
        let mut opts = Vec::new();
        if let Some(m) = &a.model {
            opts.push(format!("model={m}"));
        }
        if let Some(p) = &a.provider {
            opts.push(format!("provider={p}"));
        }
        if let Some(r) = &a.reasoning {
            opts.push(format!("reasoning={r}"));
        }
        if !opts.is_empty() {
            out.push_str(&format!(" ({})", opts.join(", ")));
        }
        out.push('\n');
    }
    out
}

/// Render the child prompt for a preset run: preset context first, then
/// whatever background the caller passed, then the goal. Returns
/// (prompt, model, provider, reasoning).
pub fn render_child(
    def: &AgentDef,
    user_context: &str,
    goal: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let mut prompt = String::new();
    if !def.context.is_empty() {
        prompt.push_str(&format!(
            "AGENT PROFILE ({}, you are this subagent):\n{}\n\n",
            def.name, def.context
        ));
    }
    if !user_context.is_empty() {
        prompt.push_str(&format!("BACKGROUND / CONTEXT:\n{user_context}\n\n"));
    }
    prompt.push_str(&format!("TASK:\n{goal}\n\n"));
    prompt.push_str(
        "You are a subagent: finish the task and end with a final summary. \
         You cannot spawn further subagents and cannot ask the user questions.",
    );
    (
        prompt,
        def.model.clone(),
        def.provider.clone(),
        def.reasoning.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests below mutate PROCESS-GLOBAL state (cwd + VELES_HOME) —
    /// serialize against each other AND the memory/prompt tests via
    /// memory's shared lock.
    /// These tests `set_current_dir` into a temp dir and then delete it.
    /// Without restoring the cwd first, the whole process (and every
    /// child it spawns, e.g. git) is left in a path that no longer
    /// exists — which broke memory:: tests in a different module.
    /// `testenv::guard` restores it on drop.
    fn global_lock() -> crate::testenv::EnvGuard {
        crate::testenv::guard()
    }

    fn setup(dir: &std::path::Path, home_rel: &str) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir.join(".veles/agents")).unwrap();
        std::fs::create_dir_all(dir.join(home_rel).join("agents")).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let _g = global_lock();
        let dir = std::env::temp_dir().join(format!(
            "veles-agents-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        setup(&dir, "home");
        std::fs::write(
            dir.join(".veles/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews PRs\nmodel: glm-5.3\nreasoning: high\ntools: [read_file, search_files]\n---\n\nYou are meticulous. Check layering first.\n",
        )
        .unwrap();
        std::env::set_current_dir(&dir).unwrap();
        std::env::set_var("VELES_HOME", dir.join("home"));
        let defs = discover();
        assert_eq!(defs.len(), 1);
        let a = &defs[0];
        assert_eq!(a.name, "reviewer");
        assert_eq!(a.description, "Reviews PRs");
        assert_eq!(a.model.as_deref(), Some("glm-5.3"));
        assert_eq!(a.reasoning.as_deref(), Some("high"));
        assert_eq!(a.tools, vec!["read_file", "search_files"]);
        assert!(a.context.contains("meticulous"));
        assert!(!a.context.contains("---"));
        drop(_g);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_overrides_personal_and_render_orders_parts() {
        let _g = global_lock();
        let dir = std::env::temp_dir().join(format!(
            "veles-agents-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        setup(&dir, "home");
        std::fs::write(dir.join(".veles/agents/x.md"), "project body").unwrap();
        std::fs::write(
            dir.join("home/agents/x.md"),
            "---\nmodel: m-personal\n---\npersonal body",
        )
        .unwrap();
        std::env::set_current_dir(&dir).unwrap();
        std::env::set_var("VELES_HOME", dir.join("home"));
        assert_eq!(discover().len(), 1);
        let a = find("x").unwrap();
        assert_eq!(a.model, None, "project copy wins whole");
        let (prompt, model, _, _) = render_child(&a, "bg here", "do it");
        assert!(
            prompt.starts_with("AGENT PROFILE")
                && prompt.contains("project body")
                && prompt.contains("BACKGROUND / CONTEXT:\nbg here")
                && prompt.ends_with("cannot ask the user questions."),
            "{prompt}"
        );
        assert_eq!(model, None);
        drop(_g);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn degenerate_frontmatter_keeps_body() {
        let _g = global_lock();
        let dir = std::env::temp_dir().join(format!(
            "veles-agents-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        setup(&dir, "home");
        std::fs::write(
            dir.join("home/agents/bad.md"),
            "---\nmodel: m\nno closing fence\nbody text",
        )
        .unwrap();
        std::env::set_current_dir(&dir).unwrap();
        std::env::set_var("VELES_HOME", dir.join("home"));
        let a = find("bad").unwrap();
        assert!(a.context.contains("body text"), "{}", a.context);
        assert_eq!(a.model, Some("m".into()), "fm keys still parsed");
        drop(_g);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A personal definition is CONFIG — the user writes it — so it
    /// lives beside `commands/` and `skills/` in the config dir. The
    /// roots still said `veles_home()`, which the XDG split made the
    /// DATA dir, so on a split box `~/.config/veles/agents/x.md` was
    /// never found. The tests above could not see it: a forced
    /// VELES_HOME puts config and data in the same place.
    #[test]
    fn a_personal_agent_is_read_from_the_config_dir() {
        let (_c, _d, _g) = crate::testenv::scratch_xdg();
        let cwd = tempfile::tempdir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        let personal = crate::config::config_dir().join("agents");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::write(personal.join("p.md"), "personal body").unwrap();
        // The wrong root, populated too, so the test fails loudly
        // rather than by an empty list.
        let data_agents = crate::config::data_dir().join("agents");
        std::fs::create_dir_all(&data_agents).unwrap();
        std::fs::write(data_agents.join("stray.md"), "data body").unwrap();
        let names: Vec<String> = discover().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["p".to_string()], "config dir only");
    }
}
