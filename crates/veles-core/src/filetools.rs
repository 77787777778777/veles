//! File tools — the hermes file-operation parity set (read_file,
//! write_file, patch, search_files).
//!
//! These are the tools that let the agent actually inspect and change
//! the filesystem. Implemented to mirror hermes' semantics:
//!   read_file    — line numbers, offset/limit pagination, char budget
//!   write_file   — whole-file overwrite (auto mkdir -p)
//!   patch        — targeted find/replace (fuzzy-ish exact match; the
//!                  full 9-strategy fuzzy matcher is later)
//!   search_files — ripgrep-style content search OR filename glob
//!
//! Security: paths are resolved and confined to the process cwd unless
//! absolute; no special hermes-guarded paths are honored here (veles
//! home edits go through config/memory layers).

use crate::lockutil::LockOk;
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Resolve a user-supplied path against the process working directory.
/// Absolute paths pass through; relative join onto cwd. `~` expands to
/// $HOME (the agent's notion of home, not necessarily /home/x).
pub(crate) fn resolve(path: &str) -> PathBuf {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        format!("{home}/{rest}")
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    } else {
        path.to_string()
    };
    let p = PathBuf::from(expanded);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

// ── read_file ──────────────────────────────────────────────────────────

/// `session` routes the call: a below-admin session resolves to its
/// user env (`user_envs::zone_for`) and the read happens INSIDE the
/// container; None / owner sessions read the host, as always.
#[derive(Default)]
pub struct ReadFileTool {
    pub session: Option<String>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file with line numbers and pagination. Use instead of cat/head/tail. \
Returns lines as 'N|CONTENT'. Use offset (1-indexed start line) and limit (max lines, \
default 2000) for large files. Cannot read binary files."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to read (absolute, relative, or ~/path)"},
                "offset": {"type": "integer", "description": "1-indexed start line (default 1)", "minimum": 1},
                "limit": {"type": "integer", "description": "Max lines (default 2000)", "minimum": 1},
                "host": {"type": "boolean", "description": "Read on the HOST instead of a sandbox. Admin/owner only and ignored below that; omit it and the call is sandboxed."}, "sandbox": {"type": "string", "description": "Name of a sandbox to read in instead of this session's — one you may reach."}
            },
            "required": ["path"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return "error: path is required".into();
        };
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2000)
            .min(2000) as usize;

        // ── sandbox zone: the path resolves INSIDE the container —
        // symlinks there point at decoys, and `podman cp` is the only
        // thing that crosses the fence. Host-side reads through the
        // bind mount would follow a planted symlink onto the HOST.
        if let crate::user_envs::Zone::Sandbox { key, name } = crate::user_envs::zone_for(
            self.session.as_deref(),
            crate::user_envs::ZoneArg::from_args(&args),
        ) {
            let cpath = crate::user_envs::container_resolve(path);
            let content = match crate::user_envs::read_bytes(&key, &cpath).await {
                Ok(b) => b,
                Err(e) => {
                    return crate::user_envs::annotate(&name, format!("error reading {cpath}: {e}"))
                }
            };
            crate::user_envs::record_read(&key, &cpath, &content);
            return crate::user_envs::annotate(
                &name,
                render_read(&cpath, &content, offset, limit, None),
            );
        }

        let p = resolve(path);
        let content = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) => return format!("error reading {}: {e}", p.display()),
        };
        // Harness: record the read baseline (stale-write guard) and
        // check for a context file in this directory (subdir hints).
        crate::harness::file_state_ledger()
            .lock()
            .unwrap()
            .record_read(path, crate::harness::mtime_ms(&p));
        let hint = crate::harness::subdir_hints()
            .lock()
            .unwrap()
            .hint_for(path, crate::harness::CONTEXT_FILE_NAMES);
        render_read(&p.display().to_string(), &content, offset, limit, hint)
    }
}

/// The shared read renderer (pagination, binary sniff, footer) — the
/// host and sandbox zones byte-identical semantics, so both feed this.
/// Shared with `skills::view_skill`, deliberately: `skill_view` pages
/// the same way `read_file` does — same `offset`/`limit` meaning, same
/// `N|CONTENT` shape, same "next offset=" footer — so the model learns
/// one contract instead of two that drift.
pub(crate) fn render_read(
    label: &str,
    content: &[u8],
    offset: usize,
    limit: usize,
    hint: Option<std::path::PathBuf>,
) -> String {
    // Binary sniff: NUL byte in the first 8KB.
    if content[..content.len().min(8192)].contains(&0) {
        return format!("error: {label} is a binary file — use a decoder, not read_file");
    }
    let text = match String::from_utf8(content.to_vec()) {
        Ok(t) => t,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };

    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    // Drop the trailing empty element from a final newline for the
    // count, but keep indexing consistent.
    let total_display = if text.ends_with('\n') {
        total - 1
    } else {
        total
    };
    if offset > total_display + 1 {
        return format!("error: offset {offset} beyond end of file ({total_display} lines)");
    }

    let end = (offset - 1 + limit).min(total_display);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(offset - 1) {
        out.push_str(&format!("{}|{}\n", i + 1, line));
    }
    let mut footer = format!(
        "(read lines {offset}-{end} of {total_display}; {} bytes)",
        text.len()
    );
    if end < total_display {
        footer.push_str(&format!("; more available — next offset={}", end + 1));
    }
    if let Some(ctx) = hint {
        footer.push_str(&format!(
            "\n[context] {} exists in this directory — read it for local conventions",
            ctx.display()
        ));
    }
    format!("{out}{footer}")
}

