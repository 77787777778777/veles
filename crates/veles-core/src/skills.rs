//! Skills — progressive-disclosure procedural memory (hermes parity).
//!
//! Design copied from hermes' tools/skills_tool.py because it works and
//! the user has ~97 skills in active use there:
//!
//!   - Directory layout: <root>/<category>/<name>/SKILL.md (+ references/,
//!     templates/, scripts/, assets/ for linked files).
//!   - SKILL.md = YAML frontmatter (name ≤64, description ≤1024) + markdown
//!     body. Description doubles as the trigger line in the prompt index.
//!   - Progressive disclosure: the system prompt carries ONLY the index
//!     (name + truncated description); full bodies load via `skill_view`
//!     when the agent decides they're relevant. Linked files load on
//!     demand via `skill_view --file_path`.
//!   - Frozen-snapshot pattern (same invariant as memory): index is built
//!     once per session; mid-session skill_manage writes hit disk
//!     immediately but never rewrite the prompt.
//!
//! Skill roots, in load order (later wins on name conflict):
//!   1. ~/.hermes/skills   (migration compat — read-only access)
//!   2. ~/.config/veles/skills  (veles' own — `config_dir()/skills`,
//!      NOT ~/.veles: that path predates the XDG split and no longer
//!      exists on a migrated machine)

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const NAME_MAX: usize = 64;
const DESC_MAX: usize = 1024;
/// How much of a skill's description the prompt index carries.
///
/// Was 57, inherited from hermes, and it truncated MID-WORD. A
/// description exists to answer one question — "does this apply to what
/// I am doing right now?" — and the trigger clause that answers it is
/// usually at the end, so 57 chars cut off exactly the deciding part.
/// `kdotool`'s 313-char description arrived as "Use for KDE Plasma
/// Wayland window management (activate/mo".
///
/// 200 is measured, not guessed. Across the 134 installed skills the
/// description lengths are p50 56, p75 58, p90 60, max 376: most were
/// authored to fit the old 57-char budget and are unaffected, while the
/// 47 that exceed it are the ones actually carrying trigger text. 200
/// covers through p90 whole and truncates only genuine outliers, for
/// roughly 1.4 KB more prompt across the 30 entries the index shows.
const INDEX_DESC_MAX: usize = 200;
/// `meta` key prefix for a per-session skill override, in the SAME
/// store table `/failover` and `/mechanisms` already use — one
/// persistence mechanism, and `--resume` keeps the choice.
///
/// Key: `skill:<session_id>:<name>` → `"on"` | `"off"`.
pub const META_PREFIX: &str = "skill:";

pub fn meta_key(session_id: &str, name: &str) -> String {
    format!("{META_PREFIX}{session_id}:{name}")
}

/// Which skills a session may see.
///
/// Skills used to be all-or-nothing: `SkillSet::load()` took no session
/// and every discovered skill went into the prompt index. Two levels of
/// choice sit on top of it now — a global `[skills] disabled` list in
/// config.toml, and per-session overrides that can switch an individual
/// skill on or off without touching the file. The session wins, because
/// it is the more specific statement.
#[derive(Debug, Clone, Default)]
pub struct SkillSelection {
    /// Globally disabled (config `[skills] disabled`).
    pub disabled: Vec<String>,
    /// Per-session, from store meta: name → on/off.
    pub session: std::collections::HashMap<String, bool>,
}

/// The selection in force for one session: config's global list plus
/// whatever that session overrode.
///
/// Reads the store directly rather than going through `Agent`, because
/// the skill TOOLS need it and they only hold a session id — the same
/// arrangement `memory`'s and `todo`'s tools already use.
pub fn selection_for(
    store: &std::sync::Mutex<crate::store::Store>,
    session_id: &str,
) -> SkillSelection {
    let mut sel = SkillSelection::global();
    let Ok(store) = store.lock() else {
        return sel;
    };
    let prefix = format!("{META_PREFIX}{session_id}:");
    for key in store.meta_scan_prefix(&prefix) {
        let Some(name) = key.strip_prefix(&prefix) else {
            continue;
        };
        let Some(v) = store.meta_get(&key) else {
            continue;
        };
        sel.session.insert(name.to_string(), v == "on");
    }
    sel
}

impl SkillSelection {
    /// Just the config layer — for surfaces with no session in hand.
    pub fn global() -> Self {
        Self {
            disabled: crate::config::load().unwrap_or_default().skills.disabled,
            session: Default::default(),
        }
    }

    pub fn allows(&self, name: &str) -> bool {
        if let Some(on) = self.session.get(name) {
            return *on;
        }
        !self.disabled.iter().any(|d| d == name)
    }

    pub fn is_empty(&self) -> bool {
        self.disabled.is_empty() && self.session.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub category: String,
    /// Directory containing SKILL.md.
    pub dir: PathBuf,
    /// Which root it came from (hermes-migration vs veles).
    pub root: String,
}

pub struct SkillSet {
    pub skills: Vec<SkillMeta>,
}

impl SkillSet {
    /// Discover skills from both roots.
    pub fn load() -> Self {
        let mut skills = Vec::new();
        let roots = skill_roots();
        for (label, root) in &roots {
            if root.is_dir() {
                discover_in(root, label, "", &mut skills);
            }
        }
        let mut skills = dedup_last_root_wins(skills);
        skills.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
        Self { skills }
    }

    /// Discover, then drop what this selection excludes.
    ///
    /// Filtering happens HERE rather than at each call site so the
    /// prompt index, `skills_list` and `skill_view` cannot disagree —
    /// an agent that can still read a skill the user switched off has
    /// not really had it switched off.
    pub fn load_selected(sel: &SkillSelection) -> Self {
        let mut set = Self::load();
        if !sel.is_empty() {
            set.skills.retain(|s| sel.allows(&s.name));
        }
        set
    }

    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// System-prompt index, hermes style.
    pub fn index(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "Skills are reusable procedures. Scan the index below; load any skill \
even partially relevant to the task with the `skill_view` tool BEFORE acting.\n\n",
        );
        let mut current_cat = String::new();
        for s in &self.skills {
            if s.category != current_cat {
                current_cat = s.category.clone();
                out.push_str(&format!(
                    "{}:\n",
                    if current_cat.is_empty() {
                        "(root)"
                    } else {
                        &current_cat
                    }
                ));
            }
            let desc = truncate_desc(&s.description);
            out.push_str(&format!("  - {}: {}\n", s.name, desc));
        }
        out
    }

