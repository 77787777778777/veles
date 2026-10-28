//! System prompt builder — the frozen-snapshot layer.
//!
//! Hermes' hard invariant, replicated: the system prompt is built ONCE per
//! session and never rewritten mid-session (prompt-cache stability).
//! Mid-session memory writes hit disk immediately; the prompt snapshot
//! refreshes on the NEXT session start.

use crate::acl::{Cap, Principal};
use crate::config::VelesConfig;
use crate::memory::{MemoryStore, StoreKind, DELIM};

/// Build the frozen system prompt.
///
/// `principal` gates private context: MEMORY / USER PROFILE / the skills
/// index are injected ONLY when the caller has `memory-read` / `skills`.
/// A guest in a shared IRC channel gets a clean-slate assistant, never
/// the owner's notes. `None` = owner surface (CLI/TUI/desktop) — full
/// injection, zero behavior change for single-user use.
///
/// Uses the global skill selection. Call `build_with_skills` where a
/// session's own choices are known — a per-session skill toggle has to
/// change what the prompt says, or it is not a toggle.
pub fn build(cfg: &VelesConfig, principal: Option<&Principal>) -> String {
    build_with_skills(cfg, principal, &crate::skills::SkillSelection::global())
}

pub fn build_with_skills(
    cfg: &VelesConfig,
    principal: Option<&Principal>,
    skills: &crate::skills::SkillSelection,
) -> String {
    build_for_session(cfg, principal, skills, None)
}

/// [`build_with_skills`] for a known session: the `prompt.build`
/// override point (PLAN-extension-api.md E3) is asked with it, when a
/// plugin overrides the point, over what core (and any module layer)
/// built.
pub fn build_for_session(
    cfg: &VelesConfig,
    principal: Option<&Principal>,
    skills: &crate::skills::SkillSelection,
    session_id: Option<&str>,
) -> String {
    let built = build_layered(cfg, principal, skills);
    if !crate::services::overridden(crate::points::PROMPT) {
        return built;
    }
    let mut disabled: Vec<String> = skills.disabled.clone();
    disabled.extend(
        skills
            .session
            .iter()
            .filter(|(_, on)| !**on)
            .map(|(name, _)| name.clone()),
    );
    disabled.sort();
    disabled.dedup();
    let input = serde_json::json!({
        "session_id": session_id,
        "principal": principal,
        "disabled_skills": disabled,
        "prompt": built,
    });
    match crate::services::ask_blocking(
        crate::points::PROMPT,
        input,
        session_id.map(str::to_string),
    ) {
        Ok(v) => match v["prompt"].as_str() {
            Some(p) => p.to_string(),
            None => built,
        },
        Err(e) => {
            let why = format!(
                "`{}` could not build the prompt ({e}); core's is used",
                crate::points::PROMPT
            );
            tracing::warn!("{why}");
            crate::notes::record_plugin_note(&why, "");
            built
        }
    }
}

/// Core's builder under any module layers (R7): what the
/// `prompt.build` point is handed.
fn build_layered(
    cfg: &VelesConfig,
    principal: Option<&Principal>,
    skills: &crate::skills::SkillSelection,
) -> String {
    // The prompt-builder slot (R7, `layers.rs`): a module's layer wraps
    // or replaces the builder below. Nothing installed — the usual case
    // — is the direct call.
    let layers = crate::layers::prompt_layers();
    if layers.is_empty() {
        return build_default(cfg, principal, skills);
    }
    let cx = crate::layers::PromptCx {
        cfg,
        principal,
        skills,
    };
    let base: &crate::layers::PromptBase =
        &|cx: &crate::layers::PromptCx<'_>| build_default(cx.cfg, cx.principal, cx.skills);
    crate::layers::run_prompt_layers(&layers, &cx, base)
}

