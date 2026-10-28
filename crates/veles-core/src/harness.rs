//! Harness guards, second family — file safety, stale-write detection,
//! subdirectory hints, context breakdown.
//!
//! Continuation of guards.rs (batch 1). Same design rule: pure policy,
//! synchronous, unit-testable; runtime owns enforcement.
//!
//! `parse_bundle` (hermes' agent/skill_bundles.py) lived here too, with
//! its own test and zero callers — a port that was never wired to
//! anything, so the test only ever proved the parser parsed. Deleted
//! rather than finished: skill bundles are a feature to decide on, not
//! a thing to keep half-present because hermes had it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ── file safety ──────────────────────────────────────────────────────
// hermes: agent/file_safety.py — protected paths shared by tools.
// veles keeps config.toml agent-editable (user decision at M11) but
// protects the credential file, live DB, and ssh keys.

/// Returns a reason string if the path is protected, else None.
///
/// Takes BOTH veles directories because the credentials are config
/// (`.env`, `secrets.env` in `config_dir()`) and the store is state
/// (`state.db` in `data_dir()`). This took one `veles_home` until
/// 2026-09-26 — which the XDG split had made the data dir, so the
/// `.env` rule guarded `~/.local/share/veles/.env`, a file that does
/// not exist, and let the real `~/.config/veles/.env` be overwritten.
/// Under a forced `VELES_HOME` both arguments are the same directory.
pub fn protected_path_reason(path: &str, config_dir: &Path, data_dir: &Path) -> Option<String> {
    let p = normalize(path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let in_dir = |d: &Path| p.parent() == Some(d);

    if in_dir(config_dir) && (name == ".env" || name.starts_with(".env.") || name == "secrets.env")
    {
        return Some("credential file (contains provider keys and bot tokens)".into());
    }
    if in_dir(data_dir) && name.starts_with("state.db") {
        return Some("live session database (use the store API, not raw writes)".into());
    }
    // `/` for both separators: the markers below are spelled with it.
    let lower = p.to_string_lossy().replace('\\', "/").to_lowercase();
    if lower.contains("/.ssh/") || lower.ends_with("/.ssh") {
        return Some("ssh key material".into());
    }
    if lower.ends_with(".pem") || lower.ends_with("id_rsa") || lower.ends_with("id_ed25519") {
        return Some("private key material".into());
    }
    None
}

fn normalize(path: &str) -> PathBuf {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(h) => format!("{h}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    };
    // One file, one key, however the caller spells it (RM3 #6): the
    // ledger used to key the RAW argument string, so `src/a.rs` and
    // `./src/a.rs` were different files to the guard — a stale write
    // could sail through by respelling the path. Absolutized and
    // lexically cleaned (no canonicalize: that would need the file to
    // exist and would fold symlinks differently before/after a create).
    let p = PathBuf::from(expanded);
    if p.is_absolute() {
        lexical_clean(p)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        lexical_clean(cwd.join(p))
    }
}

/// Drop `.` components and resolve `..` against the prefix, without
/// touching the filesystem.
fn lexical_clean(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ── file state (stale-write detection) ───────────────────────────────
// hermes: tools/file_state.py — prevents an agent from overwriting a
// file that changed since it last read it (concurrent subagents, or an
// external editor). veles records read/write mtimes per agent.

#[derive(Default)]
pub struct FileStateLedger {
    /// path -> mtime observed at last read (millis since epoch)
    reads: HashMap<PathBuf, u128>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WriteCheck {
    /// No prior read, or unchanged — safe to write.
    Ok,
    /// File changed on disk since the last read — stale write.
    ChangedExternally,
}

impl FileStateLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_read(&mut self, path: &str, mtime_ms: u128) {
        self.reads.insert(normalize(path), mtime_ms);
    }

    pub fn record_write(&mut self, path: &str, mtime_ms: u128) {
        // A write refreshes the baseline.
        self.reads.insert(normalize(path), mtime_ms);
    }

    /// Check before writing: has the file moved since we last saw it?
    pub fn check_write(&self, path: &str, current_mtime_ms: u128) -> WriteCheck {
        match self.reads.get(&normalize(path)) {
            None => WriteCheck::Ok, // never read — nothing to be stale about
            Some(seen) => {
                if current_mtime_ms > *seen {
                    WriteCheck::ChangedExternally
                } else {
                    WriteCheck::Ok
                }
            }
        }
    }
}

// ── subdirectory hints ───────────────────────────────────────────────
// hermes: agent/subdirectory_hints.py — as the agent descends into a
// directory tree, surface the context file (AGENTS.md etc.) living
// there. veles' prompt.rs only reads the ROOT context file; this tracks
// descent per session and reports newly-entered context files.

#[derive(Default)]
pub struct SubdirHints {
    /// Deepest directory we've announced context for, by normalized path.
    announced: std::collections::HashSet<PathBuf>,
}

impl SubdirHints {
    pub fn new() -> Self {
        Self::default()
    }

    /// Given a file path the agent just touched, return the context file
    /// to announce (if any) — only for directories not yet announced.
    /// `context_names` is the discovery order (prompt.rs parity).
    pub fn hint_for(&mut self, touched_path: &str, context_names: &[&str]) -> Option<PathBuf> {
        let p = normalize(touched_path);
        let dir = p.parent()?;
        if self.announced.contains(dir) {
            return None;
        }
        for name in context_names {
            let candidate = dir.join(name);
            if candidate.exists() {
                self.announced.insert(dir.to_path_buf());
                return Some(candidate);
            }
        }
        // Mark as announced-empty so we don't re-scan on every touch.
        self.announced.insert(dir.to_path_buf());
        None
    }
}

/// Global file-state ledger (shared by read_file/write_file/patch in
/// one process — the stale-overwrite guard for concurrent subagents).
pub fn file_state_ledger() -> &'static Mutex<FileStateLedger> {
    static LEDGER: std::sync::OnceLock<Mutex<FileStateLedger>> = std::sync::OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(FileStateLedger::new()))
}

/// Global subdirectory-hint tracker (per-process; announces AGENTS.md
/// etc. when the agent first touches a directory carrying one).
pub fn subdir_hints() -> &'static Mutex<SubdirHints> {
    static HINTS: std::sync::OnceLock<Mutex<SubdirHints>> = std::sync::OnceLock::new();
    HINTS.get_or_init(|| Mutex::new(SubdirHints::new()))
}