    /// The prompt's SKILLS block under `[skills] index_mode`.
    ///
    /// The full index is 136 lines of `name: 57-char description` in
    /// every prompt, for a lookup the model makes rarely and that
    /// `skills_list` answers on demand. The modes:
    ///
    ///   "full"     — every enabled skill (the historical behavior, and
    ///                the one-line rollback).
    ///   "recent"   — at most `index_max_entries` skills, with the ones
    ///                actually used in the last `index_recent_sessions`
    ///                sessions taking the seats first and the rest
    ///                backfilled, plus a line saying how many more
    ///                there are and how to reach them.
    ///   "deferred" — that pointer line only.
    ///
    /// Nothing becomes unreachable in any mode: `skills_list` and
    /// `skill_view` are core-tier and always resident. What changes is
    /// only what pays rent in every prompt — and every mode that hides
    /// something says so, in the prompt, with a count.
    pub fn index_for(&self, cfg: &crate::config::SkillsSection, recent: &[String]) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let total = self.skills.len();
        match cfg.index_mode.trim() {
            "full" => self.index(),
            "deferred" => Self::pointer(total, 0),
            // "recent" and anything unrecognised: an unknown mode must
            // not silently mean "full" — it falls back to the default,
            // which is the documented one.
            _ => {
                let mut keep: Vec<SkillMeta> = recent
                    .iter()
                    .filter_map(|n| self.skills.iter().find(|s| &s.name == n))
                    .take(cfg.index_max_entries)
                    .cloned()
                    .collect();
                // Backfill to the cap. Recency decides PRIORITY, not
                // membership: an index that listed nothing until a
                // skill had already been used would leave a fresh
                // install unable to name a single skill, on the
                // feature this project calls its core value — and it
                // would still owe a `skills_list` round-trip to learn
                // them. The CAP is what buys the tokens back (30 lines
                // instead of 136); recency only decides who gets the
                // seats.
                for s in &self.skills {
                    if keep.len() >= cfg.index_max_entries {
                        break;
                    }
                    if keep.iter().any(|k| k.name == s.name) {
                        continue;
                    }
                    keep.push(s.clone());
                }
                if keep.is_empty() {
                    return Self::pointer(total, 0);
                }
                // Render in the same (category, name) order `index()`
                // uses: the grouping headers assume it.
                keep.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
                let shown = keep.len();
                let sub = SkillSet { skills: keep };
                let mut out = sub.index();
                if shown < total {
                    out.push('\n');
                    out.push_str(&Self::pointer(total, shown));
                }
                out
            }
        }
    }

    /// The line that keeps a reduced index honest: how many skills
    /// exist, how many are listed, and the tool that shows the rest.
    fn pointer(total: usize, shown: usize) -> String {
        let hidden = total.saturating_sub(shown);
        if shown == 0 {
            format!(
                "Skills are reusable procedures. {total} are available but not \
listed here — call `skills_list` to see them, then `skill_view` to load one \
BEFORE acting.\n"
            )
        } else {
            format!(
                "{hidden} more skills are available but not listed above \
({total} in total) — call `skills_list` to see them.\n"
            )
        }
    }
}

/// Resolve a name collision in favour of the LAST root that supplied it.
///
/// `skill_roots` lists hermes first and veles second precisely so that
/// veles' copy overrides an inherited one. The de-duplication here used
/// to be `retain` over a `seen` set, which keeps the FIRST match — so it
/// did the exact opposite of the comment above it, and the HERMES copy
/// won every conflict.
///
/// That is not cosmetic. On this machine 125 of the skills veles owns
/// also exist under `~/.hermes/skills`, so every one of them was being
/// read from the hermes copy: `skill_view` showed hermes' text, edits
/// made on the veles side were invisible, and the Skills manager marked
/// them `editable: false` and refused to change or delete a skill veles
/// owned outright.
///
/// The test that claimed to cover this only ever walked ONE root.
fn dedup_last_root_wins(skills: Vec<SkillMeta>) -> Vec<SkillMeta> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<SkillMeta> = skills
        .into_iter()
        .rev()
        .filter(|s| seen.insert(s.name.clone()))
        .collect();
    // `rev` above walked from the last root backwards; put discovery
    // order back before the caller sorts, so a change to the sort key
    // cannot silently start depending on this reversal.
    out.reverse();
    out
}

fn skill_roots() -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    // Under a thread-scoped DirOverride — i.e. in TESTS — only the
    // override's own root is read, exactly like `hermes_config_yaml`:
    // the override says the scratch directories are the whole world for
    // this thread, and neither the developer's real ~/.hermes/skills
    // (147 of them on this box) nor the checkout's bundled skills are
    // part of it. Without this, every prompt test carried the
    // developer's own skill index (Step 5.5).
    if crate::config::dirs().is_none() {
        // The skills veles ships (`<repo>/skills`, installed beside
        // `bin/` as `veles-desktop` is): the lowest-priority root, so a
        // user's copy of the same name wins. The first of these is
        // `windows-node` (PLAN-windows.md §6), which `platforms:` hides
        // everywhere else.
        if let Some(bundled) = crate::config::beside_binary("skills") {
            roots.push(("bundled".to_string(), bundled));
        }
        // Migration compat: hermes' library, read-only (same policy as
        // config/.env).
        if crate::profiles::selected_profile().is_ok_and(|p| p == "default") {
            if let Some(home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
                let h = home.join(".hermes").join("skills");
                if h.is_dir() {
                    roots.push(("hermes".to_string(), h));
                }
            }
        }
    }
    roots.push((
        "veles".to_string(),
        crate::config::config_dir().join("skills"),
    ));
    roots
}

/// Walk a root: <root>/<category>/<name>/SKILL.md or <root>/<name>/SKILL.md.
fn discover_in(dir: &Path, root_label: &str, category: &str, out: &mut Vec<SkillMeta>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let skill_md = p.join("SKILL.md");
        if skill_md.is_file() {
            if let Some(fm) = read_frontmatter(&skill_md) {
                // A skill written for another platform is not in this
                // node's world at all (PLAN-windows.md §6): not in the
                // index, not in `skills_list`, not in `skill_view` — the
                // same rule as `load_selected`, for the same reason. A
                // model on Windows that can still read "run systemctl
                // --user" has not been told it is on Windows.
                if !fm.runs_on(crate::platform::OS) {
                    continue;
                }
                let dir_name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                out.push(SkillMeta {
                    name: sanitize_name(&dir_name),
                    description: fm.description,
                    category: category.to_string(),
                    dir: p,
                    root: root_label.to_string(),
                });
            }
        } else {
            // category folder — recurse one level
            let cat = if category.is_empty() {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            } else {
                format!(
                    "{}/{}",
                    category,
                    p.file_name().unwrap_or_default().to_string_lossy()
                )
            };
            discover_in(&p, root_label, &cat, out);
        }
    }
}

fn sanitize_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    n.chars().take(NAME_MAX).collect()
}

fn truncate_desc(d: &str) -> String {
    let first_line = d.lines().next().unwrap_or("").trim();
    if first_line.len() <= INDEX_DESC_MAX {
        return first_line.to_string();
    }
    // Char-aware truncation — byte slicing panics on multibyte UTF-8
    // (the library has Chinese-language skill descriptions).
    let mut end = 0;
    for (i, (byte_i, _)) in first_line.char_indices().enumerate() {
        if i >= INDEX_DESC_MAX {
            break;
        }
        end = byte_i;
    }
    // include the char that starts at `end`
    let ch_len = first_line[end..]
        .chars()
        .next()
        .map(|c| c.len_utf8())
        .unwrap_or(0);
    format!("{}...", &first_line[..end + ch_len])
}

/// What a SKILL.md's frontmatter says that discovery needs.
struct Frontmatter {
    description: String,
    /// `platforms: [linux, windows, android]` — the nodes the skill is
    /// for. `None` (no key, or an empty list) means everywhere, which
    /// keeps every hermes-imported skill visible as it always was.
    platforms: Option<Vec<String>>,
}

impl Frontmatter {
    /// Is this skill for a node whose `platform::OS` is `os`? `unix` in
    /// the list matches every unix OS (linux, android, freebsd), so a
    /// skill about shell idioms does not have to enumerate them.
    fn runs_on(&self, os: &str) -> bool {
        let Some(list) = &self.platforms else {
            return true;
        };
        list.iter()
            .any(|p| p == os || (p == "unix" && os != "windows") || p == "all" || p == "any")
    }
}

/// Minimal frontmatter reader: the description (the only field the
/// index needs; the name comes from the directory, hermes-style) and
/// the `platforms` list. None when there is no frontmatter or no
/// description.
fn read_frontmatter(path: &Path) -> Option<Frontmatter> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.strip_prefix("---")?;
    let end = raw.find("\n---")?;
    let block = &raw[..end];
    let mut description = None;
    let mut platforms = None;
    for line in block.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "description" if description.is_none() => {
                let desc: String = v
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .chars()
                    .take(DESC_MAX)
                    .collect();
                if !desc.is_empty() {
                    description = Some(desc);
                }
            }
            // `[linux, windows]`, `linux, windows` or `"linux"` — the
            // flow forms; a block list (`- linux` lines) is not read,
            // and an unread key means "everywhere", never "nowhere".
            "platforms" => {
                let list: Vec<String> = v
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|p| {
                        p.trim()
                            .trim_matches('"')
                            .trim_matches('\'')
                            .to_ascii_lowercase()
                    })
                    .filter(|p| !p.is_empty())
                    .collect();
                if !list.is_empty() {
                    platforms = Some(list);
                }
            }
            _ => {}
        }
    }
    Some(Frontmatter {
        description: description?,
        platforms,
    })
}