/// Core's own builder: the bottom of the prompt chain.
pub fn build_default(
    cfg: &VelesConfig,
    principal: Option<&Principal>,
    skills: &crate::skills::SkillSelection,
) -> String {
    let owner_like = match principal {
        None => true,
        Some(p) => p.has(Cap::All),
    };
    let mem = MemoryStore::new();

    // ── THREE TIERS, ORDERED BY VOLATILITY ───────────────────────────
    //
    // A prompt cache is keyed on a PREFIX, so where a block sits decides
    // how much of the prompt survives when that block changes. hermes
    // has had this right the whole time (`system_prompt.py`: stable /
    // context / volatile, volatile last "so the stable prefix stays
    // reusable") and the veles port flattened it into one blob.
    //
    // Worse, `WHERE YOU ARE` was added on 2026-09-02 at the very TOP —
    // a cwd-dependent fact ahead of memory, skills and everything else,
    // so opening a second project invalidated the entire prefix. That is
    // the opposite of what it should do.
    //
    //   stable   — identity and standing guidance. Same on every machine,
    //              every session, every directory. Shared by ALL sessions.
    //   context  — where we are: cwd, git root, project context file.
    //              Same for every session in a project.
    //   volatile — memory, profile, secrets, skills index. Changes
    //              whenever the user's brain does, which is often.
    //
    // Concatenated stable→context→volatile, the longest common prefix
    // between any two sessions is as long as it can be.
    let mut stable = String::new();
    let mut context = String::new();
    let mut volatile = String::new();

    stable.push_str(
        "You are veles, a personal AI agent. Rust core, Tcl scripting layer. \
You are direct, competent, and honest — you verify claims with tools before \
reporting them, and you say plainly when something is unverified or failed. \
You have persistent memory across sessions (the `memory` tool) and terminal \
access; use them proactively when they prevent the user repeating themselves.\n\n",
    );

    // Multi-user awareness: when the caller is not the owner, say who
    // they are so the model doesn't assume it's talking to the principal
    // (and doesn't leak owner-specific behavior like private plans).
    // Per-principal, so it cannot sit in the shared stable tier.
    // Since the user-envs round it also states WHERE that principal's
    // tool calls run — awareness for the model, authority for the
    // framework (the routing is enforced in the tools, not here).
    if let Some(pr) = principal {
        if !pr.has(Cap::All) {
            let zone_note = if pr.role < crate::acl::Role::Admin {
                "Their terminal, shell, file and code tools run inside a sandbox \
container (results are tagged [env: sandbox <name>], and that name is often \
SHARED with everyone else in this conversation, so treat the workspace as \
communal); the host is off-limits to them and no path they name reaches it."
            } else {
                "Their tools run inside a sandbox container by default (results \
are tagged [env: sandbox <name>]); a call carrying \"host\": true runs on the \
host instead, and \"sandbox\": \"<name>\" runs in a named one."
            };
            // The three things a model keeps rediscovering inside a box
            // by trying Linux-host habits one at a time (PLAN-windows.md
            // §6): what is not there, what does not last, and where the
            // network goes. Read from the config, so the line is as
            // stable as the config is.
            let net = if !cfg.sandbox.network {
                "none"
            } else {
                match crate::user_envs::EnvNet::parse(&cfg.sandbox.user_env_network) {
                    crate::user_envs::EnvNet::None => "none",
                    crate::user_envs::EnvNet::Host => "the host's own, loopback included",
                    crate::user_envs::EnvNet::Isolated => {
                        "outbound only; the host's 127.0.0.1 is not reachable from inside"
                    }
                }
            };
            let box_note = format!(
                "Inside the box: no systemd (nothing to systemctl) and no desktop; apt \
and pip work when the network allows, but what they install lives in the box's own \
layer and lasts only while the box does — /work is the one directory that survives a \
purge or a rebuild; network: {net}."
            );
            context.push_str(&format!(
                "You are currently serving {} (role: {}). You are NOT talking to the \
owner. Do not assume owner preferences apply; do not attempt to access owner \
resources — capability-gated tools will refuse and that refusal is final. {zone_note} \
{box_note}\n\n",
                pr.label(),
                pr.role.as_str()
            ));
        }
    }

    let model = cfg.model.default.clone().unwrap_or_else(|| "unset".into());
    let provider = cfg.model.provider.clone().unwrap_or_else(|| "unset".into());
    context.push_str(&format!(
        "Runtime: model {model} @ {provider} (session-scoped /model can override).\n\n"
    ));

    // WHERE YOU ARE — the context tier's whole reason to exist.
    //
    // Measured on a live turn (COVERAGE 2026-09-02): the user asked to
    // "read flake.nix", the agent's cwd WAS the repo containing it, and
    // the model spent twelve of fifteen tool calls hunting for the file
    // — /etc/nixos first, then an unrelated flake repo read in full
    // (26 KB), then a search across all of $HOME. ~44,000 of that turn's
    // 50,205 characters of tool output were the cost of not knowing the
    // working directory. Memory naming other repos made the absence
    // actively misleading.
    //
    // It belongs HERE and not at the top: every session in this project
    // shares the tier above it, and a different project only diverges
    // from this point rather than from the first byte.
    context.push_str(&format!(
        "════ WHERE YOU ARE ════\n{}\n\n",
        environment_block()
    ));

    // Project context files (AGENTS.md, CLAUDE.md, .veles.md, .hermes.md).
    // First match wins — only one context source is loaded per session.
    // This matches hermes' discovery order. Also cwd-dependent, so it
    // sits beside the directory it was found in.
    if let Some((ctx, source)) = load_project_context() {
        let total = ctx.chars().count();
        let cap = cfg.memory.project_char_limit;
        if cap > 0 && total > cap {
            // Same honesty contract as the memory resident caps: the
            // head rides, a pointer names the exact file, and the
            // elision is stated — never a silent cut mid-instruction.
            let cut = ctx
                .char_indices()
                .nth(cap)
                .map(|(i, _)| i)
                .unwrap_or(ctx.len());
            let elided = total - cap;
            context.push_str(&format!(
                "════ PROJECT CONTEXT [first {cap} of {total} chars] ════\n\
{head}\n\
[…{elided} chars of the project context not shown — read the rest from \
{source} with the read_file tool (raise memory.project_char_limit to resize)]\n\n",
                head = &ctx[..cut],
            ));
        } else {
            context.push_str(&format!(
                "════ PROJECT CONTEXT [{total} chars] ════\n{ctx}\n\n"
            ));
        }
    }

    // Memory blocks (frozen snapshot) — ACL-gated: guests/non-owners
    // without memory-read never see the owner's notes in context.
    if owner_like || principal.map(|p| p.has(Cap::MemoryRead)).unwrap_or(false) {
        // S1: the memory hierarchy — system/ is always in context; the
        // tree index (built from the database) signposts the rest.
        if mem.hierarchy_ready() {
            let tree = mem.tree_index();
            if !tree.trim().is_empty() {
                append_tree(&mut volatile, &tree, cfg.memory.tree_char_limit);
            }
        }
        append_block(
            &mut volatile,
            "MEMORY (your notes)",
            &mem,
            StoreKind::Memory,
            cfg,
        );
        append_block(
            &mut volatile,
            "USER PROFILE (who the user is)",
            &mem,
            StoreKind::User,
            cfg,
        );
        // S4: secret NAMES only (never values) — and only for owner-like
        // callers; guests must not even learn what secrets exist.
        if owner_like {
            if let Some(names) = crate::secrets::prompt_names() {
                volatile.push_str(&format!("\n════ SECRETS ════\n{names}\n"));
            }
        }
    }

    // Skills index (frozen snapshot) — hermes' core value feature.
    // Also ACL-gated (skills index leaks operational knowledge).
    if owner_like || principal.map(|p| p.has(Cap::Skills)).unwrap_or(false) {
        let set = crate::skills::SkillSet::load_selected(skills);
        if !set.skills.is_empty() {
            // `[skills] index_mode` decides how much of the index is
            // resident. "recent" needs to know what has actually been
            // used, which lives in the store — read best-effort: no
            // usage history (or no store yet) just means the mode falls
            // back to its pointer line, never to a silent full dump.
            let recent = recent_skill_names(cfg);
            let block = set.index_for(&cfg.skills, &recent);
            if !block.is_empty() {
                volatile.push_str("\n════ SKILLS ════\n");
                volatile.push_str(&block);
            }
        }
    }

    // Standing guidance: identical on every machine and every session,
    // so it belongs in the STABLE tier. It used to be appended after
    // memory and skills, which put ~11k characters of the most volatile
    // content ahead of text that never changes — the tail of the prompt
    // was invalidated by a memory write for no reason at all.
    stable.push_str(
        "\nTool results are ground truth — trust what they return over your assumptions. \
If a tool call fails, diagnose from its output; don't retry blindly.\n",
    );

    // Tool tiering: with `[tools] tier = "core"` only the everyday set
    // is resident; the rest are discoverable. The model must be told
    // the list it sees is not the whole toolbox — say it where it is
    // read first, not buried in tool_search's own description.
    // `tier = "full"` (everything resident) needs no such note.
    if cfg.tools.tier != "full" {
        stable.push_str(
            "Not every tool is listed here. Call `tool_search` with a capability \
description (e.g. \"web search\", \"browser\", \"image generation\") to find the rest; \
`tool_describe` shows a tool's schema and `tool_call` runs it.\n",
        );
    }

    // Background & delegation awareness. Stated once, in the STABLE tier,
    // because a resident tool the model never thinks to use is as good as
    // absent — the store showed delegate_task at 8 calls and shell/monitor
    // at 0 over six weeks while `terminal` ran 25k times. These four verbs
    // are the ones a model has to be told exist: hand a long command to
    // the background, watch for a condition, and farm independent work out
    // to subagents that report back. Kept short (schema rent is real) and
    // phrased as capability, not procedure — each tool's own description
    // carries the how.
    stable.push_str(
        "\nWORKING IN PARALLEL AND IN THE BACKGROUND: for independent work you can \
farm out, call `delegate_task` (one goal, or a `tasks` batch of up to 3) — each \
subagent runs in its own session and reports back a summary; pass `background:true` \
to keep working while it runs. For a long command, `terminal` with `background:true` \
returns a task id at once — inspect or stop it later with the `shell` tool. To be woken \
when a condition nothing else reports holds, use `monitor`. Completions arrive by \
themselves as [BACKGROUND … COMPLETE] / [MONITOR FIRED] rows — keep working or end your \
turn; never sleep or poll to wait (`shell` action `wait` blocks when you need a result \
inside the turn).\n",
    );

    // The reply attachment convention, stated once so the model stops
    // guessing. USER: "the png image didnt render, the model didnt know
    // how to inline it" — it was writing markdown ![](…) syntax, which
    // this renderer does not inline; the marker is the only thing that
    // does. And the reader it must use for the reverse direction.
    stable.push_str(
        "\nATTACHING A FILE TO A REPLY: write `MEDIA:` + an absolute path on its own \
line (one per line), e.g. `MEDIA:/home/user/pic.png`. The desktop renders PNG/GIF \
inline. Do NOT use markdown image syntax.\n\
WHAT CAN BE DELIVERED: on a chat surface (IRC, telegram) a marker only carries a \
file that lives in the engine's media directory — the one `generate_image` and \
`text_to_speech` already write into — or in the sender's own sandbox workspace. \
Anything else is replaced with a note that media was withheld, for every sender \
below the owner, because a path in a reply is a file handed to whoever is \
reading. So write outputs into the media directory rather than /tmp, and say so \
plainly if a file you made cannot be delivered.\n\
To READ an image the user attached or named, call the `vision_analyze` tool with \
the path — that tool can actually see images, your text model cannot.\n",
    );

    // stable → context → volatile. The order IS the optimisation.
    format!("{stable}{context}{volatile}")
}