// ── write_file ─────────────────────────────────────────────────────────

/// `ckpt` is the pre-mutation snapshot context (M13). None disables
/// checkpointing for this tool instance (tests, ad-hoc registries).
/// `session` routes the write (see `ReadFileTool`).
#[derive(Default)]
pub struct WriteFileTool {
    pub ckpt: Option<crate::checkpoints::CheckpointCtx>,
    pub session: Option<String>,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file, completely replacing existing content. Creates parent \
directories automatically. OVERWRITES the entire file — use patch for targeted edits."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to write (created if missing)"},
                "content": {"type": "string", "description": "Complete content"},
                "host": {"type": "boolean", "description": "Write on the HOST instead of a sandbox. Admin/owner only and ignored below that; omit it and the call is sandboxed."}, "sandbox": {"type": "string", "description": "Name of a sandbox to write in instead of this session's — one you may reach."}
            },
            "required": ["path", "content"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let (Some(path), Some(content)) = (
            args.get("path").and_then(Value::as_str),
            args.get("content").and_then(Value::as_str),
        ) else {
            return "error: path and content are required".into();
        };
        // ── sandbox zone: the write lands in the caller's env
        // container (workspace = /work, persistent). The protected-
        // path list and host checkpoints do not apply — those guard
        // THE HOST's credentials and undo history; the container has
        // neither. The stale-write guard DOES apply, hash-based.
        if let crate::user_envs::Zone::Sandbox { key, name } = crate::user_envs::zone_for(
            self.session.as_deref(),
            crate::user_envs::ZoneArg::from_args(&args),
        ) {
            let cpath = crate::user_envs::container_resolve(path);
            if let Ok(cur) = crate::user_envs::read_bytes(&key, &cpath).await {
                if crate::user_envs::changed_since_read(&key, &cpath, &cur) {
                    return crate::user_envs::annotate(
                        &name,
                        format!(
                            "error: refusing to write {cpath} — file changed since last read \
(concurrent edit? your own terminal edit counts too). read_file it again, then write."
                        ),
                    );
                }
            }
            if let Err(e) = crate::user_envs::write_bytes(&key, &cpath, content.as_bytes()).await {
                return crate::user_envs::annotate(&name, format!("error writing {cpath}: {e}"));
            }
            // Refresh the baseline to our own write.
            crate::user_envs::record_read(&key, &cpath, content.as_bytes());
            return crate::user_envs::annotate(
                &name,
                format!("wrote {} bytes to {cpath}", content.len()),
            );
        }

        let p = resolve(path);
        // File safety (harness family): never let the model overwrite
        // credentials, the live DB, or key material — even when asked.
        if let Some(reason) = crate::harness::protected_path_reason(
            path,
            &crate::config::config_dir(),
            &crate::config::data_dir(),
        ) {
            return format!("error: refusing to write {path} — protected ({reason})");
        }
        // Stale-write guard: if the file changed since this process
        // last read it, refuse and demand a re-read first.
        {
            let ledger = crate::harness::file_state_ledger().lock_ok();
            if ledger.check_write(path, crate::harness::mtime_ms(&p))
                == crate::harness::WriteCheck::ChangedExternally
            {
                return format!(
                    "error: refusing to write {path} — file changed since last read (concurrent \
edit? your own terminal edit counts too). read_file it again, then write."
                );
            }
        }
        if let Some(parent) = p.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("error creating {}: {e}", parent.display());
            }
        }
        // Undo point BEFORE the overwrite (M13). Best-effort: a failed
        // snapshot logs and the write still happens.
        if let Some(c) = &self.ckpt {
            crate::checkpoints::snapshot(c, &p, "write_file", None);
        }
        match std::fs::write(&p, content) {
            Ok(()) => {
                // Refresh the stale-write baseline to our own write.
                crate::harness::file_state_ledger()
                    .lock()
                    .unwrap()
                    .record_write(path, crate::harness::mtime_ms(&p));
                format!("wrote {} bytes to {}", content.len(), p.display())
            }
            Err(e) => format!("error writing {}: {e}", p.display()),
        }
    }
}

