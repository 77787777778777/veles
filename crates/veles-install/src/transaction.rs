//! Recovery for a locked local installer operation. Snapshots are taken
//! before writes and every successful mutation is marked. A failed later
//! step restores only those marked paths, and only if their bytes/link
//! still match the state this operation produced. Foreign newer edits
//! are never overwritten silently.
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Snapshot {
    Missing,
    File {
        bytes: Vec<u8>,
        #[cfg(unix)]
        mode: u32,
    },
    Link(PathBuf),
    /// A payload tree created by this operation. The captured identity
    /// prevents a later writer's directory from being removed on rollback.
    Directory {
        files: BTreeMap<PathBuf, String>,
        mode: u32,
    },
}

impl Snapshot {
    fn capture(path: &Path) -> Result<Self> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => Ok(Self::Link(std::fs::read_link(path)?)),
            Ok(meta) if meta.is_file() => {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode()
                };
                Ok(Self::File {
                    bytes: std::fs::read(path)?,
                    #[cfg(unix)]
                    mode,
                })
            }
            Ok(meta) if meta.is_dir() => {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode()
                };
                #[cfg(not(unix))]
                let mode = 0;
                Ok(Self::Directory {
                    files: tree_identity(path)?,
                    mode,
                })
            }
            Ok(_) => anyhow::bail!(
                "installer recovery path is not a regular file or link: {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(e) => Err(e.into()),
        }
    }

    fn restore(&self, path: &Path) -> Result<()> {
        match self {
            Self::Missing => match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                    std::fs::remove_file(path)?
                }
                Ok(meta) if meta.is_dir() => {
                    std::fs::remove_dir_all(path)?;
                }
                Ok(_) => anyhow::bail!(
                    "refusing to remove non-file recovery path {}",
                    path.display()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
            Self::File {
                bytes,
                #[cfg(unix)]
                mode,
            } => {
                let parent = path.parent().context("recovery file needs a parent")?;
                std::fs::create_dir_all(parent)?;
                let tmp = temporary(path);
                veles_core::platform::write_private(&tmp, bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // Recovery is a restore, not a new private file. Do
                    // not silently widen or narrow the operator's mode.
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(*mode))?;
                }
                if let Err(e) = veles_core::platform::replace_file(&tmp, path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
            }
            Self::Directory { .. } => {
                anyhow::bail!("an existing payload directory is never replaced by profile rollback")
            }
            Self::Link(target) => {
                #[cfg(unix)]
                {
                    let parent = path.parent().context("recovery link needs a parent")?;
                    std::fs::create_dir_all(parent)?;
                    let tmp = temporary(path);
                    let _ = std::fs::remove_file(&tmp);
                    std::os::unix::fs::symlink(target, &tmp)?;
                    if let Err(e) = std::fs::rename(&tmp, path) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e.into());
                    }
                }
                #[cfg(not(unix))]
                {
                    let text = target
                        .file_name()
                        .context("recovery module pointer has no filename")?
                        .to_string_lossy();
                    let tmp = temporary(path);
                    std::fs::write(&tmp, text.as_bytes())?;
                    veles_core::platform::replace_file(&tmp, path)?;
                }
            }
        }
        Ok(())
    }
}

fn tree_identity(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    let mut out = BTreeMap::new();
    for entry in walkdir(root)? {
        let relative = entry.strip_prefix(root)?.to_path_buf();
        let meta = std::fs::symlink_metadata(&entry)?;
        let identity = if meta.is_dir() {
            "directory".into()
        } else if meta.is_file() {
            crate::sha256(&std::fs::read(&entry)?)
        } else if meta.file_type().is_symlink() {
            format!("link:{}", std::fs::read_link(&entry)?.display())
        } else {
            anyhow::bail!("special file in recovery tree {}", entry.display())
        };
        out.insert(relative, identity);
    }
    Ok(out)
}

fn walkdir(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = vec![root.to_path_buf()];
    let mut cursor = 0;
    while cursor < out.len() {
        let path = out[cursor].clone();
        cursor += 1;
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            continue;
        }
        for item in std::fs::read_dir(path)? {
            out.push(item?.path());
        }
    }
    Ok(out)
}

