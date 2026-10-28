//! Desktop panes backend — Files / Review / Preview state for the
//! right-pane shell (hermes-desktop's right-sidebar parity).
//!
//! The agent side reaches this through the `open_preview` and
//! `focus_pane` tools, which POST to the same endpoints over loopback;
//! the Tk pane polls the preview state and reacts. Everything here is
//! loopback-local and read-mostly: the mutations are git
//! stage/unstage/revert (the Review pane's own buttons) and
//! `fs_write` — the mini-editor's save path, three lines below this.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

/// POST /api/fs/write — the mini-editor's save path (loopback only).
/// Body: {"path": "...", "content": "..."} → {"ok": true} | {"error": "..."}.
/// Same file-safety guard as the write_file tool: credentials, the live
/// DB, and key material are refused even from our own UI.
fn content_sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Deserialize)]
pub struct WriteFileBody {
    pub path: String,
    pub content: String,
    /// The `sha256` returned by the read whose buffer is being saved.
    /// Absent means an intentional write without a base (for example a
    /// newly named file); the desktop always supplies it after fs_file.
    #[serde(default)]
    pub base_sha: Option<String>,
}

pub async fn fs_write(
    State(_srv): State<Arc<DesktopServer>>,
    Json(body): Json<WriteFileBody>,
) -> Json<Value> {
    if let Some(reason) = veles_core::harness::protected_path_reason(
        &body.path,
        &veles_core::config::config_dir(),
        &veles_core::config::data_dir(),
    ) {
        return Json(json!({"error": format!("refused — protected ({reason})")}));
    }
    let p = std::path::PathBuf::from(expand_tilde(&body.path));
    // Compare the file we actually read, not an mtime the request may
    // have carried across another machine's clock or filesystem. An
    // editor save is conditional; an omitted base is the explicit
    // unconditioned/new-file path retained for the other API callers.
    let current = match std::fs::read(&p) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Json(json!({"error": format!("read {}: {e}", p.display())})),
    };
    if body.base_sha.is_none() && current.is_some() {
        return Json(
            json!({"error": "file already exists; send the sha256 of the loaded buffer", "conflict": true}),
        );
    }
    if let Some(expected) = body.base_sha.as_deref().filter(|s| !s.is_empty()) {
        let actual = current.as_deref().map(content_sha);
        if actual.as_deref() != Some(expected) {
            return Json(json!({
                "ok": false,
                "error": "file changed since it was loaded; your buffer is still unsaved. Re-read it and reconcile before saving",
                "conflict": true,
                "path": p.to_string_lossy(),
                "base_sha": expected,
                "current_sha": actual,
            }));
        }
    }
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Json(json!({"error": format!("mkdir {}: {e}", parent.display())}));
        }
    }
    if let Some(expected) = body.base_sha.as_deref().filter(|s| !s.is_empty()) {
        let actual = std::fs::read(&p).ok().as_deref().map(content_sha);
        if actual.as_deref() != Some(expected) {
            return Json(
                json!({"error": "file changed while saving; buffer left unsaved", "conflict": true}),
            );
        }
    }
    match std::fs::write(&p, &body.content) {
        Ok(()) => Json(json!({
            "ok": true,
            "path": p.to_string_lossy(),
            "bytes": body.content.len(),
            "sha256": content_sha(body.content.as_bytes()),
        })),
        Err(e) => Json(json!({"error": format!("write {}: {e}", p.display())})),
    }
}
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use super::desktop_api::DesktopServer;
use veles_core::lockutil::LockOk;

// ── file tree ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TreeQuery {
    pub path: String,
    #[serde(default)]
    pub depth: u32,
}