// ── patch ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct PatchTool {
    pub ckpt: Option<crate::checkpoints::CheckpointCtx>,
    pub session: Option<String>,
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }
    fn description(&self) -> &str {
        "Targeted find-and-replace edit in a file. Replaces the first unique occurrence of \
old_string with new_string. Prefer this over write_file for small edits. Include enough \
surrounding context in old_string to be unique. `path` may be omitted to edit the file \
from your previous file call — the result notes when a path was inferred."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "old_string": {"type": "string", "description": "Exact text to find (must be unique)"},
                "new_string": {"type": "string", "description": "Replacement text (empty to delete)"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default false)"},
                "host": {"type": "boolean", "description": "Patch on the HOST instead of a sandbox. Admin/owner only and ignored below that; omit it and the call is sandboxed."}, "sandbox": {"type": "string", "description": "Name of a sandbox to patch in instead of this session's — one you may reach."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let (Some(path), Some(old), Some(new)) = (
            args.get("path").and_then(Value::as_str),
            args.get("old_string").and_then(Value::as_str),
            args.get("new_string").and_then(Value::as_str),
        ) else {
            return "error: path, old_string, new_string are required".into();
        };
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // ── sandbox zone (same contract as write_file's) ──
        if let crate::user_envs::Zone::Sandbox { key, name } = crate::user_envs::zone_for(
            self.session.as_deref(),
            crate::user_envs::ZoneArg::from_args(&args),
        ) {
            let cpath = crate::user_envs::container_resolve(path);
            let bytes = match crate::user_envs::read_bytes(&key, &cpath).await {
                Ok(b) => b,
                Err(e) => {
                    return crate::user_envs::annotate(&name, format!("error reading {cpath}: {e}"))
                }
            };
            if crate::user_envs::changed_since_read(&key, &cpath, &bytes) {
                return crate::user_envs::annotate(
                    &name,
                    format!(
                        "error: refusing to patch {cpath} — file changed since last read \
(concurrent edit? your own terminal edit counts too). read_file it again, then patch."
                    ),
                );
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            match patch_text(&text, old, new, replace_all) {
                None => {
                    return crate::user_envs::annotate(
                        &name,
                        patch_text_error(&text, old, replace_all, &cpath),
                    )
                }
                Some((new_text, n)) => {
                    if let Err(e) =
                        crate::user_envs::write_bytes(&key, &cpath, new_text.as_bytes()).await
                    {
                        return crate::user_envs::annotate(
                            &name,
                            format!("error writing {cpath}: {e}"),
                        );
                    }
                    crate::user_envs::record_read(&key, &cpath, new_text.as_bytes());
                    return crate::user_envs::annotate(
                        &name,
                        format!(
                            "patched {cpath} ({n} replacement{})\n{}",
                            if n == 1 { "" } else { "s" },
                            patch_diff(&text, old, new, replace_all, &cpath)
                        ),
                    );
                }
            }
        }

        // File safety: same rule as write_file (harness family).
        if let Some(reason) = crate::harness::protected_path_reason(
            path,
            &crate::config::config_dir(),
            &crate::config::data_dir(),
        ) {
            return format!("error: refusing to patch {path} — protected ({reason})");
        }

        let p = resolve(path);
        // Stale-write guard (same as write_file).
        {
            let ledger = crate::harness::file_state_ledger().lock_ok();
            if ledger.check_write(path, crate::harness::mtime_ms(&p))
                == crate::harness::WriteCheck::ChangedExternally
            {
                return format!(
                    "error: refusing to patch {path} — file changed since last read (concurrent \
edit? your own terminal edit counts too). read_file it again, then patch."
                );
            }
        }
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) => return format!("error reading {}: {e}", p.display()),
        };
        let Some((new_text, n)) = patch_text(&text, old, new, replace_all) else {
            return patch_text_error(&text, old, replace_all, &p.display().to_string());
        };
        if let Some(c) = &self.ckpt {
            crate::checkpoints::snapshot(c, &p, "patch", None);
        }
        if let Err(e) = std::fs::write(&p, &new_text) {
            return format!("error writing {}: {e}", p.display());
        }
        // Refresh the stale-write baseline to our own edit.
        crate::harness::file_state_ledger()
            .lock()
            .unwrap()
            .record_write(path, crate::harness::mtime_ms(&p));
        // The diff rides the RESULT so every surface gets it — the
        // desktop's per-tool renderer, IRC, Telegram, the CLI — and the
        // model reads back what actually changed instead of trusting a
        // summary. Computed from the match offsets (the tool already
        // knows them exactly), so it is hunk-accurate for any
        // alignment, not a positional approximation.
        format!(
            "patched {} ({n} replacement{})\n{}",
            p.display(),
            if n == 1 { "" } else { "s" },
            patch_diff(&text, old, new, replace_all, &p.display().to_string())
        )
    }
}

/// The patch core: find/replace with the uniqueness rule, shared by
/// the host and sandbox zones so the semantics cannot drift. None
/// means refused (empty/absent/ambiguous old_string — the message
/// from [`patch_text_error`] names which).
fn patch_text(text: &str, old: &str, new: &str, replace_all: bool) -> Option<(String, usize)> {
    if old.is_empty() {
        return None;
    }
    let count = text.matches(old).count();
    if count == 0 {
        return None;
    }
    if count > 1 && !replace_all {
        return None;
    }
    let new_text = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    Some((new_text, if replace_all { count } else { 1 }))
}