/// The three prompt tiers, separately, for anything that needs to reason
/// about cache reuse rather than just read the prompt.
///
/// `doctor --prefix` reports their sizes: a reader has to be able to see
/// how much of the prompt survives a memory write or a change of
/// directory, and that is not visible in one concatenated blob.
pub fn tier_sizes(cfg: &VelesConfig, principal: Option<&Principal>) -> (usize, usize, usize) {
    // Rebuilt rather than cached: this runs from `doctor`, not the hot
    // path, and a second source of truth for the tier contents is
    // exactly the kind of duplicate that goes stale.
    let full = build(cfg, principal);
    let ctx_at = full.find("Runtime: model").unwrap_or(full.len());
    let vol_at = full
        .find("\n════ MEMORY TREE")
        .or_else(|| full.find("\n════ MEMORY (your notes)"))
        .or_else(|| full.find("\n════ SKILLS"))
        .unwrap_or(full.len());
    let vol_at = vol_at.max(ctx_at);
    (ctx_at, vol_at - ctx_at, full.len() - vol_at)
}

/// Load a project context file from the current working directory.
///
/// Discovery order (first match wins):
///   1. `.veles.md`  (veles-native)
///   2. `AGENTS.md`  (universal agent convention)
///   3. `CLAUDE.md`  (Claude Code convention)
///   4. `.hermes.md` (hermes convention)
///   5. `SOUL.md`    (persona/instructions)
///
/// This mirrors hermes' project-context-file behavior.
/// Inject the memory tree under its RESIDENT cap, with the same
/// honesty contract as the other resident caps: beyond
/// `tree_char_limit` the prompt carries the head plus a pointer to
/// `memory search` — the signpost's job is pointers, not payload, and
/// a wide hierarchy (post-curate: one file per topic) must not quietly
/// become the biggest block in the prompt. 0 = inject whole.
fn append_tree(p: &mut String, tree: &str, cap: usize) {
    let total = tree.chars().count();
    let title = "MEMORY TREE (system/ is in context; other files load on demand)";
    if cap == 0 || total <= cap {
        p.push_str(&format!("\n════ {title} ════\n{tree}\n"));
        return;
    }
    let cut = tree
        .char_indices()
        .nth(cap)
        .map(|(i, _)| i)
        .unwrap_or(tree.len());
    let elided = total - cap;
    p.push_str(&format!(
        "\n════ {title} [first {cap} of {total} chars] ════\n\
{head}\n\
[…{elided} chars of the memory tree not shown — it has {total} chars total; \
search the hierarchy with the `memory` tool: action=search query=<subject>]\n",
        head = &tree[..cut],
    ));
}

fn load_project_context() -> Option<(String, String)> {
    let candidates = [
        ".veles.md",
        "AGENTS.md",
        "CLAUDE.md",
        ".hermes.md",
        "SOUL.md",
    ];

    for name in &candidates {
        let path = std::path::Path::new(name);
        if path.exists() && path.is_file() {
            if let Ok(content) = std::fs::read_to_string(path) {
                let content = content.trim();
                if !content.is_empty() {
                    return Some((content.to_string(), name.to_string()));
                }
            }
        }
    }
    None
}

