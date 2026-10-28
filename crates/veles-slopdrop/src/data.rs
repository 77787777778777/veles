//! Where the module keeps its state: the withheld ledger, the verdict
//! store, the operator's denylist and the state mirror
//! (PLAN-plugins-installer.md R5, "it migrates once to the module's data
//! dir, keeping a backup").
//!
//! Linked into core, all of it lived under `data_dir()/slopdrop/`. As a
//! module it lives in the instance's state dir the host hands over
//! (`<data>/module-state/slopdrop/`), with the SAME layout inside, so
//! the move is one tree copy. The first start on a node that has the old
//! tree copies it over and renames the old tree `slopdrop.pre-module`:
//! a backup, never deleted by this code.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

fn set_dir() -> &'static RwLock<Option<PathBuf>> {
    static D: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    D.get_or_init(|| RwLock::new(None))
}

/// Make `dir` the module's state dir (the module's `start`, from
/// `cx.dirs.state`). `None` goes back to the default.
pub fn set_state_dir(dir: Option<PathBuf>) {
    *set_dir().write().unwrap_or_else(|p| p.into_inner()) = dir;
}

/// The default, when no host set one: where the host WOULD put it.
/// Computed per call (not cached): tests move `XDG_DATA_HOME` per test.
pub fn default_dir() -> PathBuf {
    veles_core::config::data_dir()
        .join("module-state")
        .join("slopdrop")
}

/// The module's state dir.
pub fn dir() -> PathBuf {
    set_dir()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_else(default_dir)
}

/// Where the linked slopdrop kept everything.
pub fn legacy_dir() -> PathBuf {
    veles_core::config::data_dir().join("slopdrop")
}

/// The marker a finished migration leaves in the new dir.
const MARKER: &str = ".migrated-from";

/// What a migration did, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to move (no legacy tree), or moved before.
    Nothing,
    /// Copied `files` entries from the legacy tree; the legacy tree is
    /// now at `backup`.
    Moved { files: usize, backup: PathBuf },
}

/// Move the legacy tree into `dir()` once. Idempotent: a second call
/// finds the marker and does nothing; a legacy tree that appears again
/// after a migration (a restore from backup) is left alone too, since
/// silently merging two ledgers is worse than asking.
pub fn migrate_once() -> std::io::Result<Migration> {
    migrate_between(&legacy_dir(), &dir())
}

fn migrate_between(old: &Path, new: &Path) -> std::io::Result<Migration> {
    if !old.is_dir() || new.join(MARKER).exists() {
        return Ok(Migration::Nothing);
    }
    std::fs::create_dir_all(new)?;
    let files = copy_tree(old, new)?;
    std::fs::write(
        new.join(MARKER),
        format!(
            "{}\n{}\n",
            old.display(),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
        ),
    )?;
    let backup = old.with_file_name("slopdrop.pre-module");
    // Keep the old tree, renamed: the backup. A name already taken (two
    // migrations from two restores) gets a numbered one.
    let backup = if backup.exists() {
        let mut n = 2;
        loop {
            let b = old.with_file_name(format!("slopdrop.pre-module.{n}"));
            if !b.exists() {
                break b;
            }
            n += 1;
        }
    } else {
        backup
    };
    std::fs::rename(old, &backup)?;
    Ok(Migration::Moved { files, backup })
}

/// Copy `from` into `to` (dirs made, files overwritten only where `to`
/// has none: a file both sides have is the NEW side's, which is the
/// state the running module wrote). Returns the files copied.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<usize> {
    let mut n = 0;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            std::fs::create_dir_all(&dst)?;
            n += copy_tree(&src, &dst)?;
        } else if !dst.exists() {
            std::fs::copy(&src, &dst)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legacy_tree_moves_once_and_is_kept_as_a_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("slopdrop");
        let new = tmp.path().join("module-state").join("slopdrop");
        std::fs::create_dir_all(old.join("local").join("procs")).unwrap();
        std::fs::write(old.join("withheld.json"), "[]").unwrap();
        std::fs::write(
            old.join("local").join("procs").join("a.tcl"),
            "proc a {} {}",
        )
        .unwrap();
        let m = migrate_between(&old, &new).unwrap();
        assert!(matches!(m, Migration::Moved { files: 2, .. }), "{m:?}");
        assert_eq!(
            std::fs::read_to_string(new.join("withheld.json")).unwrap(),
            "[]"
        );
        assert!(new.join("local").join("procs").join("a.tcl").exists());
        assert!(!old.exists(), "the legacy dir is renamed away");
        let backup = tmp.path().join("slopdrop.pre-module");
        assert!(
            backup.join("withheld.json").exists(),
            "the backup keeps the files"
        );
        assert!(new.join(MARKER).exists());
        // Again: nothing (the marker), even with a legacy tree back.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("withheld.json"), "[{\"id\":\"x\"}]").unwrap();
        assert_eq!(migrate_between(&old, &new).unwrap(), Migration::Nothing);
        assert_eq!(
            std::fs::read_to_string(new.join("withheld.json")).unwrap(),
            "[]",
            "a later legacy tree is never merged over the live state"
        );
    }

    #[test]
    fn no_legacy_tree_is_nothing_to_do() {
        let tmp = tempfile::tempdir().unwrap();
        let m = migrate_between(&tmp.path().join("none"), &tmp.path().join("new")).unwrap();
        assert_eq!(m, Migration::Nothing);
    }

    #[test]
    fn the_dir_follows_what_the_host_set() {
        // The dir is process-global: hold the scratch-home lock so the
        // tests that write the stores do not run in between.
        let (_home, _env) = veles_core::testenv::scratch_home();
        let tmp = tempfile::tempdir().unwrap();
        set_state_dir(Some(tmp.path().to_path_buf()));
        assert_eq!(dir(), tmp.path());
        set_state_dir(None);
        assert!(
            dir().ends_with("module-state/slopdrop"),
            "{}",
            dir().display()
        );
    }
}