/// Why [`patch_text`] refused, in the historical wording.
fn patch_text_error(text: &str, old: &str, replace_all: bool, label: &str) -> String {
    if old.is_empty() {
        return "error: old_string must not be empty".into();
    }
    let count = text.matches(old).count();
    if count == 0 {
        return format!("error: old_string not found in {label}");
    }
    debug_assert!(count > 1 && !replace_all);
    format!(
        "error: old_string matches {count} times — add context to make it unique, \
or set replace_all=true"
    )
}

/// A hunk-accurate unified diff of exactly the replacements `patch` is
/// about to report, built from the match offsets rather than a
/// whole-file walk — the tool knows where every replacement lands, so
/// there is no need to guess (and a positional diff WOULD guess: its
/// `-`/`+` pairs drift on any edit that shifts lines, and this text is
/// read back by the model, where an inexact diff actively misleads).
///
/// Non-line-aligned replacements are handled by expanding to the whole
/// affected lines on the `-` side and splicing the new text into them
/// for the `+` side — which is literally what the file now contains.
fn patch_diff(text: &str, old: &str, new: &str, replace_all: bool, path: &str) -> String {
    const CTX: usize = 3;
    const MAX_LINES: usize = 60;

    // Byte offset of each line start (line 0 at 0, one past each '\n').
    let mut line_starts: Vec<usize> = Vec::with_capacity(text.len() / 32 + 1);
    line_starts.push(0);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let n_lines = line_starts.len();
    // `line_starts` counts a phantom empty line after a trailing
    // newline; context must never extend into it (it would emit a
    // phantom blank context line and inflate the hunk count).
    let last_line = if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        n_lines - 2
    } else {
        n_lines - 1
    };
    let line_of = |off: usize| -> usize {
        match line_starts.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    };
    // The line's text WITHOUT its newline.
    let line_text = |li: usize| -> &str {
        let end = if li + 1 < n_lines {
            line_starts[li + 1] - 1
        } else {
            text.len()
        };
        text.get(line_starts[li]..end).unwrap_or("")
    };

    // Every occurrence's byte range, in order.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(old) {
        let s = from + rel;
        ranges.push((s, s + old.len()));
        if !replace_all {
            break;
        }
        from = s + old.len();
    }
    if ranges.is_empty() {
        return String::new();
    }

    // Affected line span per occurrence, merged when close enough that
    // one hunk's context would cover both anyway.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in &ranges {
        let l0 = line_of(s);
        let l1 = line_of(e.saturating_sub(1).max(s));
        match groups.last_mut() {
            Some(g) if l0 <= g.1 + 2 * CTX + 1 => {
                if l1 > g.1 {
                    g.1 = l1;
                }
            }
            _ => groups.push((l0, l1)),
        }
    }

    let mut out = format!("--- {path}\n+++ {path}\n");
    let mut shown = 0usize;
    let mut truncated = false;
    for &(g0, g1) in &groups {
        if truncated {
            break;
        }
        // The group's slice of the file, with every replacement inside
        // it applied: this IS the "+" side.
        let gs = line_starts[g0];
        let ge = if g1 + 1 < n_lines {
            line_starts[g1 + 1]
        } else {
            text.len()
        };
        let mut new_side = String::new();
        let mut cursor = gs;
        for &(s, e) in ranges.iter().filter(|&&(s, _)| s >= gs && s < ge) {
            new_side.push_str(&text[cursor..s]);
            new_side.push_str(new);
            cursor = e;
        }
        new_side.push_str(&text[cursor..ge]);

        let c0 = g0.saturating_sub(CTX);
        let c1 = (g1 + CTX).min(last_line);
        let old_cnt = c1 - c0 + 1;
        let new_lines: Vec<&str> = if new_side.is_empty() {
            Vec::new()
        } else {
            new_side.lines().collect()
        };
        let new_cnt = old_cnt - (g1 - g0 + 1) + new_lines.len();
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            c0 + 1,
            old_cnt,
            c0 + 1,
            new_cnt
        ));
        let mut emit = |prefix: char, body: &str, shown: &mut usize, trunc: &mut bool| {
            if *shown >= MAX_LINES {
                *trunc = true;
                return;
            }
            out.push_str(&format!("{prefix}{body}\n"));
            *shown += 1;
        };
        for li in c0..g0 {
            emit(' ', line_text(li), &mut shown, &mut truncated);
        }
        for li in g0..=g1 {
            emit('-', line_text(li), &mut shown, &mut truncated);
        }
        for l in &new_lines {
            emit('+', l, &mut shown, &mut truncated);
        }
        for li in (g1 + 1)..=c1 {
            emit(' ', line_text(li), &mut shown, &mut truncated);
        }
    }
    if truncated {
        out.push_str("… [diff truncated]\n");
    }
    out
}