/// Inject one memory block under its RESIDENT cap.
///
/// `system/core.md` and `system/user.md` grew to be injected whole —
/// 45k + 15k chars in every prompt — defeating the on-demand hierarchy
/// the tree index was built for. Beyond `[memory] core_char_limit` /
/// `user_resident_char_limit` the prompt carries the head PLUS a
/// pointer naming the exact path and the `memory` tool call that reads
/// the rest — never a silent truncation. A cap of 0 disables capping
/// (and `core_char_limit = 66000` etc. restores the inject-everything
/// behavior exactly — the one-line rollback).
fn append_block(
    p: &mut String,
    title: &str,
    mem: &MemoryStore,
    kind: StoreKind,
    cfg: &VelesConfig,
) {
    let raw = mem.read(kind);
    if raw.trim().is_empty() {
        return;
    }
    let total = raw.chars().count();
    let entries = raw.split(DELIM).count();
    let cap = match kind {
        StoreKind::Memory => cfg.memory.core_char_limit,
        StoreKind::User => cfg.memory.user_resident_char_limit,
    };
    if cap == 0 || total <= cap {
        p.push_str(&format!(
            "════ {title} [{entries} entries, {total} chars] ════\n{raw}\n"
        ));
        return;
    }
    // Cut on a char boundary.
    let cut = raw
        .char_indices()
        .nth(cap)
        .map(|(i, _)| i)
        .unwrap_or(raw.len());
    let head = &raw[..cut];
    let path = kind.hier_path();
    let elided = total - cap;
    p.push_str(&format!(
        "════ {title} [{entries} entries, first {cap} of {total} chars] ════\n\
{head}\n\
[…{elided} chars of {path} not shown — read the rest with the `memory` tool: \
action=read path={path}]\n"
    ));
}

/// Skills used in the last `index_recent_sessions` sessions, for the
/// `recent` index mode.
///
/// Opens the store read-only and on its own — the prompt builder has no
/// store handle, and threading one through every caller to serve one
/// optional block is a worse trade than a single query here. Any
/// failure (no database yet, a locked file, a fresh install) returns
/// empty, which degrades to the pointer line rather than to a silent
/// full index.
/// The skill names the `recent` index mode seats first, read from the
/// store. Public so `veles doctor` can report the same signal the prompt
/// builder uses rather than re-deriving it (and drifting).
pub fn recent_skill_names(cfg: &VelesConfig) -> Vec<String> {
    if cfg.skills.index_mode.trim() != "recent" {
        return Vec::new();
    }
    let db = crate::config::veles_home().join("state.db");
    if !db.exists() {
        return Vec::new();
    }
    crate::store::Store::open(&db)
        .and_then(|s| s.recent_skill_names(cfg.skills.index_recent_sessions))
        .unwrap_or_default()
}

/// Blocks truncated to their resident cap, as (title, shown, total),
/// read off the BUILT prompt — the Memory note reports the elision, so
/// a capped block is visible in the transcript as well as in the
/// prompt.
pub fn truncated_blocks(prompt: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    for line in prompt.lines() {
        let t = line.trim();
        if !(t.starts_with('\u{2550}') && t.ends_with('\u{2550}')) {
            continue;
        }
        let Some(first) = t.find("first ") else {
            continue;
        };
        let Some(of) = t[first..].find(" of ") else {
            continue;
        };
        let shown: usize = t[first + 6..first + of].trim().parse().unwrap_or(0);
        let rest = &t[first + of + 4..];
        let Some(end) = rest.find(" chars") else {
            continue;
        };
        let total: usize = rest[..end].trim().parse().unwrap_or(0);
        if shown == 0 || total == 0 {
            continue;
        }
        let title = t
            .trim_matches('\u{2550}')
            .trim()
            .split('[')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        out.push((title, shown, total));
    }
    out
}

/// The working directory and what kind of place it is.
///
/// Deliberately short and deliberately STABLE for the session: every
/// line here is re-sent on every round, and anything that can change
/// mid-session (a branch, a dirty flag, a clock) would be a prefix
/// regression wearing a helpful hat. The prompt is frozen at session
/// start anyway, so a moving value would go stale rather than update.
pub fn environment_block() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into());
    let mut s = format!("Working directory: {cwd}");
    // A repo root is the single most useful extra fact: it tells the
    // model that a bare filename in the request most likely means THIS
    // tree, not some other one it remembers.
    if let Some(root) = git_root(std::path::Path::new(&cwd)) {
        s.push_str(&format!("\nGit repository root: {}", root.display()));
    }
    s.push_str(
        "\nA relative path, or a bare filename the user mentions, resolves here \
unless they say otherwise — look here FIRST before searching elsewhere.",
    );
    // What this node is (PLAN-windows.md §6): the platform, the shell the
    // terminal tool runs, and what a model used to Linux will not find
    // here. Fixed for the process, so it is as stable as the rest.
    s.push('\n');
    s.push_str(&crate::platform::facts().line());
    s
}

/// Walk up for a `.git` (file or directory — worktrees use a file).
fn git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut cur = Some(start);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// What was injected into the system prompt, as (label, chars).
///
/// The "nothing implicit" rule covers the prompt too: memory, the user
/// profile, project context and the skills index all enter the model's
/// view without the user typing them, and the amount matters — a bloated
/// MEMORY block is a real cause of a turn behaving oddly. This reads the
/// BUILT prompt rather than duplicating the build logic, so it cannot
/// drift from what was actually sent.
pub fn injected_blocks(prompt: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    // Blocks are delimited by a "════ TITLE ════" banner line.
    let mut current: Option<(String, usize)> = None;
    for line in prompt.lines() {
        let t = line.trim();
        if t.starts_with('\u{2550}') && t.ends_with('\u{2550}') {
            if let Some(done) = current.take() {
                out.push(done);
            }
            let title = t.trim_matches('\u{2550}').trim();
            // "MEMORY (your notes) [1234 chars]" -> "MEMORY (your notes)"
            let title = title.split('[').next().unwrap_or(title).trim();
            if !title.is_empty() {
                current = Some((title.to_string(), 0));
            }
            continue;
        }
        if let Some((_, ref mut n)) = current {
            *n += line.chars().count() + 1;
        }
    }
    if let Some(done) = current.take() {
        out.push(done);
    }
    out
}