/// The description alone (what `skill_manage` reports after an install).
fn parse_frontmatter(path: &Path) -> Option<String> {
    read_frontmatter(path).map(|f| f.description)
}

/// The line that warns a body will not survive `cap_tool_output`.
///
/// The cap (`[agent] tool_output_cap`) is applied to EVERY tool result
/// as head 60% / tail 40% — so what a long skill loses is its MIDDLE,
/// and it loses it silently: the elision marker names the cap but not
/// the source file, and this tool's header used to name the root
/// LABEL ("veles") rather than the path. An agent holding a cut skill
/// therefore had no file to go read and no parameter to page with.
///
/// USER 2026-09-19, of `rust-agent-runtime` (55,276 chars against a
/// 30,000 cap): "is the middle of it just invisible forever now".
/// Eleven whole sections were, including the one on implementing
/// tools.
///
/// `None` when nothing will be cut — a notice on every small skill is
/// noise, and noise is what gets skimmed past. `cap == 0` disables
/// truncation altogether, so it disables this too.
fn cap_warning(path: &Path, chars: usize, lines: usize, cap: usize) -> Option<String> {
    if cap == 0 || chars <= cap {
        return None;
    }
    Some(format!(
        "[!] This skill is {chars} chars / {lines} lines and the tool-output cap \
         is {cap}: the MIDDLE of what follows will be elided before you read it. \
         Page it instead — skill_view with offset/limit (1-indexed lines), or \
         read_file on {}.\n\n",
        path.display()
    ))
}

/// Full SKILL.md body + linked-file listing for skill_view.
///
/// `offset`/`limit` page by LINE through `filetools::render_read`, the
/// same renderer `read_file` uses. Both `None` returns the raw body,
/// because the desktop skills pane and the TUI render it in a widget
/// that scrolls on its own and would otherwise grow a line-number
/// gutter nobody asked for.
pub fn view_skill(
    set: &SkillSet,
    name: &str,
    file_path: Option<&str>,
    offset: Option<usize>,
    limit: Option<usize>,
) -> String {
    view_skill_capped(
        set,
        name,
        file_path,
        offset,
        limit,
        crate::config::live().agent.tool_output_cap,
    )
}

/// `view_skill` with the cap injected, so the warning can be tested
/// without a config file deciding the outcome.
pub fn view_skill_capped(
    set: &SkillSet,
    name: &str,
    file_path: Option<&str>,
    offset: Option<usize>,
    limit: Option<usize>,
    cap: usize,
) -> String {
    let Some(s) = set.get(name) else {
        // near-miss suggestion, hermes style
        let mut near: Vec<&str> = set
            .skills
            .iter()
            .filter(|x| x.name.contains(name) || name.contains(&x.name))
            .map(|x| x.name.as_str())
            .collect();
        near.dedup();
        let hint = if near.is_empty() {
            String::new()
        } else {
            format!(" — similar: {}", near.join(", "))
        };
        return format!("skill not found: {name}{hint}");
    };
    if let Some(fp) = file_path {
        return read_linked_file(s, fp, offset, limit);
    }
    let path = s.dir.join("SKILL.md");
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let linked = list_linked_files(&s.dir);

    // The header carries the PATH. That one line is the difference
    // between "the middle is gone" and "the middle is one read_file
    // away" — see `cap_warning`.
    let mut out = format!(
        "--- {} ({}, {} root) ---\npath: {}\n",
        s.name,
        s.category,
        s.root,
        path.display()
    );
    // Before the body, because head 60% is the part that survives.
    if offset.is_none() && limit.is_none() {
        if let Some(w) = cap_warning(&path, body.chars().count(), body.lines().count(), cap) {
            out.push_str(&w);
        }
    }
    if offset.is_some() || limit.is_some() {
        out.push_str(&crate::filetools::render_read(
            &path.display().to_string(),
            body.as_bytes(),
            offset.unwrap_or(1).max(1),
            limit.unwrap_or(2000),
            None,
        ));
    } else {
        out.push_str(&body);
    }
    if !linked.is_empty() {
        out.push_str("\n\nlinked_files (load via skill_view file_path):\n");
        for f in &linked {
            out.push_str(&format!("  - {f}\n"));
        }
    }
    out
}

fn read_linked_file(
    s: &SkillMeta,
    fp: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> String {
    // path traversal guard: only allow paths under the skill dir. The
    // lexical half first: a `..`, a root or a drive in the reference is
    // refused before the filesystem is asked, so the answer is "refusing",
    // never "not found" (on Windows `dir.join("../..C:\x")` is not even a
    // path canonicalize can fail on honestly — 2026-10-13).
    {
        use std::path::Component;
        if std::path::Path::new(fp).components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return format!("refusing path outside skill dir: {fp}");
        }
    }
    let target = s.dir.join(fp);
    let canon_dir = match s.dir.canonicalize() {
        Ok(c) => c,
        Err(e) => return format!("skill dir unreadable: {e}"),
    };
    let canon = match target.canonicalize() {
        Ok(c) => c,
        Err(e) => return format!("linked file not found: {fp} ({e})"),
    };
    if !canon.starts_with(&canon_dir) {
        return format!("refusing path outside skill dir: {fp}");
    }
    let body = match std::fs::read_to_string(&canon) {
        Ok(b) => b,
        Err(e) => return format!("read failed: {e}"),
    };
    // A reference file is subject to the same cap as a SKILL.md, and
    // the biggest one here is already a third of it.
    if offset.is_some() || limit.is_some() {
        return crate::filetools::render_read(
            &canon.display().to_string(),
            body.as_bytes(),
            offset.unwrap_or(1).max(1),
            limit.unwrap_or(2000),
            None,
        );
    }
    body
}

fn list_linked_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["references", "templates", "scripts", "assets"] {
        let d = dir.join(sub);
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().is_file() {
                out.push(format!("{sub}/{}", e.file_name().to_string_lossy()));
            }
        }
    }
    out.sort();
    out
}

// ── install (S13, Letta `skills install` parity) ─────────────────────