// ── search_files ───────────────────────────────────────────────────────

/// Which `rg` the HOST-side search runs. The sandbox zone below shells
/// into a container with its own `rg` on argv, so this never reaches it.
///
/// `VELES_RG` first — the Android app sets it to the APK's librg.so
/// (Engine.java), the same way `VELES_TCLSH` names its tclsh, because
/// Android extracts executables only as `lib*.so` into the app's native
/// library directory and there is no `rg` on a phone's PATH. Then a bare
/// `rg`, which is what every other platform resolves through PATH (the
/// flake puts ripgrep there; NixOS' system one, Termux's `pkg install
/// ripgrep`). An unset or empty var must not turn into an empty argv[0].
pub fn rg_path() -> String {
    std::env::var("VELES_RG")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "rg".to_string())
}

/// `session` routes the search (see `ReadFileTool`).
#[derive(Default)]
pub struct SearchFilesTool {
    pub session: Option<String>,
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }
    fn description(&self) -> &str {
        "Search file contents (ripgrep, target='content') or find files by name \
(target='files', glob). Use instead of grep/rg/find/ls. Returns matches with file paths \
and line numbers (content mode) or matching paths (files mode)."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex (content) or glob (files)"},
                "path": {"type": "string", "description": "Directory/file to search (default '.')"},
                "target": {"type": "string", "enum": ["content", "files"], "description": "content=search inside files, files=find by name"},
                "limit": {"type": "integer", "description": "Max results (default 50)"},
                "host": {"type": "boolean", "description": "Search on the HOST instead of a sandbox. Admin/owner only and ignored below that; omit it and the call is sandboxed."}, "sandbox": {"type": "string", "description": "Name of a sandbox to search in instead of this session's — one you may reach."}
            },
            "required": ["pattern"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return "error: pattern is required".into();
        };
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("content");
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;

        // ── sandbox zone: rg runs INSIDE the container, argv-mode (no
        // shell ⇒ no quoting surface), against the container's
        // filesystem. The image carries rg; if it does not, the error
        // names the fix (rebuild scripts/build-sandbox-image.sh).
        //
        // The literal `"rg"` below is the CONTAINER's binary and stays
        // literal on purpose: `rg_path` (VELES_RG) names this host's rg,
        // which is the wrong one inside another filesystem — on a phone
        // it would hand podman a path in the app's private storage.
        // See rg_path.
        if let crate::user_envs::Zone::Sandbox { key, name } = crate::user_envs::zone_for(
            self.session.as_deref(),
            crate::user_envs::ZoneArg::from_args(&args),
        ) {
            let root = crate::user_envs::container_resolve(path);
            let argv: Vec<String> = if target == "content" {
                vec![
                    "rg".into(),
                    "-n".into(),
                    "--no-heading".into(),
                    "--max-count".into(),
                    "5".into(),
                    pattern.to_string(),
                    root,
                ]
            } else {
                vec!["rg".into(), "--files".into(), root]
            };
            let out = match crate::user_envs::exec_argv(&key, &argv, 120).await {
                Ok(o) => o,
                Err(e) => {
                    return crate::user_envs::annotate(&name, format!("error: user env: {e:#}"))
                }
            };
            // Same acceptance rule as the host path below: exit 1 is
            // "no matches", exit 2 with stdout is partial results.
            if out.exit_code == 0 || out.exit_code == 1 || !out.stdout.is_empty() {
                return crate::user_envs::annotate(
                    &name,
                    with_skips(
                        search_body(&out.stdout, target, pattern, limit),
                        summarize_rg_stderr(&out.stderr),
                    ),
                );
            }
            return crate::user_envs::annotate(
                &name,
                format!(
                    "rg error: {}",
                    summarize_rg_stderr(&out.stderr).unwrap_or_else(|| "unknown failure".into())
                ),
            );
        }

        let root = resolve(path);
        let root_str = root.display().to_string();

        // Shell out to rg (ripgrep is ubiquitous on this box; on a phone
        // `rg_path` is the APK's own librg.so). For content search:
        // rg -n --no-heading. For filename search: rg --files | glob
        // match. Fall back gracefully if rg is absent.
        let rg = rg_path();
        let output = if target == "content" {
            std::process::Command::new(&rg)
                .args(["-n", "--no-heading", "--max-count", "5", pattern, &root_str])
                .output()
        } else {
            std::process::Command::new(&rg)
                .args(["--files", &root_str])
                .output()
        };

        match output {
            // Exit 2 means rg could not read SOMETHING — a locked
            // directory, a dangling symlink — and it says so on stderr
            // while STILL printing every match it did find on stdout.
            // Treating that as a failure threw the matches away and
            // returned the stderr instead: a real search silently
            // became "rg error:" followed by kilobytes of "Permission
            // denied". Measured on one live turn: 8,082 characters of
            // it, 57 identical-shaped lines, zero matches carried.
            // Partial results are results.
            Ok(o) if o.status.success() || o.status.code() == Some(1) || !o.stdout.is_empty() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let skipped = summarize_rg_stderr(&String::from_utf8_lossy(&o.stderr));
                with_skips(search_body(&stdout, target, pattern, limit), skipped)
            }
            Ok(o) => format!(
                "rg error: {}",
                summarize_rg_stderr(&String::from_utf8_lossy(&o.stderr))
                    .unwrap_or_else(|| "unknown failure".into())
            ),
            Err(e) => format!("error: could not run rg ({e}) — is ripgrep installed?"),
        }
    }
}