#[cfg(test)]
mod tests {
    /// The prompt-builder slot (R7): `build_with_skills` runs a
    /// module's layer over core's builder, and a purge of the module
    /// leaves core's prompt alone again. The layer acts only on a probe
    /// model name: the slot is process-global.
    #[test]
    fn a_prompt_layer_wraps_the_builder_and_a_purge_restores_it() {
        use crate::layers::{NextPrompt, PromptCx, PromptLayer};
        struct Probe;
        impl PromptLayer for Probe {
            fn build(&self, cx: &PromptCx<'_>, next: NextPrompt<'_>) -> String {
                let base = next.build(cx);
                if cx.cfg.model.default.as_deref() == Some("layer-probe-model") {
                    format!("{base}\n\nPROBE LAYER LINE")
                } else {
                    base
                }
            }
        }
        let (_home, _guard) = crate::testenv::scratch_home();
        let mut cfg = VelesConfig::default();
        cfg.model.default = Some("layer-probe-model".into());
        let skills = crate::skills::SkillSelection::default();
        let plain = build_with_skills(&cfg, None, &skills);
        assert!(!plain.contains("PROBE LAYER LINE"));

        let prev = crate::lifecycle::thread_owner();
        crate::lifecycle::set_thread_owner("module:prompt-probe");
        crate::layers::install_prompt(std::sync::Arc::new(Probe));
        crate::lifecycle::clear_thread_owner_for_test(prev);
        let layered = build_with_skills(&cfg, None, &skills);
        assert!(layered.ends_with("PROBE LAYER LINE"), "{layered}");
        assert!(
            layered.starts_with(&plain),
            "a wrapping layer builds on core's prompt"
        );

        assert_eq!(crate::layers::purge_owner(Some("module:prompt-probe")), 1);
        assert_eq!(build_with_skills(&cfg, None, &skills), plain);
    }

    use super::*;

    /// The tier order IS the optimisation, so it gets an assertion.
    ///
    /// hermes has had this right the whole time and the veles port
    /// flattened it. Worse, `WHERE YOU ARE` went in at the very top on
    /// 2026-09-02 — a cwd-dependent fact ahead of everything — so
    /// opening a second project threw away the entire cached prefix.
    /// Least volatile first, most volatile last.
    #[test]
    fn the_prompt_is_ordered_least_volatile_first() {
        let p = build(&VelesConfig::default(), None);
        let at = |needle: &str| p.find(needle);

        // Stable: identity and the standing guidance that never varies.
        let identity = at("You are veles").expect("identity");
        let ground_truth = at("Tool results are ground truth").expect("guidance");
        let media = at("ATTACHING A FILE TO A REPLY").expect("conventions");
        // Context: this machine, this directory.
        let runtime = at("Runtime: model").expect("runtime line");
        let where_you_are = at("WHERE YOU ARE").expect("environment block");

        assert!(identity < ground_truth, "identity leads");
        assert!(
            ground_truth < runtime && media < runtime,
            "standing guidance is STABLE and must precede the context tier — \
             it used to be appended after memory, so a memory write \
             invalidated text that never changes"
        );
        assert!(
            runtime < where_you_are,
            "the environment block belongs in the context tier"
        );

        // Volatile last: whichever of the private blocks exist must all
        // come after the context tier. (A scratch home may have none.)
        for vol in [
            "════ MEMORY TREE",
            "════ MEMORY (your notes)",
            "════ USER PROFILE",
            "════ SECRETS",
            "════ SKILLS",
        ] {
            if let Some(i) = at(vol) {
                assert!(
                    i > where_you_are,
                    "{vol} is volatile and must come after the context tier, \
                     or a memory write throws away the cwd block too"
                );
            }
        }
    }

    /// The delivery contract is stated in the prompt because a model that does
    /// not know it writes its output to `/tmp`, attaches it, and then reports a
    /// file it could never send. The gate in `media::deliverable` holds either
    /// way — this is the half that keeps the gate from being exercised on
    /// every turn. Pinned so a prompt tidy-up cannot drop it silently.
    #[test]
    fn the_prompt_states_what_can_actually_be_delivered() {
        let p = build(&VelesConfig::default(), None);
        let start = p
            .find("ATTACHING A FILE TO A REPLY")
            .expect("the convention block");
        let end = start
            + p[start..]
                .find("To READ an image")
                .expect("the reader half follows it");
        let section = &p[start..end];

        assert!(section.contains("WHAT CAN BE DELIVERED"), "{section}");
        assert!(
            section.contains("media directory"),
            "the model has to be told where deliverable files live: {section}"
        );
        assert!(
            section.contains("withheld"),
            "and what the note it will see back means: {section}"
        );
        assert!(
            section.contains("below the owner"),
            "the contract is about the recipient, not the file: {section}"
        );
    }

    /// The awareness line that makes delegation and background work
    /// reachable IN PRACTICE. Promoting delegate_task/shell/monitor to
    /// the core tier puts their schemas in the array, but the store showed
    /// a resident-tier model still not thinking to use them — so the
    /// prompt names the capability once, in the stable tier. Pinned
    /// against a prompt tidy-up dropping it silently: the tools are named,
    /// and the line is stable (same in every session, so it cannot sit in
    /// a volatile tier where a memory write would evict it).
    #[test]
    fn the_prompt_names_delegation_and_background_work() {
        let p = build(&VelesConfig::default(), None);
        let section = p
            .find("WORKING IN PARALLEL AND IN THE BACKGROUND")
            .map(|i| &p[i..])
            .expect("the awareness block");
        for tool in ["delegate_task", "terminal", "shell", "monitor"] {
            assert!(
                section.contains(tool),
                "the awareness line must name `{tool}`: {section}"
            );
        }
        assert!(
            section.contains("background:true"),
            "and say how to hand work to the background: {section}"
        );
        // It is STABLE: it must precede the context tier (the runtime
        // line), or a per-session/per-machine fact would evict it from the
        // cacheable prefix for no reason.
        let awareness = p.find("WORKING IN PARALLEL").unwrap();
        let runtime = p.find("Runtime: model").unwrap();
        assert!(
            awareness < runtime,
            "the awareness line is stable and must precede the context tier"
        );
    }