/// Install a skill from a git URL into the OWNED veles root
/// (`~/.config/veles/skills/`). Accepted shapes:
///   - https://host/path/repo.git | git@host:path/repo.git — repo whose
///     root or <category>/ subdir carries SKILL.md dirs; whole repo is
///     copied under its own name (or `as` name)
///   - https://host/path/repo + `--name x` — same with a chosen name
///
/// The hermes migration root is never written.
///
/// Returns a human-readable report. Refuses to clobber an existing
/// skill unless `overwrite`.
pub fn install(url: &str, as_name: Option<&str>, overwrite: bool) -> String {
    // Resolve the target root: the owned veles skills dir.
    let roots = skill_roots();
    let Some((_, dest_root)) = roots.iter().find(|(l, _)| l == "veles") else {
        return "error: no veles skills root".into();
    };

    // Shallow clone into a scratch dir (git is on PATH everywhere veles runs).
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("veles-skill-install-{stamp}"));
    let _ = std::fs::remove_dir_all(&tmp);
    let fetch = std::process::Command::new("git")
        .args([
            "clone",
            "--quiet",
            "--depth",
            "1",
            url,
            &tmp.to_string_lossy(),
        ])
        .status();
    match fetch {
        Ok(s) if s.success() => {}
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return format!("error: git clone failed for {url}");
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return format!("error: cannot run git: {e}");
        }
    }

    // Validate: find SKILL.md files in the clone (root or one category deep).
    let mut found: Vec<PathBuf> = Vec::new();
    collect_skill_dirs(&tmp, &mut found);
    if found.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        return format!(
            "error: no SKILL.md found in {url} — a skill needs SKILL.md at the repo root (or inside a category dir)"
        );
    }
    if found.len() > 1 && as_name.is_none() {
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(&tmp).unwrap_or(p).display().to_string())
            .collect();
        let _ = std::fs::remove_dir_all(&tmp);
        return format!(
            "multiple skills in the repo ({}) — install with a name: veles skills install {url} --name <one-of-them>",
            names.join(", ")
        );
    }

    let src_dir = found.remove(0);
    // Name: explicit `as` > frontmatter name > dir/file stem.
    let name = as_name
        .map(String::from)
        .or_else(|| read_frontmatter_name(&src_dir.join("SKILL.md")))
        .unwrap_or_else(|| {
            src_dir
                .file_stem()
                .or_else(|| src_dir.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "installed-skill".into())
        });
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        let _ = std::fs::remove_dir_all(&tmp);
        return format!("error: invalid skill name '{name}'");
    }
    let dest = dest_root.join(&name);
    if dest.exists() && !overwrite {
        let _ = std::fs::remove_dir_all(&tmp);
        return format!(
            "skill '{name}' already exists at {} — pass --overwrite to replace it",
            dest.display()
        );
    }
    if let Err(e) = copy_dir_recursive(&src_dir, &dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        return format!("error: copy failed: {e}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // Report what landed (description from the frontmatter when present).
    let desc = parse_frontmatter(&dest.join("SKILL.md")).unwrap_or_default();
    format!(
        "installed '{name}' into {}\n  description: {desc}\n(restarts of existing sessions keep their frozen index; new sessions pick it up)",
        dest.display()
    )
}

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
            if out.len() > 32 {
                return; // absurd repo guard
            }
        }
    }
}

fn read_frontmatter_name(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines().skip(1).take(12) {
        if line.trim() == "---" {
            break;
        }
        if let Some(v) = line.strip_prefix("name:") {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let p = e.path();
        if p.file_name().map(|n| n == ".git").unwrap_or(true) {
            continue;
        }
        let t = dst.join(e.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &t)?;
        } else {
            std::fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

// ── tools ─────────────────────────────────────────────────────────────

// ── management operations ───────────────────────────────────────────
//
// These were inline in `SkillManageTool`, reachable only by the agent
// asking for them mid-turn. The desktop needed the same three
// operations, and a second copy of "is this skill writable" is exactly
// the kind of duplication that ends with two answers — so the logic
// lives here and both surfaces call it.
//
// The hermes root stays read-only, everywhere: it is someone else's
// library that veles reads for migration compatibility.

/// Where veles' own (writable) skills live.
pub fn veles_skills_root() -> PathBuf {
    crate::config::config_dir().join("skills")
}

/// The SKILL.md path for a skill veles is allowed to modify.
fn writable_path(set: &SkillSet, name: &str) -> anyhow::Result<PathBuf> {
    let meta = set
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("skill not found: {name}"))?;
    anyhow::ensure!(
        meta.root == "veles",
        "{name} is {}-imported and read-only — copy it into ~/.config/veles/skills first",
        meta.root
    );
    Ok(meta.dir.join("SKILL.md"))
}

/// Create a skill from a full SKILL.md body.
pub fn create_skill(name: &str, category: &str, content: &str) -> anyhow::Result<PathBuf> {
    let name = sanitize_name(name);
    anyhow::ensure!(!name.is_empty(), "a skill needs a name");
    anyhow::ensure!(
        content.contains("description:"),
        "a skill needs YAML frontmatter with a description — that line is what the index shows"
    );
    let root = veles_skills_root();
    let cat = sanitize_name(category);
    let dir = if cat.is_empty() {
        root.join(&name)
    } else {
        root.join(&cat).join(&name)
    };
    anyhow::ensure!(!dir.exists(), "{name} already exists — edit it instead");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), content)?;
    Ok(dir)
}

/// A starter SKILL.md, so "New skill" produces something valid rather
/// than an empty file the loader will skip.
pub fn skill_template(name: &str, description: &str) -> String {
    let desc = if description.trim().is_empty() {
        format!("Use when working on {name}.")
    } else {
        description.trim().replace('\n', " ")
    };
    format!(
        "---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\n\
         The description above is the ONLY part that reaches the system prompt\n\
         index, so it has to say when to reach for this skill. Everything below\n\
         loads on demand, when the agent decides the skill is relevant.\n\n\
         ## Steps\n\n1. \n"
    )
}

/// Replace a skill's SKILL.md wholesale (the editor's save path).
pub fn write_skill(name: &str, content: &str) -> anyhow::Result<PathBuf> {
    let set = SkillSet::load();
    let path = writable_path(&set, name)?;
    anyhow::ensure!(
        content.contains("description:"),
        "the body must keep its YAML frontmatter description"
    );
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Replace one occurrence of `old` with `new` in a skill's SKILL.md.
pub fn patch_skill(name: &str, old: &str, new: &str) -> anyhow::Result<PathBuf> {
    let set = SkillSet::load();
    let path = writable_path(&set, name)?;
    let body =
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("SKILL.md unreadable: {e}"))?;
    match body.matches(old).count() {
        0 => anyhow::bail!("old_string not found"),
        1 => {
            std::fs::write(&path, body.replacen(old, new, 1))?;
            Ok(path)
        }
        n => anyhow::bail!("old_string matches {n} times — be more specific"),
    }
}

/// Delete a skill veles owns.
pub fn delete_skill(name: &str) -> anyhow::Result<PathBuf> {
    let set = SkillSet::load();
    let meta = set
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("skill not found: {name}"))?;
    anyhow::ensure!(
        meta.root == "veles",
        "{name} is {}-imported — veles never deletes another library's skills",
        meta.root
    );
    let dir = meta.dir.clone();
    std::fs::remove_dir_all(&dir)?;
    Ok(dir)
}

pub struct SkillsListTool {
    pub session_id: String,
    pub store: std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
}

#[async_trait]
impl crate::tools::Tool for SkillsListTool {
    fn name(&self) -> &str {
        "skills_list"
    }
    fn description(&self) -> &str {
        "List available skills (name + description + category). Load one with skill_view before acting on tasks it covers."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": {"type": "string", "description": "Optional category filter"}
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        let set = SkillSet::load_selected(&selection_for(&self.store, &self.session_id));
        let cat = args["category"].as_str().map(|s| s.to_string());
        let mut out = String::new();
        for s in &set.skills {
            if let Some(c) = &cat {
                if !s.category.starts_with(c) {
                    continue;
                }
            }
            out.push_str(&format!("{} [{}]: {}\n", s.name, s.category, s.description));
        }
        if out.is_empty() {
            "no skills".into()
        } else {
            out
        }
    }
}

pub struct SkillViewTool {
    pub session_id: String,
    pub store: std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
}