#[cfg(not(windows))]
fn recovery_key(path: &Path) -> PathBuf {
    // Canonicalize only the parent. Following the final component would
    // turn `install/current` into its release target and recover the
    // wrong object.
    path.parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .and_then(|parent| path.file_name().map(|name| parent.join(name)))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Windows: `canonicalize` returns a verbatim `\\?\C:\...` path, and only for
/// a directory that exists. A path captured before its parent existed and
/// marked after must still map to ONE key, so the nearest existing ancestor
/// is canonicalized, the verbatim prefix dropped, and the rest appended.
#[cfg(windows)]
fn recovery_key(path: &Path) -> PathBuf {
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let mut existing = parent.to_path_buf();
    let mut rest = Vec::new();
    let base = loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            break canonical;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(up)) => {
                rest.push(name.to_os_string());
                existing = up.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    };
    let text = base.to_string_lossy();
    let mut key = match text.strip_prefix(r"\\?\") {
        Some(stripped) if !stripped.starts_with("UNC") => PathBuf::from(stripped),
        _ => base.clone(),
    };
    for part in rest.iter().rev() {
        key.push(part);
    }
    match path.file_name() {
        Some(name) => key.join(name),
        None => path.to_path_buf(),
    }
}

fn temporary(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.recovery-{}.tmp", std::process::id()))
}

#[derive(Debug)]
pub(crate) struct Recovery {
    before: BTreeMap<PathBuf, Snapshot>,
    after: BTreeMap<PathBuf, Snapshot>,
    /// Owner of each pre-existing file or link. Kept out of the snapshot so
    /// a reviewed chown after a mark is not mistaken for a foreign edit; a
    /// privileged restore puts the original owner back (a restored quadlet
    /// must stay its account's, not become root's).
    owners: BTreeMap<PathBuf, (u32, u32)>,
}

#[cfg(unix)]
fn owner_of(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.is_file() || meta.file_type().is_symlink())
        .map(|meta| (meta.uid(), meta.gid()))
}

#[cfg(not(unix))]
fn owner_of(_path: &Path) -> Option<(u32, u32)> {
    None
}

#[cfg(unix)]
fn restore_owner(path: &Path, owner: (u32, u32)) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    if (meta.uid(), meta.gid()) == owner {
        return Ok(());
    }
    std::os::unix::fs::lchown(path, Some(owner.0), Some(owner.1))
        .with_context(|| format!("restoring the owner of {}", path.display()))
}

#[cfg(not(unix))]
fn restore_owner(_path: &Path, _owner: (u32, u32)) -> Result<()> {
    Ok(())
}

impl Recovery {
    pub(crate) fn capture(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut before = BTreeMap::new();
        let mut owners = BTreeMap::new();
        for path in paths {
            let snapshot = Snapshot::capture(&path).with_context(|| {
                format!("capturing installer recovery state {}", path.display())
            })?;
            let key = recovery_key(&path);
            if let Some(owner) = owner_of(&path) {
                owners.insert(key.clone(), owner);
            }
            before.insert(key, snapshot);
        }
        Ok(Self {
            before,
            after: BTreeMap::new(),
            owners,
        })
    }

    pub(crate) fn track(&mut self, path: &Path) -> Result<()> {
        let key = recovery_key(path);
        if let std::collections::btree_map::Entry::Vacant(entry) = self.before.entry(key.clone()) {
            entry.insert(Snapshot::capture(path)?);
            if let Some(owner) = owner_of(path) {
                self.owners.insert(key, owner);
            }
        }
        Ok(())
    }

    pub(crate) fn mark_after(&mut self, path: &Path) -> Result<()> {
        let key = recovery_key(path);
        anyhow::ensure!(
            self.before.contains_key(&key),
            "uncaptured recovery path {}",
            path.display()
        );
        self.after.insert(key, Snapshot::capture(path)?);
        Ok(())
    }