/// Current mtime of a path in ms since epoch (0 when absent).
pub fn mtime_ms(path: &std::path::Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Context-file discovery order (prompt.rs parity).
pub const CONTEXT_FILE_NAMES: &[&str] = &[
    ".veles.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".hermes.md",
    "SOUL.md",
];

// ── context breakdown ────────────────────────────────────────────────
// hermes: agent/context_breakdown.py — per-role token accounting so the
// user can see WHERE the context is going. Pure computation over a
// (role, char_count) list; token estimate = chars/4 (hermes' rule of
// thumb for mixed text).

#[derive(Debug, Clone, PartialEq)]
pub struct ContextBreakdown {
    pub system_chars: u64,
    pub user_chars: u64,
    pub assistant_chars: u64,
    pub tool_chars: u64,
    pub messages: u64,
}

pub fn breakdown(entries: &[(String, usize)]) -> ContextBreakdown {
    let mut b = ContextBreakdown {
        system_chars: 0,
        user_chars: 0,
        assistant_chars: 0,
        tool_chars: 0,
        messages: 0,
    };
    for (role, chars) in entries {
        b.messages += 1;
        match role.as_str() {
            "system" => b.system_chars += *chars as u64,
            "user" => b.user_chars += *chars as u64,
            "assistant" => b.assistant_chars += *chars as u64,
            _ => b.tool_chars += *chars as u64,
        }
    }
    b
}

impl ContextBreakdown {
    pub fn total_chars(&self) -> u64 {
        self.system_chars + self.user_chars + self.assistant_chars + self.tool_chars
    }
    pub fn estimated_tokens(&self) -> u64 {
        self.total_chars() / 4
    }
}

// ── per-session file focus (RM3 #5) ───────────────────────────────────
//
// Models expect consecutive file ops to reuse the previous path when
// none is given — qwen-flash called `patch {old_string, new_string}`
// after a read and got a refusal that taught nothing. The focus is the
// path of the last SUCCESSFUL file op for the session; the agent loop
// injects it into pathless `patch`/`read_file` calls (see
// repair::fill_implicit_path) and says so in an arg note.

pub fn file_focus_set(session_id: &str, path: &str) {
    file_focus()
        .lock()
        .unwrap()
        .insert(session_id.to_string(), path.to_string());
}

pub fn file_focus_get(session_id: &str) -> Option<String> {
    file_focus().lock().unwrap().get(session_id).cloned()
}

pub fn file_focus_clear(session_id: &str) {
    file_focus().lock().unwrap().remove(session_id);
}

fn file_focus() -> &'static Mutex<HashMap<String, String>> {
    static FOCUS: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    FOCUS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_safety_protects_secrets_not_code() {
        // The XDG layout: credentials are CONFIG, the store is STATE.
        // On Windows a rooted path without a drive is not absolute and
        // normalize() gives it the cwd's drive; spell the fixture with one
        // so both sides agree (`C:/home/x` is an absolute Windows path).
        let w = |p: &str| {
            if cfg!(windows) {
                format!("C:{p}")
            } else {
                p.to_string()
            }
        };
        let cfg_s = w("/home/x/.config/veles");
        let data_s = w("/home/x/.local/share/veles");
        let cfg = Path::new(&cfg_s);
        let data = Path::new(&data_s);
        let deny = |p: &str| protected_path_reason(&w(p), cfg, data);
        assert!(deny("/home/x/.config/veles/.env").is_some());
        assert!(deny("/home/x/.config/veles/.env.bak").is_some());
        assert!(deny("/home/x/.config/veles/secrets.env").is_some());
        assert!(deny("/home/x/.local/share/veles/state.db").is_some());
        assert!(deny("/home/x/.local/share/veles/state.db-wal").is_some());
        assert!(deny("/home/x/.ssh/id_ed25519").is_some());
        assert!(deny("/home/x/project/src/main.rs").is_none());
        // config.toml stays agent-editable (M11); the approval screen
        // covers it, not this hard deny.
        assert!(deny("/home/x/.config/veles/config.toml").is_none());
        // A forced single-directory home (VELES_HOME) is both halves.
        let one_s = w("/opt/data");
        let one = Path::new(&one_s);
        assert!(protected_path_reason(&w("/opt/data/.env"), one, one).is_some());
        assert!(protected_path_reason(&w("/opt/data/state.db"), one, one).is_some());
    }

    #[test]
    fn stale_write_detection() {
        let mut l = FileStateLedger::new();
        l.record_read("src/a.rs", 1000);
        assert_eq!(l.check_write("src/a.rs", 1000), WriteCheck::Ok);
        assert_eq!(
            l.check_write("src/a.rs", 1500),
            WriteCheck::ChangedExternally
        );
        assert_eq!(l.check_write("src/b.rs", 9999), WriteCheck::Ok); // never read
        l.record_write("src/a.rs", 1500);
        assert_eq!(l.check_write("src/a.rs", 1500), WriteCheck::Ok);
    }

    /// RM3 #6: the ledger keyed the RAW argument string, so
    /// `src/a.rs` and `./src/a.rs` were different keys — the
    /// stale-write guard could be silently BYPASSED by spelling the
    /// path differently (and a `~`-relative form dodged it too). One
    /// file, one key, however the model spells it.
    #[test]
    fn ledger_keys_survive_path_spellings() {
        let mut l = FileStateLedger::new();
        l.record_read("src/a.rs", 1000);
        assert_eq!(
            l.check_write("./src/a.rs", 1500),
            WriteCheck::ChangedExternally,
            "'./' spelling must not bypass the guard"
        );
        l.record_read("./b.rs", 1000);
        assert_eq!(
            l.check_write("b.rs", 1500),
            WriteCheck::ChangedExternally,
            "bare spelling after dotted read must not bypass either"
        );
    }

    /// RM3 #5: the per-session "current file" for implicit-path
    /// inference.
    #[test]
    fn file_focus_is_per_session() {
        file_focus_set("focus-s1", "/srv/app/src/main.rs");
        assert_eq!(
            file_focus_get("focus-s1").as_deref(),
            Some("/srv/app/src/main.rs")
        );
        assert_eq!(file_focus_get("focus-s2"), None, "no cross-session bleed");
        file_focus_set("focus-s1", "/srv/app/src/other.rs");
        assert_eq!(
            file_focus_get("focus-s1").as_deref(),
            Some("/srv/app/src/other.rs"),
            "latest touch wins"
        );
        file_focus_clear("focus-s1");
        assert_eq!(file_focus_get("focus-s1"), None);
    }

    #[test]
    fn subdir_hint_announces_once() {
        let dir = std::env::temp_dir().join("veles-subdir-hint-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# test").unwrap();
        let mut h = SubdirHints::new();
        // Touch a file IN the directory that carries the context file.
        let touched = dir.join("main.rs");
        let names = ["AGENTS.md", ".veles.md"];
        let first = h.hint_for(touched.to_str().unwrap(), &names);
        assert!(first.is_some());
        assert!(first.unwrap().ends_with("AGENTS.md"));
        // Second touch in the same dir: no re-announcement.
        assert!(h.hint_for(touched.to_str().unwrap(), &names).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_breakdown_math() {
        let entries = vec![
            ("system".to_string(), 4000usize),
            ("user".to_string(), 1000),
            ("assistant".to_string(), 3000),
            ("tool".to_string(), 2000),
        ];
        let b = breakdown(&entries);
        assert_eq!(b.total_chars(), 10000);
        assert_eq!(b.estimated_tokens(), 2500);
        assert_eq!(b.messages, 4);
    }
}