#[async_trait]
impl crate::tools::Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }
    fn description(&self) -> &str {
        // The WHY of paging lives in `cap_warning`, which fires only
        // when a body will actually be cut. This is resident schema
        // and pays rent on every turn (`the_core_tier_is_under_the_
        // declared_budget`), so it says only what the model cannot
        // work out from the output it is holding.
        "Load a skill's full SKILL.md (with its linked-file list), or one linked file via file_path \
(e.g. references/api.md). Page a long one with offset/limit."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name (from skills_list)"},
                "file_path": {"type": "string", "description": "Optional linked file within the skill"},
                "offset": {"type": "integer", "description": "1-indexed start line", "minimum": 1},
                "limit": {"type": "integer", "description": "max lines", "minimum": 1}
            },
            "required": ["name"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let sel = selection_for(&self.store, &self.session_id);
        let set = SkillSet::load_selected(&sel);
        let Some(name) = args["name"].as_str() else {
            return "missing name".into();
        };
        // Say WHY rather than "not found": a skill switched off for this
        // session is a different situation from one that never existed,
        // and the agent can act on the difference.
        if set.get(name).is_none() && SkillSet::load().get(name).is_some() {
            return format!("{name} is switched off for this session");
        }
        // Record the load before rendering: this is what the `recent`
        // index mode reads, so a skill the agent actually reaches for
        // earns its place in the next session's resident index. Best
        // effort — a bookkeeping failure must not fail the view.
        if set.get(name).is_some() {
            if let Ok(s) = self.store.lock() {
                let _ = s.record_skill_use(name, &self.session_id);
            }
        }
        view_skill(
            &set,
            name,
            args["file_path"].as_str(),
            args["offset"].as_u64().map(|n| n as usize),
            args["limit"].as_u64().map(|n| n as usize),
        )
    }
}

pub struct SkillManageTool;

