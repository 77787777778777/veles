//! Checkpoints — undo for the agent's file edits.
//!
//! This is the other half of the M11 approval decision. veles does not
//! ship hermes' unconditional blocklist (USER: "utter nonsense"), so the
//! agent is allowed to act; what makes that safe in practice is being
//! able to put a file back exactly as it was. Every `write_file` and
//! `patch` copies the previous bytes aside first.
//!
//! Deliberately NOT git: the user's repositories are theirs, and an
//! agent creating commits/stashes in them is its own kind of damage.
//! Blobs live under `VELES_HOME/checkpoints/` and the index lives in the
//! `checkpoints` table.
//!
//! Snapshotting is best-effort: if it fails, the edit still happens and
//! the failure is logged. A tool that refuses to work because its safety
//! net is unavailable is worse than one without a net.

use crate::config::CheckpointsSection;
use crate::store::{Checkpoint, Store};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// What a file tool needs to snapshot before it mutates something.
#[derive(Clone)]
pub struct CheckpointCtx {
    pub store: Arc<Mutex<Store>>,
    pub session_id: String,
}

/// Checkpoint settings, read once. Config is immutable for the life of
/// the process (same assumption `veles_home()` makes), so this avoids
/// threading `&VelesConfig` through every tool constructor.
fn settings() -> &'static CheckpointsSection {
    static S: OnceLock<CheckpointsSection> = OnceLock::new();
    S.get_or_init(|| {
        crate::config::load()
            .map(|c| c.checkpoints)
            .unwrap_or_default()
    })
}

pub fn blob_root() -> PathBuf {
    crate::config::veles_home().join("checkpoints")
}

/// A name made safe to be one path component under the blob root — the
/// per-session directory is `sanitize(session_id)`. pub(crate): B6's
/// bundle keys and its adopt check use the same rule.
pub(crate) fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .chars()
        .take(60)
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Copy the current bytes of `path` aside. Returns the checkpoint id.
///
/// A missing file is recorded too (blob = None): restoring that means
/// deleting whatever the agent created, which is exactly the undo the
/// user expects for "it wrote a file I didn't want".
pub fn snapshot(ctx: &CheckpointCtx, path: &Path, tool: &str, note: Option<&str>) -> Option<i64> {
    let cfg = settings();
    if !cfg.enabled {
        return None;
    }
    match snapshot_inner(ctx, path, tool, note, cfg) {
        Ok(id) => id,
        Err(e) => {
            // Never block the edit on the safety net failing.
            tracing::warn!("checkpoint for {} failed: {e:#}", path.display());
            None
        }
    }
}

fn snapshot_inner(
    ctx: &CheckpointCtx,
    path: &Path,
    tool: &str,
    note: Option<&str>,
    cfg: &CheckpointsSection,
) -> Result<Option<i64>> {
    let meta = std::fs::metadata(path).ok();
    let path_str = path.display().to_string();

    // New file: nothing to copy, but record that it did not exist.
    let Some(meta) = meta else {
        let store = ctx
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        let id = store.checkpoint_add(&ctx.session_id, &path_str, None, None, 0, tool, note)?;
        return Ok(Some(id));
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let max = cfg.max_file_mb.saturating_mul(1024 * 1024);
    if max > 0 && meta.len() > max {
        tracing::info!(
            "checkpoint skipped for {} ({} bytes > {} MB cap)",
            path.display(),
            meta.len(),
            cfg.max_file_mb
        );
        return Ok(None);
    }

    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let dir = blob_root().join(sanitize(&ctx.session_id));
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .unwrap_or_else(|| "file".into());
    let blob = dir.join(format!("{stamp}-{name}"));
    std::fs::write(&blob, &bytes).with_context(|| format!("write {}", blob.display()))?;

    let store = ctx
        .store
        .lock()
        .map_err(|_| anyhow::anyhow!("store poisoned"))?;
    let id = store.checkpoint_add(
        &ctx.session_id,
        &path_str,
        Some(&blob.display().to_string()),
        Some(&sha256_hex(&bytes)),
        bytes.len() as i64,
        tool,
        note,
    )?;
    Ok(Some(id))
}

/// Put a checkpointed file back. The CURRENT bytes are checkpointed
/// first, so an unwanted restore can itself be undone.
pub fn restore(store: &Arc<Mutex<Store>>, id: i64) -> Result<String> {
    let cp = {
        let s = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        s.checkpoint_get(id)?
    }
    .ok_or_else(|| anyhow::anyhow!("no checkpoint {id}"))?;
    let path = PathBuf::from(&cp.path);

    let ctx = CheckpointCtx {
        store: store.clone(),
        session_id: cp.session_id.clone(),
    };
    snapshot(
        &ctx,
        &path,
        "restore",
        Some(&format!("before restoring #{id}")),
    );

    match &cp.blob {
        Some(blob) => {
            let bytes =
                std::fs::read(blob).with_context(|| format!("checkpoint blob missing: {blob}"))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, &bytes).with_context(|| format!("restore {}", path.display()))?;
            Ok(format!("restored {} ({} bytes)", cp.path, bytes.len()))
        }
        None => {
            // The file did not exist when the checkpoint was taken.
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(format!("removed {} (did not exist before)", cp.path)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(format!("{} already absent", cp.path))
                }
                Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
            }
        }
    }
}

