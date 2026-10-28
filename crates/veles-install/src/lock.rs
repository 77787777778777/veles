//! Process-independent exclusion for installer mutations. The lock is
//! acquired after an interactive preview and before answers/files are
//! revalidated. It is never held across a prompt or an async reload.
use anyhow::{Context, Result};
use std::path::Path;
#[cfg(all(test, unix))]
use std::path::PathBuf;

pub(crate) struct OperationLock {
    _file: std::fs::File,
    #[cfg(all(test, unix))]
    pub(crate) path: PathBuf,
}

impl OperationLock {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        if root.exists() {
            let metadata = std::fs::symlink_metadata(root)?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "installer root must be a real directory, not a symlink: {}",
                root.display()
            );
        } else {
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating installer root {}", root.display()))?;
            let metadata = std::fs::symlink_metadata(root)?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "installer root became a symlink: {}",
                root.display()
            );
        }
        let path = root.join(".operation.lock");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        veles_core::platform::private_open_options(&mut options);
        let file = options
            .open(&path)
            .with_context(|| format!("opening installer lock {}", path.display()))?;
        // std uses flock on Unix and LockFileEx on Windows. Advisory
        // locking is sufficient only if EVERY mutating entry point uses
        // this helper; CLI, direct service and HTTP all do.
        file.lock().with_context(|| {
            format!(
                "waiting for another installer operation at {}",
                path.display()
            )
        })?;
        Ok(Self {
            _file: file,
            #[cfg(all(test, unix))]
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn another_process_cannot_mutate_until_the_lock_is_released() {
        let temp = tempfile::tempdir().unwrap();
        let lock = super::OperationLock::acquire(temp.path()).unwrap();
        let probe = std::process::Command::new("flock")
            .arg("-n")
            .arg(&lock.path)
            .arg("-c")
            .arg("exit 7")
            .status()
            .unwrap();
        assert!(!probe.success());
        drop(lock);
        let probe = std::process::Command::new("flock")
            .arg("-n")
            .arg(temp.path().join(".operation.lock"))
            .arg("-c")
            .arg("exit 0")
            .status()
            .unwrap();
        assert!(probe.success());
    }
}