    /// Two sessions in DIFFERENT directories must still share every byte
    /// of the stable tier — that is the whole reason it is a tier.
    #[test]
    fn a_change_of_directory_only_invalidates_from_the_context_tier() {
        // This test CHANGES the process-wide cwd, and until 2026-09-03
        // it did so holding nothing — the one unguarded mutator left in
        // the crate. It raced every test that reads the cwd: two prompt
        // tests assert the prompt names it and one asserts two builds
        // are byte-identical, and all three failed intermittently under
        // a full `cargo test`. `guard()` also restores the cwd if the
        // body panics, which the manual "cd back" below cannot.
        let _g = crate::testenv::guard();
        let cfg = VelesConfig::default();
        let here = std::env::current_dir().expect("cwd");
        let a = build(&cfg, None);
        let tmp = std::env::temp_dir();
        std::env::set_current_dir(&tmp).expect("cd");
        let b = build(&cfg, None);
        std::env::set_current_dir(&here).expect("cd back");

        let shared = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
        let stable_end = a.find("Runtime: model").expect("context tier starts");
        assert!(
            shared >= stable_end,
            "two directories shared only {shared} bytes but the stable tier is \
             {stable_end} — the cwd is leaking into the stable prefix"
        );
    }
    /// USER's turn that prompted this: "read flake.nix" with the agent
    /// already IN the repo containing it. Twelve of fifteen tool calls
    /// went on finding the file — /etc/nixos first, then an unrelated
    /// flake repo read in full, then a search across all of $HOME.
    /// Nothing in the prompt said where the agent was standing.
    #[test]
    fn the_prompt_says_where_the_agent_is_standing() {
        // Reads the process cwd and asserts the prompt names it, so it
        // cannot run while another test is standing somewhere else.
        let _g = crate::testenv::lock();
        let p = build(&VelesConfig::default(), None);
        assert!(p.contains("WHERE YOU ARE"), "the block is present");
        let cwd = std::env::current_dir().unwrap().display().to_string();
        assert!(p.contains(&cwd), "the actual working directory is named");
        assert!(
            p.contains("resolves here"),
            "and says a bare filename means HERE first"
        );
    }

    /// It has to be CHEAP, or it is just more prefix. The whole case is
    /// twenty cached tokens against twelve wasted rounds.
    #[test]
    fn the_environment_block_stays_small_and_stable() {
        // "byte-identical between two calls" is only true if nobody
        // moves the cwd between them.
        let _g = crate::testenv::lock();
        let b = environment_block();
        // 400 on Linux, where the platform line is one short sentence;
        // Windows and Android name what is absent and what differs
        // (PLAN-windows.md §6), which is the one place a longer line
        // pays for itself in avoided wrong turns.
        let cap = if cfg!(target_os = "linux") { 400 } else { 800 };
        assert!(
            b.len() < cap,
            "the block must stay tiny — it rides every round: {} chars",
            b.len()
        );
        // Stable within a session is the whole cache contract: two
        // builds in the same process must be byte-identical, or the
        // prefix moves under us.
        assert_eq!(b, environment_block(), "must not vary between calls");
        // Nothing time-shaped, nothing that a commit would change.
        for forbidden in ["branch", ":00", "dirty", "HEAD"] {
            assert!(
                !b.contains(forbidden),
                "`{forbidden}` can change mid-session and would move the prefix: {b}"
            );
        }
    }
    #[test]
    fn test_build_includes_basic_info() {
        let cfg = VelesConfig::default();
        let prompt = build(&cfg, None);
        assert!(prompt.contains("You are veles"));
    }

    #[test]
    fn test_load_project_context_finds_file() {
        // set_current_dir is PROCESS-wide: while this test holds the cwd
        // at /tmp, every other test resolving a relative path or shelling
        // out sees it too. That is what made the memory:: tests fail
        // intermittently under load. Serialize on the shared lock.
        let _g = crate::testenv::lock();
        // Create a temp .veles.md in cwd, check it loads
        let tmp = std::env::temp_dir();
        let prev = std::env::current_dir().unwrap_or_default();
        std::env::set_current_dir(&tmp).ok();
        std::fs::write(".veles_test_ctx.md", "# Test\nContext here").ok();

        // The function checks specific filenames, not _test — so test
        // by writing an actual AGENTS.md
        std::fs::write("AGENTS.md", "# Test Project\nInstructions here").ok();
        let ctx = load_project_context();
        assert!(ctx.is_some());
        let (body, name) = ctx.expect("loaded");
        assert!(body.contains("Test Project"));
        assert_eq!(name, "AGENTS.md");
        std::fs::remove_file("AGENTS.md").ok();
        std::fs::remove_file(".veles_test_ctx.md").ok();
        std::env::set_current_dir(prev).ok();
    }

    #[test]
    fn test_guest_prompt_has_no_private_context() {
        // The S15 privacy gate: a guest principal's prompt must NOT
        // contain the memory/user blocks even when files exist. We
        // can't easily seed the real VELES_HOME here, so assert on the
        // STRUCTURE: the guest prompt lacks the block headers, and the
        // prompt says who it's serving.
        let cfg = VelesConfig::default();
        let guest = crate::acl::Principal::guest();
        let p = build(&cfg, Some(&guest));
        assert!(
            !p.contains("════ MEMORY"),
            "guest must not get memory block"
        );
        assert!(
            !p.contains("════ USER PROFILE"),
            "guest must not get user profile"
        );
        assert!(
            !p.contains("════ SKILLS"),
            "guest must not get skills index"
        );
        assert!(
            p.contains("NOT talking to the owner"),
            "guest prompt names the caller"
        );
    }

    #[test]
    fn test_owner_prompt_unchanged() {
        // Owner (and None = owner surfaces): the pre-ACL prompt shape.
        // Take the shared env lock — memory tests mutate VELES_HOME.
        let _g = crate::testenv::lock();
        let cfg = VelesConfig::default();
        let owner = crate::acl::Principal::owner();
        let p = build(&cfg, Some(&owner));
        assert!(!p.contains("NOT talking to the owner"));
        let p2 = build(&cfg, None);
        assert!(!p2.contains("NOT talking to the owner"));
        // Both follow the owner code path (memory + skills injected).
        // NOT asserting p == p2: sibling test modules set VELES_HOME
        // under their own locks, so the memory snapshot read can differ
        // between the two calls. The equivalence is structural: neither
        // gets the guest marker, both get the base identity.
        for prompt in [&p, &p2] {
            assert!(prompt.contains("You are veles"));
            assert!(!prompt.contains("NOT talking to the owner"));
        }
    }
    /// The prompt inspector backs the Memory engine note: what entered
    /// the model's view without the user typing it, and how much of it.
    #[test]
    fn injected_blocks_reports_each_banner_and_its_size() {
        let prompt = "You are veles.\n\
                      \u{2550}\u{2550} PROJECT CONTEXT [12 chars] \u{2550}\u{2550}\n\
                      hello\n\
                      \u{2550}\u{2550} MEMORY (your notes) \u{2550}\u{2550}\n\
                      one\n\
                      two\n";
        let got = injected_blocks(prompt);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["PROJECT CONTEXT", "MEMORY (your notes)"],
            "each banner is a block, and the [N chars] suffix is not part of the name"
        );
        // "hello\n" = 6
        assert_eq!(got[0].1, 6, "{got:?}");
        // "one\n" + "two\n" = 8
        assert_eq!(got[1].1, 8, "{got:?}");