/// The shared result shaper for both zones: content lines or glob
/// hits, capped at `limit`.
fn search_body(stdout: &str, target: &str, pattern: &str, limit: usize) -> String {
    if target == "files" {
        // glob match against basenames
        let glob = glob_to_regex(pattern);
        let re = match regex::Regex::new(&glob) {
            Ok(r) => r,
            Err(e) => return format!("error: bad glob {pattern}: {e}"),
        };
        let mut hits = Vec::new();
        for line in stdout.lines() {
            let base = Path::new(line)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if re.is_match(&base) {
                hits.push(line.to_string());
                if hits.len() >= limit {
                    break;
                }
            }
        }
        if hits.is_empty() {
            format!("no files matching {pattern}")
        } else {
            hits.join("\n")
        }
    } else {
        let lines: Vec<&str> = stdout.lines().take(limit).collect();
        if lines.is_empty() {
            format!("no matches for {pattern}")
        } else {
            lines.join("\n")
        }
    }
}

/// Append the skip note to a search result, if there is one.
fn with_skips(body: String, skipped: Option<String>) -> String {
    match skipped {
        Some(s) => format!("{body}\n[{s}]"),
        None => body,
    }
}

/// Collapse ripgrep's stderr into ONE line per distinct reason.
///
/// rg emits one line per unreadable path, and a home directory with a
/// few root-owned build trees in it produces dozens of them — 8,082
/// characters on the turn that prompted this, every line saying the
/// same thing about a different path. The model does not need the
/// paths; it needs to know its search did not cover everything, which
/// is one line. The tokens are not the only cost: that spew is billed
/// uncached when it lands and then re-sent, cached, every round after.
///
/// Anything that is NOT a recognisable per-path skip is passed through
/// (capped), because an unrecognised rg error is a real error and
/// summarising it away would be the silent-failure habit again.
pub(crate) fn summarize_rg_stderr(stderr: &str) -> Option<String> {
    let mut skips: std::collections::BTreeMap<String, usize> = Default::default();
    let mut other: Vec<&str> = Vec::new();
    for line in stderr.lines().filter(|l| !l.trim().is_empty()) {
        // "rg: <path>: <reason> (os error N)" — the reason is what
        // repeats. The trailing "(os error N)" is what distinguishes a
        // per-path skip from a real diagnostic: `rg: regex parse error:
        // unclosed group` has the same colon shape and must NOT be
        // collapsed into a skip count (the test that caught this is
        // `repeated_rg_skips_collapse_to_a_count`).
        let reason = line
            .strip_prefix("rg: ")
            .filter(|rest| rest.ends_with(')') && rest.contains("(os error "))
            .and_then(|rest| rest.rsplit_once(": ").map(|(_, r)| r));
        match reason {
            Some(r) => *skips.entry(r.to_string()).or_default() += 1,
            None => other.push(line),
        }
    }
    let mut parts: Vec<String> = skips
        .into_iter()
        .map(|(reason, n)| format!("rg skipped {n} path(s): {reason}"))
        .collect();
    // Unrecognised lines are real errors: keep a few, verbatim.
    for l in other.iter().take(3) {
        parts.push((*l).to_string());
    }
    if other.len() > 3 {
        parts.push(format!("... and {} more rg messages", other.len() - 3));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Convert a simple glob (supporting * and ?) to a regex matching the
/// whole basename. Good enough for filename searches (and for the
/// approval allowlist, which matches whole commands the same way).
pub(crate) fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    for c in glob.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c if "\\^$.|+()[]{}".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    re
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ripgrep exits 2 when it cannot read something — a root-owned
    /// build tree, a locked directory — and prints its findings on
    /// stdout ANYWAY. Treating that as a failure threw the matches away
    /// and returned the stderr instead. On the turn that prompted this
    /// fix, a single search returned 8,082 characters of "Permission
    /// denied" and zero matches.
    #[tokio::test]
    async fn a_search_that_cannot_read_everything_still_returns_what_it_found() {
        let dir = std::env::temp_dir().join(format!("veles-rgpartial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("readable")).unwrap();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("readable/f.txt"), "the needle is here\n").unwrap();
        std::fs::write(dir.join("locked/g.txt"), "needle\n").unwrap();
        // Make one subtree unreadable, which is what makes rg exit 2.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
                .unwrap();
        }

        let out = SearchFilesTool::default()
            .call(json!({"pattern": "needle", "path": dir.display().to_string()}))
            .await;

        // Restore before asserting, so a failure cannot leave the tree
        // undeletable for the next run.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                dir.join("locked"),
                std::fs::Permissions::from_mode(0o755),
            );
        }

        assert!(
            out.replace('\\', "/").contains("readable/f.txt"),
            "the match it DID find must survive: {out}"
        );
        assert!(
            !out.starts_with("rg error"),
            "a partial read is not a failed search: {out}"
        );
        // …and the skip is reported, because a search that did not
        // cover everything must not look like one that did. (unix only:
        // the unreadable subtree above is a chmod, which Windows has no
        // equivalent of here — there rg reads everything and reports no
        // skip, correctly.)
        #[cfg(unix)]
        assert!(
            out.contains("rg skipped"),
            "the incomplete coverage must be stated: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half: one line per REASON, not one per path.
    #[test]
    fn repeated_rg_skips_collapse_to_a_count() {
        let noisy: String = (0..57)
            .map(|i| format!("rg: /home/u/dir{i}/x: Permission denied (os error 13)\n"))
            .collect();
        assert!(noisy.len() > 2000, "the input is the shape we measured");
        let s = summarize_rg_stderr(&noisy).expect("a summary");
        assert_eq!(s, "rg skipped 57 path(s): Permission denied (os error 13)");
        assert!(s.len() < 80, "8k of spew becomes one short line");

        // Distinct reasons stay distinct.
        let mixed = "rg: /a/b: Permission denied (os error 13)\n\
                     rg: /c/d: Permission denied (os error 13)\n\
                     rg: /e/f: No such file or directory (os error 2)\n";
        let s = summarize_rg_stderr(mixed).unwrap();
        assert!(s.contains("2 path(s): Permission denied"), "{s}");
        assert!(s.contains("1 path(s): No such file"), "{s}");

        // An unrecognised message is a REAL error and passes through —
        // summarising it away would be the silent-failure habit again.
        let real = "rg: regex parse error: unclosed group\n";
        assert_eq!(summarize_rg_stderr(real).unwrap(), real.trim());

        assert!(summarize_rg_stderr("").is_none(), "silence stays silent");
    }

    #[test]
    fn glob_basics() {
        assert_eq!(glob_to_regex("*.rs"), "^.*\\.rs$");
        assert_eq!(glob_to_regex("main.?"), "^main\\..$");
        assert_eq!(glob_to_regex("a*b"), "^a.*b$");
    }

    #[test]
    fn resolve_home_expansion() {
        let home = std::env::var("HOME").unwrap();
        let p = resolve("~/foo/bar.txt");
        assert!(p.to_string_lossy().starts_with(&home));
    }

    /// `VELES_RG` names the rg the host-side search runs — the Android
    /// app's librg.so (Engine.java) — and a bare `rg` is the fallback
    /// everywhere else. An empty var must not become an empty argv[0].
    /// VELES_RG is process-global and cargo runs tests in parallel, so
    /// this holds the env lock and restores the var it read.
    #[test]
    fn rg_path_follows_veles_rg_and_falls_back_to_bare_rg() {
        let _g = crate::testenv::lock();
        let prev = std::env::var("VELES_RG").ok();
        let restore = || match &prev {
            Some(v) => std::env::set_var("VELES_RG", v),
            None => std::env::remove_var("VELES_RG"),
        };

        std::env::remove_var("VELES_RG");
        assert_eq!(rg_path(), "rg", "no var: a bare rg, resolved on PATH");

        std::env::set_var("VELES_RG", "");
        assert_eq!(
            rg_path(),
            "rg",
            "an empty var is not an empty argv[0]; fall back"
        );

        std::env::set_var("VELES_RG", "/data/app/~~/lib/arm64/librg.so");
        assert_eq!(
            rg_path(),
            "/data/app/~~/lib/arm64/librg.so",
            "the app's own librg.so, verbatim"
        );

        restore();
    }

    #[tokio::test]
    async fn write_refuses_protected_paths() {
        // Reads the process-global VELES_HOME and attempts writes
        // beneath it, so it needs one of its own — otherwise it pokes
        // at whichever scratch home another test currently has set.
        let (home_dir, _env) = crate::testenv::scratch_home();
        let home = home_dir.path().to_path_buf();
        let env_path = home.join(".env");
        let w = WriteFileTool::default();
        let res = w
            .call(json!({"path": env_path.display().to_string(), "content": "overwrite attempt"}))
            .await;
        assert!(res.starts_with("error: refusing"), "result: {res}");
        let p = PatchTool::default();
        let res = p
            .call(json!({
                "path": env_path.display().to_string(),
                "old_string": "x",
                "new_string": "y"
            }))
            .await;
        assert!(res.starts_with("error: refusing"), "result: {res}");
    }

    #[tokio::test]
    async fn stale_write_guard_trips() {
        let dir = std::env::temp_dir().join("veles-stale-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("stale.txt");
        std::fs::write(&path, "original").unwrap();
        let ps = path.display().to_string();

        // Read establishes the baseline.
        let r = ReadFileTool::default();
        let res = r.call(json!({"path": ps})).await;
        assert!(res.contains("original"), "read: {res}");

        // External mutation bumps the mtime (std-only, no extra deps).
        let now = std::time::SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now + std::time::Duration::from_secs(60))
            .unwrap();

        let w = WriteFileTool::default();
        let res = w
            .call(json!({"path": ps, "content": "overwrite attempt"}))
            .await;
        assert!(
            res.contains("changed since last read"),
            "stale guard: {res}"
        );

        // Re-reading clears the stale state; the write then succeeds.
        let _ = r.call(json!({"path": ps})).await;
        let res = w.call(json!({"path": ps, "content": "fresh write"})).await;
        assert!(res.contains("wrote"), "post-reread write: {res}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let dir = std::env::temp_dir().join("veles-filetool-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rt.txt");

        let w = WriteFileTool::default();
        let res = w
            .call(json!({"path": path.display().to_string(), "content": "alpha\nbeta\ngamma"}))
            .await;
        assert!(res.contains("wrote"), "write result: {res}");

        let r = ReadFileTool::default();
        let res = r.call(json!({"path": path.display().to_string()})).await;
        assert!(res.contains("1|alpha"));
        assert!(res.contains("3|gamma"));
        assert!(res.contains("of 3"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn patch_unique_and_ambiguous() {
        let dir = std::env::temp_dir().join("veles-filetool-test2");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("p.txt");
        std::fs::write(&path, "one\ntwo\none\n").unwrap();

        let p = PatchTool::default();
        // ambiguous without replace_all
        let res = p
            .call(json!({"path": path.display().to_string(), "old_string": "one", "new_string": "uno"}))
            .await;
        assert!(res.contains("matches 2 times"), "res: {res}");

        // unique replacement
        let res = p
            .call(json!({"path": path.display().to_string(), "old_string": "two", "new_string": "dos"}))
            .await;
        assert!(res.contains("patched"), "res: {res}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ndos\none\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The patch RESULT carries a hunk-accurate unified diff — every
    /// surface (desktop renderer, IRC, Telegram, CLI) and the model
    /// itself read what actually changed, instead of a summary that
    /// says a change happened without showing it.
    #[tokio::test]
    async fn patch_result_carries_an_exact_unified_diff() {
        let dir = std::env::temp_dir().join("veles-filetool-diff");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("d.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\nf\ng\n").unwrap();

        let p = PatchTool::default();
        let res = p
            .call(json!({"path": path.display().to_string(), "old_string": "d", "new_string": "D\nD2"}))
            .await;
        // Summary first (previews read the head), then the diff.
        assert!(res.starts_with("patched"), "res: {res}");
        // 7 context+old lines (c0=0..c1=6), 7 new lines (d → two lines).
        assert!(
            res.contains("@@ -1,7 +1,8 @@"),
            "hunk header must carry real counts: {res}"
        );
        assert!(
            res.lines().any(|l| l == "-d")
                && res.lines().any(|l| l == "+D")
                && res.lines().any(|l| l == "+D2"),
            "removed and added lines must be exact: {res}"
        );
        assert!(
            res.lines().any(|l| l == " c") && res.lines().any(|l| l == " e"),
            "context lines surround the change: {res}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\nb\nc\nD\nD2\ne\nf\ng\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// replace_all with distant occurrences → one hunk each, with
    /// correct independent line numbers; a NON-line-aligned replacement
    /// expands to whole lines on both sides (what the file now contains).
    #[test]
    fn patch_diff_hunks_and_mid_line_replacements_are_exact() {
        let d = patch_diff(
            "a\nX\nm1\nm2\nm3\nm4\nm5\nm6\nb\nX\ntail\n",
            "X",
            "Y",
            true,
            "f.txt",
        );
        let hunks: Vec<&str> = d.lines().filter(|l| l.starts_with("@@")).collect();
        // Distant replacements stay separate hunks (nearby ones would
        // correctly merge: shared context covers both).
        assert_eq!(hunks, vec!["@@ -1,5 +1,5 @@", "@@ -7,5 +7,5 @@"], "d: {d}");

        let d = patch_diff("alpha beta\n", "a b", "A B", false, "m.txt");
        assert!(
            d.lines().any(|l| l == "-alpha beta") && d.lines().any(|l| l == "+alphA Beta"),
            "mid-line replacement shows whole lines, exactly: {d}"
        );

        // A pure deletion has no + lines and the new-side count shrinks.
        let d = patch_diff("k1\nk2\nk3\n", "k2\n", "", false, "del.txt");
        assert!(d.contains("@@ -1,3 +1,2 @@"), "d: {d}");
        assert!(
            d.lines().any(|l| l == "-k2")
                && !d
                    .lines()
                    .any(|l| l.starts_with('+') && !l.starts_with("+++")),
            "a pure deletion has no + lines: {d}"
        );
    }
}