/// Age + size retention. Returns (rows removed, bytes freed).
pub fn prune(store: &Arc<Mutex<Store>>) -> Result<(usize, u64)> {
    let cfg = settings();
    let mut removed = 0usize;
    let mut freed = 0u64;

    let old = {
        let s = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        s.checkpoints_older_than(cfg.retention_days)?
    };
    for cp in old {
        freed += drop_one(store, &cp)?;
        removed += 1;
    }

    let budget = cfg.max_total_mb.saturating_mul(1024 * 1024) as i64;
    loop {
        let (_, total) = {
            let s = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store poisoned"))?;
            s.checkpoint_stats()?
        };
        if budget <= 0 || total <= budget {
            break;
        }
        let oldest = {
            let s = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store poisoned"))?;
            s.checkpoints_oldest(20)?
        };
        if oldest.is_empty() {
            break;
        }
        for cp in oldest {
            freed += drop_one(store, &cp)?;
            removed += 1;
        }
    }
    Ok((removed, freed))
}

fn drop_one(store: &Arc<Mutex<Store>>, cp: &Checkpoint) -> Result<u64> {
    if let Some(b) = &cp.blob {
        let _ = std::fs::remove_file(b);
    }
    let s = store
        .lock()
        .map_err(|_| anyhow::anyhow!("store poisoned"))?;
    s.checkpoint_delete(cp.id)?;
    Ok(cp.bytes.max(0) as u64)
}

/// Human-readable listing (CLI, slash commands, chat adapters).
pub fn render_list(items: &[Checkpoint]) -> String {
    if items.is_empty() {
        return "no checkpoints".into();
    }
    let mut out = String::new();
    for c in items {
        let kind = if c.blob.is_some() { "edit" } else { "new " };
        out.push_str(&format!(
            "#{:<5} {kind} {:<10} {:>8}B  {}  {}\n",
            c.id, c.tool, c.bytes, c.created_at, c.path
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // VELES_HOME is process-global and these tests set it; cargo runs
    // them in parallel threads, so they take crate::testenv::lock() —
    // without it one test writes its blobs into the other's tree.

    fn ctx(dir: &Path) -> (CheckpointCtx, Arc<Mutex<Store>>) {
        let store = Arc::new(Mutex::new(Store::open(&dir.join("state.db")).unwrap()));
        store.lock().unwrap().create_session("cp-test").unwrap();
        (
            CheckpointCtx {
                store: store.clone(),
                session_id: "cp-test".into(),
            },
            store,
        )
    }

    #[test]
    fn snapshot_then_restore_is_byte_exact() {
        let _guard = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!("veles-cp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("VELES_HOME", &dir);
        let (c, store) = ctx(&dir);

        let f = dir.join("hello.txt");
        std::fs::write(&f, "original\ncontent\n").unwrap();
        let id = snapshot(&c, &f, "write_file", None).expect("checkpoint id");

        std::fs::write(&f, "clobbered").unwrap();
        let msg = restore(&store, id).unwrap();
        assert!(msg.contains("restored"), "{msg}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original\ncontent\n");

        // The restore itself was checkpointed: undo of the undo works.
        let list = store.lock().unwrap().checkpoint_list(None, 10).unwrap();
        assert!(list.iter().any(|c| c.tool == "restore"));
        let redo = list.iter().find(|c| c.tool == "restore").unwrap().id;
        restore(&store, redo).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "clobbered");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restoring_a_created_file_deletes_it() {
        let _guard = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!("veles-cp2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("VELES_HOME", &dir);
        let (c, store) = ctx(&dir);

        let f = dir.join("brand-new.txt");
        let id = snapshot(&c, &f, "write_file", None).expect("checkpoint id");
        std::fs::write(&f, "the agent made this").unwrap();
        assert!(f.exists());

        let msg = restore(&store, id).unwrap();
        assert!(msg.contains("removed"), "{msg}");
        assert!(!f.exists(), "restore of a never-existed file deletes it");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