    pub(crate) fn finish<T>(self, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(original) => {
                let mut failures = Vec::new();
                for (path, before) in self.before.iter().rev() {
                    let Some(after) = self.after.get(path) else {
                        continue;
                    };
                    match Snapshot::capture(path) {
                        Ok(Snapshot::Directory { files: current, .. }) if matches!(after, Snapshot::Directory { files:expected, .. } if current == *expected) =>
                            {}
                        Ok(current) if current != *after => {
                            failures.push(format!(
                                "{} changed after the failed operation; left untouched",
                                path.display()
                            ));
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            failures.push(format!("could not inspect {}: {e}", path.display()));
                            continue;
                        }
                    }
                    if let Err(e) = before.restore(path) {
                        failures.push(format!("could not restore {}: {e:#}", path.display()));
                    } else if let Some(owner) = self.owners.get(path) {
                        if let Err(e) = restore_owner(path, *owner) {
                            failures.push(format!("{e:#}"));
                        }
                    }
                }
                if failures.is_empty() {
                    Err(original.context("installer operation failed; its recorded file and selection changes were restored"))
                } else {
                    Err(original.context(format!("RECOVERY INCOMPLETE: {}; inspect install.toml and config backups before retrying", failures.join("; "))))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_only_marked_files_and_preserves_a_newer_foreign_edit() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let record = temp.path().join("install.toml");
        std::fs::write(&config, "old\n").unwrap();
        std::fs::write(&record, "record\n").unwrap();
        let mut recovery = Recovery::capture([config.clone(), record.clone()]).unwrap();
        std::fs::write(&config, "new\n").unwrap();
        recovery.mark_after(&config).unwrap();
        assert!(recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&record).unwrap(), "record\n");

        let mut recovery = Recovery::capture([config.clone()]).unwrap();
        std::fs::write(&config, "ours\n").unwrap();
        recovery.mark_after(&config).unwrap();
        std::fs::write(&config, "someone else\n").unwrap();
        let err = recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .unwrap_err();
        assert!(format!("{err:#}").contains("RECOVERY INCOMPLETE"));
        assert_eq!(std::fs::read_to_string(config).unwrap(), "someone else\n");
    }

    #[test]
    fn a_new_payload_is_removed_only_when_its_tree_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("payload");
        let mut recovery = Recovery::capture([root.clone()]).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/veles"), "one").unwrap();
        recovery.mark_after(&root).unwrap();
        let error = recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .unwrap_err();
        assert!(
            !root.exists(),
            "unchanged new tree should be removed: {error:#}"
        );

        let mut recovery = Recovery::capture([root.clone()]).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/veles"), "one").unwrap();
        recovery.mark_after(&root).unwrap();
        std::fs::write(root.join("foreign"), "keep").unwrap();
        let error = recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .unwrap_err();
        assert!(format!("{error:#}").contains("RECOVERY INCOMPLETE"));
        assert_eq!(
            std::fs::read_to_string(root.join("foreign")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn restores_an_existing_file_with_platform_replace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plugin.toml");
        std::fs::write(&path, "before\n").unwrap();
        let mut recovery = Recovery::capture([path.clone()]).unwrap();
        std::fs::write(&path, "after\n").unwrap();
        recovery.mark_after(&path).unwrap();
        assert!(recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before\n");
    }

    #[cfg(unix)]
    #[test]
    fn restores_the_original_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("component-state");
        std::fs::write(&path, "before\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut recovery = Recovery::capture([path.clone()]).unwrap();
        std::fs::write(&path, "after\n").unwrap();
        recovery.mark_after(&path).unwrap();
        assert!(recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .is_err());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn restores_a_symlink_selection() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        std::os::unix::fs::symlink("one", &current).unwrap();
        let mut recovery = Recovery::capture([current.clone()]).unwrap();
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink("two", &current).unwrap();
        recovery.mark_after(&current).unwrap();
        assert!(recovery
            .finish::<()>(Err(anyhow::anyhow!("injected")))
            .is_err());
        assert_eq!(std::fs::read_link(current).unwrap(), PathBuf::from("one"));
    }
}