#[async_trait]
impl crate::tools::Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }
    fn description(&self) -> &str {
        "Create or update skills in ~/.config/veles/skills. Actions: create {name, content, category?}, patch {name, old_string, new_string}, delete {name}. content is the full SKILL.md (frontmatter + body). patch edits SKILL.md in place. Hermes-imported skills are read-only here — copy first with create."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "patch", "delete"]},
                "name": {"type": "string"},
                "content": {"type": "string"},
                "category": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["action", "name"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let action = args["action"].as_str().unwrap_or("");
        let Some(name) = args["name"].as_str() else {
            return "missing name".into();
        };
        let name = sanitize_name(name);

        // Each arm is one call into the shared operation, so the agent
        // and the desktop manager get the same answer — including the
        // same refusal on a hermes-imported skill. A tool returns prose,
        // so the Result is rendered rather than propagated.
        let done = match action {
            "create" => match args["content"].as_str() {
                None => return "create: missing content".into(),
                Some(content) => {
                    create_skill(&name, args["category"].as_str().unwrap_or(""), content)
                        .map(|dir| format!("created skill {name} at {}", dir.display()))
                }
            },
            "patch" => match args["old_string"].as_str() {
                None => return "patch: missing old_string".into(),
                Some(old) => patch_skill(&name, old, args["new_string"].as_str().unwrap_or(""))
                    .map(|_| format!("patched {name}")),
            },
            "delete" => delete_skill(&name).map(|_| format!("deleted skill {name}")),
            other => return format!("unknown action: {other}"),
        };
        match done {
            Ok(msg) => msg,
            Err(e) => format!("{action}: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A skill tree built in a scratch dir. Uses the same layout the
    /// discoverer walks: <root>/<category>/<name>/SKILL.md.
    fn skill_fixture(root: &Path, category: &str, name: &str, description: &str) -> PathBuf {
        skill_fixture_with(root, category, name, description, "")
    }

    /// A fixture with one more frontmatter line (`platforms: [...]`).
    fn skill_fixture_with(
        root: &Path,
        category: &str,
        name: &str,
        description: &str,
        extra: &str,
    ) -> PathBuf {
        let dir = root.join(category).join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!("{extra}\n")
        };
        let body = format!(
            "---\nname: {name}\ndescription: \"{description}\"\n{extra}---\n\n# {name}\n\nbody text\n"
        );
        std::fs::write(dir.join("SKILL.md"), body).expect("write SKILL.md");
        dir
    }

    /// `platforms:` in the frontmatter decides whether a skill exists on
    /// this node at all (PLAN-windows.md §6). A skill for another OS is
    /// not discovered; one naming this OS, or `unix` on a unix node, or
    /// no key at all, is. The hermes library has no such key anywhere,
    /// so nothing imported disappears.
    #[test]
    fn a_skill_for_another_platform_is_not_discovered_here() {
        let root = tempfile::tempdir().unwrap();
        let here = crate::platform::OS;
        skill_fixture_with(
            root.path(),
            "",
            "here",
            "for this os",
            &format!("platforms: [{here}]"),
        );
        skill_fixture_with(
            root.path(),
            "",
            "elsewhere",
            "for another os",
            "platforms: [plan9]",
        );
        skill_fixture_with(
            root.path(),
            "",
            "either",
            "for both",
            &format!("platforms: plan9, \"{here}\""),
        );
        skill_fixture_with(
            root.path(),
            "",
            "unix-family",
            "for unix",
            "platforms: [unix]",
        );
        skill_fixture(root.path(), "", "untagged", "for everyone");
        skill_fixture_with(
            root.path(),
            "",
            "empty-list",
            "for everyone too",
            "platforms: []",
        );

        let mut found = Vec::new();
        discover_in(root.path(), "t", "", &mut found);
        let mut names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        let mut want = vec!["here", "either", "untagged", "empty-list"];
        if cfg!(unix) {
            want.push("unix-family");
        }
        want.sort();
        assert_eq!(names, want, "platforms: filters discovery");
    }

    #[test]
    fn frontmatter_description_is_extracted_and_quoting_survives() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SKILL.md");
        std::fs::write(
            &p,
            "---\nname: x\ndescription: \"quoted desc\"\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(
            parse_frontmatter(&p).as_deref(),
            Some("quoted desc"),
            "double quotes are stripped"
        );
        std::fs::write(&p, "---\nname: x\ndescription: 'single'\n---\n").unwrap();
        assert_eq!(parse_frontmatter(&p).as_deref(), Some("single"));
        std::fs::write(&p, "---\nname: x\nother: y\n---\n").unwrap();
        assert!(
            parse_frontmatter(&p).is_none(),
            "no description means None — the index must not show a blank line"
        );
        std::fs::write(&p, "no frontmatter at all").unwrap();
        assert!(parse_frontmatter(&p).is_none());
    }

    /// The dedup, tested as the pure function it now is — the roots
    /// themselves cannot be faked here (the hermes one comes off the
    /// real HOME), which is exactly why the old test walked a single
    /// root and the bug survived.
    #[test]
    fn a_name_in_both_roots_resolves_to_the_later_one() {
        let meta = |name: &str, root: &str| SkillMeta {
            name: name.into(),
            description: format!("from {root}"),
            category: String::new(),
            dir: PathBuf::from(format!("/{root}/{name}")),
            root: root.into(),
        };
        // Discovery order: hermes first, then veles — as skill_roots
        // lists them.
        let got = dedup_last_root_wins(vec![
            meta("shared", "hermes"),
            meta("hermes-only", "hermes"),
            meta("shared", "veles"),
            meta("veles-only", "veles"),
        ]);
        let by = |n: &str| {
            got.iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("{n} missing from {got:?}"))
                .root
                .clone()
        };
        assert_eq!(got.len(), 3, "one entry per name: {got:?}");
        assert_eq!(by("shared"), "veles", "the LATER root wins the conflict");
        assert_eq!(by("hermes-only"), "hermes", "an unshared name survives");
        assert_eq!(by("veles-only"), "veles");
    }

    /// Ownership is what the Skills manager gates editing on, so the
    /// winner's `root` has to be the winner's — not the loser's label
    /// carried over.
    #[test]
    fn the_winner_brings_its_own_directory_and_root() {
        let got = dedup_last_root_wins(vec![
            SkillMeta {
                name: "shared".into(),
                description: "old".into(),
                category: String::new(),
                dir: PathBuf::from("/hermes/shared"),
                root: "hermes".into(),
            },
            SkillMeta {
                name: "shared".into(),
                description: "new".into(),
                category: String::new(),
                dir: PathBuf::from("/veles/shared"),
                root: "veles".into(),
            },
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].root, "veles");
        assert_eq!(got[0].dir, PathBuf::from("/veles/shared"));
        assert_eq!(got[0].description, "new", "and its text, not the old one");
    }

    #[test]
    fn discovery_walks_categories_and_later_roots_win() {
        let (_home, _env) = crate::testenv::scratch_home();
        // The REAL veles root shape: with VELES_HOME forced,
        // config_dir() collapses onto it, so skills live at
        // <home>/skills (not the stale ~/.veles/skills this test used
        // to invent — discover_in takes the path directly, so the old
        // name never mattered to the assertions, but it taught the
        // wrong layout to anyone reading). The hermes root is skipped:
        // the real one may exist but its entries are not
        // deterministic, and this test only asserts relative ordering
        // within what it created.
        let veles_root = crate::config::config_dir().join("skills");
        let a = skill_fixture(&veles_root, "cat1", "alpha", "first");
        let b = skill_fixture(&veles_root, "cat1", "beta", "second");
        let c = skill_fixture(&veles_root, "", "gamma", "third");

        let mut found = Vec::new();
        discover_in(&veles_root, "veles", "", &mut found);
        // Root-level and nested both land; names are sanitized dir names.
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"beta"), "{names:?}");
        assert!(names.contains(&"gamma"), "{names:?}");
        assert_eq!(
            found.iter().find(|s| s.name == "alpha").unwrap().dir,
            a,
            "dir recorded for later view_skill"
        );
        drop((b, c));
    }

    /// Under a scratch home, the developer's OWN skills must not leak
    /// into the set — the plan's Step 5.5: test prompts used to list the
    /// real machine's 147 skills because `skill_roots()` reached the
    /// real `~/.hermes/skills` through `UserDirs`, which a thread-scoped
    /// `DirOverride` cannot intercept (only `VELES_HOME`-derived paths
    /// can). The override says "these scratch directories are the whole
    /// world for this thread" — the same rule `hermes_config_yaml`
    /// already obeys. On a box with no ~/.hermes this was invisible; on
    /// this one every prompt test carried the developer's library.
    #[test]
    fn a_scratch_home_hides_the_developers_own_skills() {
        let (home, _env) = crate::testenv::scratch_home();
        // Even a FAKE hermes install inside the scratch world is not
        // read: the compat root is the real developer's library, and
        // nothing under the scratch home is that.
        let fake = home.path().join(".hermes").join("skills");
        skill_fixture(&fake, "", "fake-hermes-skill", "should not be found");

        let set = SkillSet::load();
        assert!(
            set.skills.iter().all(|s| s.root != "hermes"),
            "the hermes compat root must not be read under an override: {:?}",
            set.skills
                .iter()
                .filter(|s| s.root == "hermes")
                .map(|s| &s.name)
                .collect::<Vec<_>>()
        );
        // And the veles root IS the scratch one: a skill planted there
        // is found, and only skills from there.
        let veles_root = crate::config::config_dir().join("skills");
        skill_fixture(&veles_root, "", "scratch-skill", "found");
        let set = SkillSet::load();
        let names: Vec<&str> = set.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["scratch-skill"],
            "the scratch world's skills are the whole index"
        );
    }

    #[test]
    fn sanitize_name_neutrons_hostile_directory_names() {
        // A directory name is data, never a path component beyond itself.
        assert_eq!(sanitize_name("my skill!"), "my-skill-");
        assert_eq!(sanitize_name("../etc/passwd"), "---etc-passwd");
        assert_eq!(sanitize_name("中文-skill"), "中文-skill", "unicode kept");
        let long = "x".repeat(100);
        assert_eq!(sanitize_name(&long).len(), NAME_MAX);
    }

    #[test]
    fn linked_file_view_refuses_path_traversal() {
        // WSA-2026-6's class: a network/model-supplied file_path escaping
        // the skill dir. The canonicalize guard must refuse it.
        let root = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        std::fs::write(secret.path().join("secret.txt"), "top secret").unwrap();

        let dir = skill_fixture(root.path(), "cat", "trav", "desc");
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("references").join("ok.md"), "safe content").unwrap();

        let set = SkillSet {
            skills: vec![SkillMeta {
                name: "trav".into(),
                description: "desc".into(),
                category: "cat".into(),
                dir: dir.clone(),
                root: "veles".into(),
            }],
        };

        let ok = view_skill(&set, "trav", Some("references/ok.md"), None, None);
        assert!(ok.contains("safe content"), "{ok}");

        // A ../ chain out of the skill dir. Aimed at the secret DIR this
        // test actually created: the first version asserted
        // `!out.contains("top secret")` against a path that pointed at
        // /etc/passwd instead, so that assertion could not fail however
        // broken the guard was.
        let escape = format!(
            "../../../../../../../..{}/secret.txt",
            secret.path().display()
        );
        let out = view_skill(&set, "trav", Some(&escape), None, None);
        assert!(
            !out.contains("top secret"),
            "a ../ chain must not read outside the skill dir: {out}"
        );
        assert!(
            out.contains("refusing"),
            "…and must say it refused, not merely fail to find it — a \
             read that happens to fail is not a guard: {out}"
        );

        // Symlinks are the WSA-2026-6 shape proper: the path stays
        // inside the skill dir, the TARGET does not. canonicalize is
        // what closes it, and a guard built on string prefixes would
        // pass everything above and still leak here.
        #[cfg(unix)]
        {
            let link = dir.join("references").join("escape.md");
            std::os::unix::fs::symlink(secret.path().join("secret.txt"), &link).unwrap();
            let via_link = view_skill(&set, "trav", Some("references/escape.md"), None, None);
            assert!(
                !via_link.contains("top secret"),
                "a symlink out of the skill dir must not be followed: {via_link}"
            );
            assert!(
                via_link.contains("refusing"),
                "…and must be refused explicitly: {via_link}"
            );
        }

        let abs = view_skill(&set, "trav", Some("/etc/passwd"), None, None);
        assert!(
            !abs.contains("root:x:0:0:"),
            "absolute paths are also confined: {abs}"
        );
    }

    #[test]
    fn index_truncates_long_and_multibyte_descriptions_without_panicking() {
        // Byte slicing panics on multibyte UTF-8; the library carries
        // Chinese descriptions. truncate_desc must be char-aware.
        let zh = "这是一段很长的中文描述".repeat(10);
        let t = truncate_desc(&zh);
        assert!(t.ends_with("..."), "{t}");
        assert!(t.is_char_boundary(t.len() - 3), "cut on a char boundary");

        let long_ascii = "word ".repeat(40);
        let t2 = truncate_desc(&long_ascii);
        assert!(t2.len() <= INDEX_DESC_MAX + 4, "{t2}");

        // Multiline: only the first line survives.
        let t3 = truncate_desc("first line\nsecond line");
        assert_eq!(t3, "first line");
    }

    #[test]
    fn near_miss_suggestion_names_the_closest_skill() {
        let dir = tempfile::tempdir().unwrap();
        let d = skill_fixture(dir.path(), "cat", "skill-view", "desc");
        let set = SkillSet {
            skills: vec![SkillMeta {
                name: "skill-view".into(),
                description: "desc".into(),
                category: "cat".into(),
                dir: d,
                root: "veles".into(),
            }],
        };
        // A typo'd name suggests instead of a bare failure. "skillview"
        // CONTAINS no match, but "skill-view" contains "skillview" with
        // the dash removed — the near-miss filter is substring-based
        // both ways (`x.name.contains(name) || name.contains(&x.name)`),
        // so use a prefix typo instead.
        let out = view_skill(&set, "skill-vie", None, None, None);
        assert!(
            out.starts_with("skill not found: skill-vie"),
            "names the miss: {out}"
        );
        assert!(
            out.contains("similar: skill-view"),
            "suggests the close match: {out}"
        );
    }

    // ── skills index modes (Step 2.3) ───────────────────────────────

    /// A set of `n` skills named skill-00.., all in one category.
    fn set_of(n: usize) -> SkillSet {
        SkillSet {
            skills: (0..n)
                .map(|i| SkillMeta {
                    name: format!("skill-{i:02}"),
                    description: format!("does thing {i}"),
                    category: "cat".into(),
                    dir: PathBuf::from("/nonexistent"),
                    root: "veles".into(),
                })
                .collect(),
        }
    }

    fn skills_cfg(mode: &str) -> crate::config::SkillsSection {
        crate::config::SkillsSection {
            index_mode: mode.into(),
            ..Default::default()
        }
    }

    /// `recent` lists what was actually used and COUNTS the rest — the
    /// model must never be left believing the short list is the whole
    /// toolbox.
    #[test]
    fn recent_mode_keeps_the_used_ones_and_counts_the_rest() {
        let set = set_of(50);
        let recent = vec!["skill-07".to_string(), "skill-31".to_string()];
        let mut cfg = skills_cfg("recent");
        cfg.index_max_entries = 3;
        let out = set.index_for(&cfg, &recent);

        // The used ones take seats FIRST — they survive a cap that only
        // has room for one more.
        assert!(out.contains("skill-07"), "a used skill is listed: {out}");
        assert!(out.contains("skill-31"), "{out}");
        assert!(!out.contains("skill-49"), "the cap really bites: {out}");
        assert!(
            out.contains("47 more skills") && out.contains("50 in total"),
            "the hidden ones are counted, not hidden silently: {out}"
        );
        assert!(out.contains("skills_list"), "and reachable: {out}");
    }

    /// Recency decides PRIORITY, not membership: a used skill outranks
    /// an unused one for a scarce seat, but spare seats are filled
    /// rather than left empty.
    #[test]
    fn recent_mode_backfills_spare_seats_up_to_the_cap() {
        let set = set_of(50);
        let mut cfg = skills_cfg("recent");
        cfg.index_max_entries = 10;
        let out = set.index_for(&cfg, &["skill-49".to_string()]);

        let listed = out
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .count();
        assert_eq!(listed, 10, "spare seats are filled, not wasted: {out}");
        assert!(out.contains("skill-49"), "the used one keeps its seat");
        assert!(out.contains("skill-00"), "and the rest backfill: {out}");
        assert!(out.contains("40 more skills"), "{out}");
    }

    /// The cap is a cap: more used skills than `index_max_entries`
    /// still yields at most that many lines.
    #[test]
    fn recent_mode_respects_index_max_entries() {
        let set = set_of(50);
        let recent: Vec<String> = (0..50).map(|i| format!("skill-{i:02}")).collect();
        let mut cfg = skills_cfg("recent");
        cfg.index_max_entries = 5;
        let out = set.index_for(&cfg, &recent);
        let listed = out
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .count();
        assert_eq!(listed, 5, "capped at index_max_entries: {out}");
        assert!(out.contains("45 more skills"), "{out}");
    }

    /// A fresh install has no usage history. It must still be able to
    /// name skills — listing nothing would make the feature invisible
    /// on day one — while still paying for only `index_max_entries`
    /// lines instead of all of them.
    #[test]
    fn recent_mode_without_history_still_lists_a_bounded_page() {
        let set = set_of(50);
        let mut cfg = skills_cfg("recent");
        cfg.index_max_entries = 30;
        let out = set.index_for(&cfg, &[]);

        let listed = out
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .count();
        assert_eq!(listed, 30, "a bounded page, not nothing and not all 50");
        assert!(out.contains("skill-00"), "real names, usable today: {out}");
        assert!(out.contains("20 more skills"), "{out}");
    }

    /// `index_max_entries = 0` is the "pointer only" setting inside
    /// recent mode, and must not silently mean "no limit".
    #[test]
    fn recent_mode_with_a_zero_cap_is_the_pointer_only() {
        let set = set_of(50);
        let mut cfg = skills_cfg("recent");
        cfg.index_max_entries = 0;
        let out = set.index_for(&cfg, &["skill-07".to_string()]);
        assert!(!out.contains("skill-07"), "nothing is listed: {out}");
        assert!(out.contains("50 are available"), "{out}");
    }

    /// `deferred` still has to tell the model the skills exist and how
    /// to find them — a pointer, not a disappearance.
    #[test]
    fn deferred_mode_still_tells_the_model_how_to_find_them() {
        let set = set_of(50);
        let out = set.index_for(&skills_cfg("deferred"), &["skill-07".to_string()]);
        assert!(!out.contains("skill-07"), "not even the used ones: {out}");
        assert!(out.contains("50 are available"), "{out}");
        assert!(out.contains("skills_list"), "{out}");
        assert!(out.contains("skill_view"), "and how to load one: {out}");
    }

    /// The one-line rollback: `index_mode = "full"` is byte-identical
    /// to the historical index, so reverting the whole step is a config
    /// edit rather than a code change.
    #[test]
    fn full_mode_is_byte_identical_to_today() {
        let set = set_of(50);
        assert_eq!(
            set.index_for(&skills_cfg("full"), &["skill-07".to_string()]),
            set.index(),
            "full mode is the historical index, unchanged"
        );
    }

    /// An unrecognised mode falls back to the DEFAULT, not to "full".
    /// A typo in config.toml quietly restoring the full dump is the
    /// kind of silent regression this whole step exists to remove.
    #[test]
    fn an_unknown_mode_falls_back_to_the_default_not_to_full() {
        let set = set_of(50);
        let recent = vec!["skill-07".to_string()];
        assert_eq!(
            set.index_for(&skills_cfg("recnt"), &recent),
            set.index_for(&skills_cfg("recent"), &recent),
        );
    }

    /// An empty set stays empty in every mode — the prompt must not
    /// grow a SKILLS banner announcing zero skills.
    #[test]
    fn an_empty_set_produces_no_block_in_any_mode() {
        let set = SkillSet { skills: vec![] };
        for mode in ["full", "recent", "deferred"] {
            assert!(
                set.index_for(&skills_cfg(mode), &[]).is_empty(),
                "{mode} on an empty set"
            );
        }
    }

    /// Every real skill_view load is recorded — this is the signal the
    /// `recent` index mode runs on. When it recorded ~2.5% of loads the
    /// seats went alphabetical and 107 of 137 skills were nameless in
    /// every prompt. A miss or a session-disabled skill records
    /// nothing: `recent` must mean "reached for", not "asked about".
    ///
    /// Asserted through `recent_skill_names`, the query the index
    /// actually consumes. It deliberately returns DISTINCT names (one
    /// row per call goes in; "how often" is answerable from the table,
    /// but the index only needs membership).
    #[tokio::test]
    async fn skill_view_records_every_real_load() {
        let (home, _env) = crate::testenv::scratch_home();
        let root = crate::config::config_dir().join("skills");
        skill_fixture(&root, "cat", "real-skill", "a skill that exists");
        skill_fixture(&root, "cat", "other-skill", "another one");
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("state.db")).unwrap(),
        ));
        let tool = SkillViewTool {
            session_id: "s1".into(),
            store: store.clone(),
        };
        use crate::tools::Tool;

        // Nothing recorded yet: the index would fall back to its
        // bounded alphabetical page.
        assert!(store
            .lock()
            .unwrap()
            .recent_skill_names(5)
            .unwrap()
            .is_empty());

        // A real load records, and a second load of a DIFFERENT skill
        // records too — the bug was loads silently not landing, so two
        // names in the recent list is the observation that matters.
        let out = tool.call(json!({"name": "real-skill"})).await;
        assert!(out.contains("real-skill"), "{out}");
        tool.call(json!({"name": "other-skill"})).await;
        let recent = store.lock().unwrap().recent_skill_names(5).unwrap();
        assert!(recent.contains(&"real-skill".to_string()), "{recent:?}");
        assert!(recent.contains(&"other-skill".to_string()), "{recent:?}");

        // A linked-file read of a real skill still records (it IS a
        // load of that skill) — and a miss records nothing, so
        // `recent` means "reached for", not "asked about".
        tool.call(json!({"name": "real-skill", "file_path": "refs/x.md"}))
            .await;
        tool.call(json!({"name": "nope-not-here"})).await;
        let recent = store.lock().unwrap().recent_skill_names(5).unwrap();
        assert!(
            !recent.iter().any(|n| n == "nope-not-here"),
            "a miss is not a use: {recent:?}"
        );
        assert!(recent.contains(&"real-skill".to_string()), "{recent:?}");
    }

    // ── paging (2026-09-19) ──────────────────────────────────────────

    /// A skill whose body is `lines` numbered lines, and the set that
    /// names it. Big enough to be cut when the caller wants it cut.
    fn paged_fixture(root: &Path, name: &str, lines: usize) -> SkillSet {
        let dir = root.join("cat").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut body = format!("---\nname: {name}\ndescription: \"d\"\n---\n");
        for i in 1..=lines {
            body.push_str(&format!("line {i} of the body\n"));
        }
        std::fs::write(dir.join("SKILL.md"), body).expect("write SKILL.md");
        SkillSet {
            skills: vec![SkillMeta {
                name: name.into(),
                description: "d".into(),
                category: "cat".into(),
                dir,
                root: "veles".into(),
            }],
        }
    }

    /// The warning is about ONE thing: this body will not survive
    /// `cap_tool_output`. Under the cap it must stay silent, or every
    /// skill load grows a line of noise.
    #[test]
    fn the_cap_warning_fires_only_when_the_body_will_not_fit() {
        let p = Path::new("/x/SKILL.md");
        assert!(
            cap_warning(p, 40_000, 900, 30_000).is_some(),
            "over the cap: the model is about to lose the middle"
        );
        assert!(
            cap_warning(p, 1_000, 30, 30_000).is_none(),
            "under the cap: nothing was cut, say nothing"
        );
        assert!(
            cap_warning(p, 10_000_000, 9, 0).is_none(),
            "cap 0 disables truncation entirely (agent.rs cap_tool_output)"
        );
        let w = cap_warning(p, 40_000, 900, 30_000).unwrap();
        assert!(w.contains("/x/SKILL.md"), "names the file: {w}");
        assert!(w.contains("offset"), "names the way out: {w}");
        assert!(w.contains("900"), "names the line count to page over: {w}");
    }

    /// `cap_tool_output` keeps the first 60% and the last 40% and drops
    /// the MIDDLE. So a warning is only useful if it is inside the
    /// first 60% — a notice about truncation that gets truncated is
    /// the joke this whole change exists to stop.
    ///
    /// USER 2026-09-19, on rust-agent-runtime: "is the middle of it
    /// just invisible forever now". It was: eleven whole sections,
    /// including the one about implementing tools.
    #[test]
    fn the_cap_warning_sits_where_the_cut_cannot_reach_it() {
        let root = tempfile::tempdir().unwrap();
        let set = paged_fixture(root.path(), "big", 4000);
        let out = view_skill_capped(&set, "big", None, None, None, 30_000);
        let at = out.find("offset").expect("the warning is present");
        assert!(
            at < 600,
            "the warning must land in the first lines, not at {at} where \
             the cut can eat it"
        );
        // And the head+tail cut really would eat the middle of this one.
        assert!(
            crate::agent::cap_tool_output(&out, 30_000).is_some(),
            "fixture must actually exceed the cap or this proves nothing"
        );
    }

    /// The header said "veles root" — the LABEL of the root, not the
    /// path. So an agent holding a truncated skill had no file to go
    /// read with `read_file`, and the middle really was unreachable.
    #[test]
    fn the_header_names_the_file_not_just_the_root_label() {
        let root = tempfile::tempdir().unwrap();
        let set = paged_fixture(root.path(), "small", 3);
        let out = view_skill(&set, "small", None, None, None);
        let want = set.get("small").unwrap().dir.join("SKILL.md");
        assert!(
            out.contains(&want.display().to_string()),
            "the path is the whole affordance: {out}"
        );
    }

    /// Paging is by LINE, through `filetools::render_read`, so
    /// `skill_view` and `read_file` are one contract rather than two
    /// that drift.
    #[test]
    fn skill_view_pages_by_line_and_says_where_to_resume() {
        let root = tempfile::tempdir().unwrap();
        let set = paged_fixture(root.path(), "big", 500);
        let out = view_skill(&set, "big", None, Some(10), Some(5));
        assert!(out.contains("10|"), "starts at the asked-for line: {out}");
        assert!(out.contains("14|"), "and runs `limit` lines: {out}");
        assert!(!out.contains("15|"), "and stops there: {out}");
        assert!(
            !out.contains("9|line 5"),
            "nothing before the offset: {out}"
        );
        assert!(
            out.contains("next offset=15"),
            "a page that is not the last one says how to continue: {out}"
        );
        // The header survives every page — a page without it cannot be
        // told apart from a page of some other skill.
        assert!(out.contains("big"), "header on every page: {out}");
    }

    #[test]
    fn an_offset_past_the_end_is_an_error_not_an_empty_page() {
        let root = tempfile::tempdir().unwrap();
        let set = paged_fixture(root.path(), "big", 20);
        let out = view_skill(&set, "big", None, Some(9999), None);
        assert!(
            out.contains("beyond end of file"),
            "silence would read as an empty skill: {out}"
        );
    }

    /// Back-compat, and it matters: the desktop skills pane
    /// (`desktop_api.rs`) and the TUI (`tui.rs`) render this body in a
    /// widget that scrolls on its own. An unrequested page would give
    /// both of them `N|` line-number gutters they never asked for.
    #[test]
    fn an_unpaged_view_is_still_the_raw_body() {
        let root = tempfile::tempdir().unwrap();
        let set = paged_fixture(root.path(), "small", 3);
        let out = view_skill(&set, "small", None, None, None);
        assert!(out.contains("line 1 of the body"), "{out}");
        assert!(
            !out.contains("1|line 1"),
            "no line-number gutter unless paging was asked for: {out}"
        );
    }

    #[test]
    fn a_linked_file_pages_too() {
        let root = tempfile::tempdir().unwrap();
        let set = paged_fixture(root.path(), "big", 5);
        let dir = set.get("big").unwrap().dir.clone();
        std::fs::create_dir_all(dir.join("references")).unwrap();
        let mut ref_body = String::new();
        for i in 1..=200 {
            ref_body.push_str(&format!("ref line {i}\n"));
        }
        std::fs::write(dir.join("references").join("r.md"), ref_body).unwrap();
        let out = view_skill(&set, "big", Some("references/r.md"), Some(3), Some(2));
        assert!(out.contains("3|ref line 3"), "{out}");
        assert!(out.contains("4|ref line 4"), "{out}");
        assert!(!out.contains("5|ref line 5"), "{out}");
    }

    /// The tool has to ADVERTISE the paging or the model never reaches
    /// for it — the same reason the description is the fix half of
    /// every tool defect in this file.
    #[test]
    fn the_tool_offers_the_paging_it_implements() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("state.db")).unwrap(),
        ));
        let tool = SkillViewTool {
            session_id: "s".into(),
            store,
        };
        let schema = crate::tools::Tool::schema(&tool);
        let props = &schema["properties"];
        assert!(props.get("offset").is_some(), "{schema}");
        assert!(props.get("limit").is_some(), "{schema}");
        let desc = crate::tools::Tool::description(&tool);
        assert!(
            desc.contains("offset"),
            "the description is where the model looks first: {desc}"
        );
    }
}