        // The preamble before the first banner belongs to no block.
        assert!(
            !names.contains(&"You are veles."),
            "prose before the first banner is not a block"
        );
    }

    #[test]
    fn injected_blocks_is_empty_without_banners() {
        assert!(injected_blocks("just a plain prompt, nothing injected").is_empty());
    }

    // ── resident memory caps (Step 2.2) ─────────────────────────────

    /// A pinned store holding an oversized `system/core.md`, plus the
    /// exact text that landed in it. Pinned with `at()` (not `new()`)
    /// for the reason that function's doc comment gives.
    fn oversized_core(home: &std::path::Path) -> (MemoryStore, String) {
        let s = MemoryStore::at(home.to_path_buf());
        s.ensure_hierarchy().expect("hierarchy");
        // Each note ENDS with a unique marker. Uniform padding made the
        // "is the tail resident?" assertion vacuous: a tail of pure
        // filler also occurs inside the head, so `contains` passed on a
        // correctly truncated prompt.
        let notes: Vec<String> = (0..40)
            .map(|i| format!("note {i}: {} end-of-note-{i}", "x".repeat(200)))
            .collect();
        s.write(StoreKind::Memory, &notes).expect("seed core");
        let raw = s.read(StoreKind::Memory);
        (s, raw)
    }

    /// Over the cap the prompt keeps the head, drops the tail, and
    /// SAYS it dropped the tail. A silent cut is the one outcome that
    /// is worse than paying for the tokens: the model would read a
    /// partial memory as a complete one.
    #[test]
    fn an_oversized_core_is_truncated_with_a_pointer_not_silently() {
        let (home, _env) = crate::testenv::scratch_home();
        let (mem, raw) = oversized_core(home.path());
        let mut cfg = VelesConfig::default();
        cfg.memory.core_char_limit = 500;

        let mut p = String::new();
        append_block(&mut p, "MEMORY (your notes)", &mem, StoreKind::Memory, &cfg);

        let total = raw.chars().count();
        assert!(
            total > 500,
            "the fixture must exceed the cap ({total} chars)"
        );

        let head: String = raw.chars().take(500).collect();
        assert!(p.contains(&head), "the head stays resident");
        let tail: String = raw.chars().skip(total - 100).collect();
        assert!(!p.contains(&tail), "the tail must NOT be resident");
        assert!(
            !p.contains("end-of-note-39"),
            "nor the last note's own marker"
        );

        assert!(p.contains("not shown"), "the elision is stated: {p:?}");
        assert!(p.contains("system/core.md"), "it names the file");
        assert!(p.contains("action=read"), "and the way to read the rest");
        assert!(
            p.contains(&format!("first 500 of {total} chars")),
            "the banner carries the arithmetic: {p:?}"
        );
        assert_eq!(
            truncated_blocks(&p),
            vec![("MEMORY (your notes)".to_string(), 500, total)],
            "and the note reads that arithmetic back off the built prompt"
        );
    }

    /// Under the cap, nothing changes at all — the historical banner,
    /// byte for byte. `core_char_limit = 0` is the documented "no cap"
    /// escape hatch and must behave identically on a huge file.
    #[test]
    fn under_the_cap_and_at_cap_zero_the_block_is_untouched() {
        let (home, _env) = crate::testenv::scratch_home();
        let (mem, raw) = oversized_core(home.path());
        let total = raw.chars().count();

        let mut big = VelesConfig::default();
        big.memory.core_char_limit = total + 1;
        let mut a = String::new();
        append_block(&mut a, "MEMORY (your notes)", &mem, StoreKind::Memory, &big);

        let mut off = VelesConfig::default();
        off.memory.core_char_limit = 0;
        let mut b = String::new();
        append_block(&mut b, "MEMORY (your notes)", &mem, StoreKind::Memory, &off);

        assert_eq!(a, b, "cap 0 == cap larger than the file");
        assert!(a.contains(&raw), "the whole file is resident");
        assert!(!a.contains("not shown"), "and nothing claims otherwise");
        assert!(
            truncated_blocks(&a).is_empty(),
            "an uncapped block is not reported as capped"
        );
    }

    /// The pointer is an INSTRUCTION the model will act on. If the path
    /// it names drifts from what the `memory` tool accepts, the prompt
    /// is telling the model to make a call that fails — so round-trip
    /// the path through the real tool rather than asserting a literal.
    #[tokio::test]
    async fn the_pointer_names_a_path_the_memory_tool_accepts() {
        let (home, _env) = crate::testenv::scratch_home();
        let (mem, raw) = oversized_core(home.path());
        let mut cfg = VelesConfig::default();
        cfg.memory.core_char_limit = 500;
        let mut p = String::new();
        append_block(&mut p, "MEMORY (your notes)", &mem, StoreKind::Memory, &cfg);

        // Parse the path back OUT of the text the model reads.
        let marker = "action=read path=";
        let at = p.find(marker).expect("the pointer names a read call");
        let path: String = p[at + marker.len()..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ']')
            .collect();
        assert_eq!(path, "system/core.md");

        let tool = crate::memory::MemoryTool {
            session_id: "resident-cap-test".into(),
        };
        let out = crate::tools::Tool::call(
            &tool,
            serde_json::json!({ "target": "memory", "action": "read", "path": path }),
        )
        .await;

        assert!(
            !out.starts_with("usage:") && !out.contains("no such file"),
            "the tool accepts the path the prompt hands the model: {out}"
        );
        let tail: String = raw.chars().skip(raw.chars().count() - 60).collect();
        assert!(
            out.contains(&tail),
            "and it returns exactly what the prompt elided: {out:.300}"
        );
    }

    // ── project-context resident cap (Bill 1) ────────────────────────

    /// A scratch project whose AGENTS.md is over the default resident
    /// cap. Pinned with unique tail markers (uniform filler would make
    /// "the tail is gone" vacuously true).
    fn oversized_project() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut content = String::new();
        for i in 0..70 {
            content.push_str(&format!("rule {i}: {} END-MARKER-{i}\n", "x".repeat(190)));
        }
        let total = content.trim().chars().count();
        assert!(
            total > 12_000,
            "fixture must exceed the default cap {total}"
        );
        std::fs::write(dir.path().join("AGENTS.md"), content.trim()).expect("write");
        let path = dir.path().to_path_buf();
        (dir, path, content.trim().to_string())
    }

    /// The one uncapped injection left: a project-context file rode in
    /// WHOLE no matter its size. Now it gets the same resident cap and
    /// the same honesty the memory blocks have — head rides, the elision
    /// is stated, and the pointer names the file. Silent truncation is
    /// the one outcome worse than paying for the tokens: the model
    /// would read a partial AGENTS.md as a complete one.
    #[test]
    fn an_oversized_project_context_is_truncated_with_a_pointer_not_silently() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_dir, path, content) = oversized_project();
        let total = content.chars().count();
        std::env::set_current_dir(&path).expect("chdir");

        let cfg = VelesConfig::default();
        assert_eq!(
            cfg.memory.project_char_limit, 12_000,
            "this test pins the default resident share"
        );
        let p = build(&cfg, None);

        let head: String = content.chars().take(12_000).collect();
        assert!(p.contains(&head), "the head stays resident");
        assert!(
            !p.contains("END-MARKER-69"),
            "the tail must NOT be resident"
        );
        assert!(p.contains("not shown"), "the elision is stated");
        assert!(p.contains("AGENTS.md"), "it names the file");
        assert!(p.contains("read_file"), "and the tool that reads the rest");
        assert!(
            p.contains(&format!("first 12000 of {total} chars")),
            "the banner carries the arithmetic"
        );
        assert_eq!(
            truncated_blocks(&p),
            vec![("PROJECT CONTEXT".to_string(), 12_000, total)],
            "the note reads that arithmetic back off the built prompt"
        );
    }

    /// Under the cap — nothing changes. Cap 0 — the documented
    /// inject-everything rollback, byte for byte.
    #[test]
    fn under_the_cap_and_at_cap_zero_the_project_context_is_untouched() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (dir, path, content) = oversized_project();
        let total = content.chars().count();
        let _ = dir;
        std::env::set_current_dir(&path).expect("chdir");

        let mut big = VelesConfig::default();
        big.memory.project_char_limit = total + 1;
        let a = build(&big, None);

        let mut off = VelesConfig::default();
        off.memory.project_char_limit = 0;
        let b = build(&off, None);

        assert_eq!(a, b, "cap 0 == cap larger than the file");
        assert!(a.contains(&content), "the whole file is resident");
        assert!(!a.contains("not shown"), "and nothing claims otherwise");
        assert!(
            truncated_blocks(&a).is_empty(),
            "an uncapped project context is not reported as capped"
        );
    }

    // ── memory tree resident cap (Bill 1) ────────────────────────────

    /// A wide hierarchy (post-curate shape: one file per topic) makes
    /// the derived tree index the biggest block in the prompt — pointers
    /// becoming payload. The tree rides under its own resident cap with
    /// the same honesty: head plus a pointer to memory search, never a
    /// silent cut.
    #[test]
    fn an_oversized_tree_is_truncated_with_a_search_pointer_not_silently() {
        let (home, _env) = crate::testenv::scratch_home();
        let s = crate::memory::MemoryStore::at(home.path().to_path_buf());
        s.ensure_hierarchy().unwrap();
        s.write(crate::memory::StoreKind::Memory, &["core note".into()])
            .unwrap();
        for i in 0..12 {
            let body = format!("topic {i}: {} TRAIL-{i}", "y".repeat(600));
            s.write_file(&format!("reference/topics/t{i:02}.md"), &body)
                .unwrap();
        }
        let tree = s.tree_index();
        let total = tree.chars().count();
        assert!(
            total > 4_000,
            "the fixture must exceed the default tree cap: {total}"
        );

        let mut p = String::new();
        append_tree(&mut p, &tree, 4_000);

        let head: String = tree.chars().take(4_000).collect();
        assert!(p.contains(&head), "the head stays resident");
        assert!(!p.contains("TRAIL-11"), "the tail must NOT be resident");
        assert!(p.contains("not shown"), "the elision is stated");
        assert!(p.contains("action=search"), "it names the search call");
        assert!(
            p.contains(&format!("first 4000 of {total} chars")),
            "the banner carries the arithmetic"
        );
        assert_eq!(
            truncated_blocks(&p),
            vec![(
                "MEMORY TREE (system/ is in context; other files load on demand)".to_string(),
                4_000,
                total
            )],
            "the note reads back off the built prompt"
        );
    }

    /// Cap 0 (the rollback) and cap-above-size inject the whole tree.
    #[test]
    fn an_under_cap_tree_is_untouched() {
        let (home, _env) = crate::testenv::scratch_home();
        let s = crate::memory::MemoryStore::at(home.path().to_path_buf());
        s.ensure_hierarchy().unwrap();
        s.write(crate::memory::StoreKind::Memory, &["core note".into()])
            .unwrap();
        for i in 0..4 {
            s.write_file(
                &format!("reference/topics/t{i}.md"),
                &format!("topic {i}: {}", "y".repeat(200)),
            )
            .unwrap();
        }
        let tree = s.tree_index();
        let total = tree.chars().count();
        assert!(total <= 4_000, "fixture must fit: {total}");

        let mut capped = String::new();
        append_tree(&mut capped, &tree, 4_000);
        let mut off = String::new();
        append_tree(&mut off, &tree, 0);
        assert_eq!(capped, off, "cap 0 == cap above the size");
        assert!(capped.contains(&tree), "the whole tree is resident");
        assert!(truncated_blocks(&capped).is_empty());
    }
}