/// GET /api/fs/tree?path=...&depth=N — bounded directory listing.
/// Depth is capped at 3 and each level at 400 entries so a fat tree can
/// never blow up a single response; the client expands lazily.
pub async fn fs_tree(
    State(_srv): State<Arc<DesktopServer>>,
    Query(q): Query<TreeQuery>,
) -> Json<Value> {
    let root = expand_tilde(&q.path);
    let depth = q.depth.clamp(1, 3);
    match read_dir_level(std::path::Path::new(&root), depth) {
        Ok(entries) => Json(json!({ "path": root, "entries": entries })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

fn read_dir_level(path: &std::path::Path, depth: u32) -> std::io::Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| {
        (
            !e.file_type().map(|t| t.is_dir()).unwrap_or(false), // dirs first
            e.file_name().to_string_lossy().to_lowercase(),
        )
    });
    for e in entries.into_iter().take(400) {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or_else(|_| {
            // broken symlink — treat as a file, never panic
            std::fs::symlink_metadata(e.path())
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false)
        });
        let mut entry = json!({
            "name": e.file_name().to_string_lossy(),
            "path": e.path().to_string_lossy(),
            "is_dir": is_dir,
        });
        if is_dir && depth > 1 {
            if let Ok(children) = read_dir_level(&e.path(), depth - 1) {
                entry["children"] = json!(children);
            }
        }
        out.push(entry);
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct ReadFileQuery {
    pub path: String,
    #[serde(default = "default_read_limit")]
    pub limit: usize,
}

fn default_read_limit() -> usize {
    200
}

/// GET /api/fs/read?path=...&limit=N — file head for the Preview pane.
/// Binary files are reported, not dumped.
pub async fn fs_read(
    State(_srv): State<Arc<DesktopServer>>,
    Query(q): Query<ReadFileQuery>,
) -> Json<Value> {
    let p = std::path::PathBuf::from(expand_tilde(&q.path));
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };
    if meta.len() > 2_000_000 {
        return Json(json!({ "error": "file too large to preview (2MB cap)" }));
    }
    match std::fs::read(&p) {
        Ok(bytes) => {
            if bytes.iter().take(8000).any(|&b| b == 0) {
                return Json(json!({
                    "path": p.to_string_lossy(),
                    "binary": true,
                    "size": bytes.len(),
                }));
            }
            let text = String::from_utf8_lossy(&bytes).to_string();
            let lines: Vec<&str> = text.lines().collect();
            let shown = lines
                .into_iter()
                .take(q.limit.clamp(1, 5000))
                .collect::<Vec<_>>()
                .join("\n");
            Json(json!({
                "path": p.to_string_lossy(),
                "binary": false,
                "size": bytes.len(),
                "text": shown,
                "truncated": bytes.len() > shown.len(),
            }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(d) = directories::UserDirs::new() {
            return d.home_dir().join(rest).to_string_lossy().to_string();
        }
    }
    p.to_string()
}

#[derive(Deserialize)]
pub struct FileQuery {
    pub path: String,
}

/// GET /api/fs/file?path=... — the ENTIRE file, for the Dev pane editor.
///
/// `fs_read` is a preview HEAD: it serves the first N lines (default
/// 200). The editor used to open files through it, so every file over
/// 200 lines loaded truncated — and because LSP sync sends the buffer
/// text, the language server inherited the truncation and diagnosed
/// "missing braces" on a document cut off mid-block. An editor needs
/// the whole file or nothing.
///
/// No line limit, no trailing-newline loss: the body is the file's
/// bytes decoded losslessly-or-not (`from_utf8_lossy`, same as every
/// other fs surface here). 8MB cap: past that a text widget is the
/// wrong tool anyway, and the LSP round trip would fight the request
/// body limit on the way back out.
pub async fn fs_file(
    State(_srv): State<Arc<DesktopServer>>,
    Query(q): Query<FileQuery>,
) -> Json<Value> {
    let p = std::path::PathBuf::from(expand_tilde(&q.path));
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };
    if meta.len() > 8_000_000 {
        return Json(json!({ "error": "file too large to edit (8MB cap)" }));
    }
    match std::fs::read(&p) {
        Ok(bytes) => {
            if bytes.iter().take(8000).any(|&b| b == 0) {
                return Json(json!({
                    "path": p.to_string_lossy(),
                    "binary": true,
                    "size": bytes.len(),
                }));
            }
            Json(json!({
                "path": p.to_string_lossy(),
                "binary": false,
                "size": bytes.len(),
                "text": String::from_utf8_lossy(&bytes).to_string(),
                "sha256": content_sha(&bytes),
            }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ── review (git) ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GitQuery {
    pub repo: String,
}

fn run_git(repo: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git spawn failed: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!("{}{}", stdout, stderr).trim().to_string())
    }
}

/// GET /api/git/status?repo=... — porcelain status for the Review pane.
pub async fn git_status(
    State(_srv): State<Arc<DesktopServer>>,
    Query(q): Query<GitQuery>,
) -> Json<Value> {
    let repo = std::path::PathBuf::from(expand_tilde(&q.repo));
    match run_git(&repo, &["status", "--porcelain=v1", "-z"]) {
        Ok(raw) => {
            let files: Vec<Value> = raw
                .split('\0')
                .filter(|s| s.len() >= 4)
                .map(|s| {
                    let (xy, path) = s.split_at(2);
                    let path = path.trim_start();
                    json!({
                        "status": xy.trim().chars().next().unwrap_or('?').to_string(),
                        "path": path,
                        "raw": xy,
                    })
                })
                .collect();
            let branch = run_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
            Json(json!({ "repo": repo.to_string_lossy(), "branch": branch.trim(), "files": files }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// GET /api/git/diff?repo=...&path=... — the diff for one file (or the
/// whole tree when path is empty). Unstaged vs HEAD.
pub async fn git_diff(
    State(_srv): State<Arc<DesktopServer>>,
    Query(q): Query<GitDiffQuery>,
) -> Json<Value> {
    let repo = std::path::PathBuf::from(expand_tilde(&q.repo));
    let mut args = vec!["diff", "HEAD", "--"];
    if !q.path.is_empty() {
        args.push(&q.path);
    }
    match run_git(&repo, &args) {
        Ok(d) => {
            // Cap: a 500KB diff is already unreadable in any pane.
            let capped = if d.len() > 500_000 {
                d[..500_000].to_string()
            } else {
                d
            };
            Json(json!({ "repo": repo.to_string_lossy(), "path": q.path, "diff": capped }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

#[derive(Deserialize)]
pub struct GitDiffQuery {
    pub repo: String,
    #[serde(default)]
    pub path: String,
}

/// POST /api/git/mutate — stage/unstage/revert from the Review pane.
/// These are the only git WRITES the desktop does; every one goes
/// through this single auditable endpoint.
pub async fn git_mutate(
    State(_srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let repo = std::path::PathBuf::from(expand_tilde(body["repo"].as_str().unwrap_or_default()));
    let action = body["action"].as_str().unwrap_or_default();
    let path = body["path"].as_str().unwrap_or_default();
    if repo.as_os_str().is_empty() || path.is_empty() {
        return Json(json!({ "error": "repo and path are required" }));
    }
    let res = match action {
        "stage" => run_git(&repo, &["add", "--", path]),
        "unstage" => run_git(&repo, &["restore", "--staged", "--", path]),
        // Revert is destructive — the pane confirms before calling this.
        "revert" => run_git(&repo, &["checkout", "HEAD", "--", path]),
        _ => return Json(json!({ "error": format!("unknown action `{action}`") })),
    };
    match res {
        Ok(_) => Json(json!({ "ok": true, "action": action, "path": path })),
        Err(e) => Json(json!({ "error": e })),
    }
}

// ── preview state ────────────────────────────────────────────────────
//
// The agent's open_preview tool POSTs here; the desktop Preview pane
// polls and renders. Kinds: url (open the browser), file (show content
// in the pane), artifact (a transcript artifact, future). State lives
// in desktop_state.json's `preview` key (durable across restarts).

static PREVIEW: Mutex<Option<Value>> = Mutex::new(None);

fn load_preview_state() -> Value {
    if let Some(v) = PREVIEW.lock_ok().as_ref() {
        return v.clone();
    }
    let st = super::desktop_api::load_desktop_state_pub();
    let v = st.get("preview").cloned().unwrap_or(json!(null));
    *PREVIEW.lock_ok() = Some(v.clone());
    v
}

/// GET /api/preview — current preview item (or null).
pub async fn preview_get(State(_srv): State<Arc<DesktopServer>>) -> Json<Value> {
    Json(load_preview_state())
}

/// POST /api/preview — set the preview item. Body: {kind, url?, path?,
/// label?}. Also written to desktop_state.json so the pane survives a
/// desktop restart.
pub async fn preview_set(
    State(_srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let kind = body["kind"].as_str().unwrap_or("url");
    let item = json!({
        "kind": kind,
        "url": body["url"],
        "path": body["path"],
        "label": body["label"],
        "set_at": chrono::Utc::now().to_rfc3339(),
    });
    *PREVIEW.lock_ok() = Some(item.clone());
    if let Ok(mut st) = Ok::<_, anyhow::Error>(super::desktop_api::load_desktop_state_pub()) {
        if !st.is_object() {
            st = json!({});
        }
        st["preview"] = item.clone();
        let _ = super::desktop_api::save_desktop_state_pub(&st);
    }
    Json(json!({ "ok": true, "preview": item }))
}

/// POST /api/focus — the agent's focus_pane tool lands here. The Tk
/// client polls it alongside /api/preview and reacts (raise the pane).
pub async fn focus_set(
    State(_srv): State<Arc<DesktopServer>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let pane = body["pane"].as_str().unwrap_or_default();
    let valid = ["chat", "files", "review", "terminal", "preview", "sessions"];
    if !valid.contains(&pane) {
        return Json(json!({ "error": format!("unknown pane `{pane}`") }));
    }
    let item = json!({
        "pane": pane,
        "set_at": chrono::Utc::now().to_rfc3339(),
    });
    if let Ok(mut st) = Ok::<_, anyhow::Error>(super::desktop_api::load_desktop_state_pub()) {
        if !st.is_object() {
            st = json!({});
        }
        st["focus_request"] = item.clone();
        let _ = super::desktop_api::save_desktop_state_pub(&st);
    }
    Json(json!({ "ok": true, "focus": item }))
}

pub async fn focus_get(State(_srv): State<Arc<DesktopServer>>) -> Json<Value> {
    let st = super::desktop_api::load_desktop_state_pub();
    Json(st.get("focus_request").cloned().unwrap_or(json!(null)))
}
