//! Typed actions. A preview describes the same values apply will use;
//! unimplemented privileged operations stay `Unsupported`, never shell text
//! mistaken for an executed action.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component as PathComponent, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    Missing,
    Different,
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    WriteFile {
        path: PathBuf,
        /// Byte array over JSON; the preview carries a digest, not just
        /// a description. Never skipped during serialization.
        contents: Vec<u8>,
        mode: Option<u32>,
        expected_sha256: Option<String>,
    },
    Package {
        manager: String,
        packages: Vec<String>,
    },
    Command {
        argv: Vec<String>,
        as_user: Option<String>,
        privileged: bool,
    },
    SystemdUnit {
        name: String,
        contents: String,
        privileged: bool,
    },
    Quadlet {
        name: String,
        contents: String,
        owner: Option<String>,
    },
    Image {
        source: String,
        digest: Option<String>,
        owner: Option<String>,
    },
    Download {
        url: String,
        sha256: String,
        destination: PathBuf,
    },
    ConfigEdit {
        path: PathBuf,
        values: std::collections::BTreeMap<String, toml::Value>,
    },
    Secret {
        name: String,
    },
}

pub fn sha256(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn relative(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "an action path cannot be empty"
    );
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            PathComponent::Normal(s) => out.push(s),
            _ => bail!(
                "action path {} must be relative and contain no . or ..",
                path.display()
            ),
        }
    }
    Ok(out)
}

fn destination(root: &Path, path: &Path) -> Result<PathBuf> {
    let meta = std::fs::symlink_metadata(root)
        .with_context(|| format!("install root {} does not exist", root.display()))?;
    anyhow::ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "install root must be a real directory"
    );
    let mut current = root.to_path_buf();
    let path = relative(path)?;
    let count = path.components().count();
    for (n, part) in path.components().enumerate() {
        current.push(part.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("refusing symlink in action path {}", current.display())
            }
            Ok(m) if n + 1 < count && !m.is_dir() => {
                bail!("action parent is not a directory: {}", current.display())
            }
            Ok(m) if n + 1 == count && !m.is_file() => {
                bail!("action target is not a regular file: {}", current.display())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if n + 1 < count {
                    std::fs::create_dir(&current)?;
                }
            }
            Err(e) => return Err(e.into()),
            _ => {}
        }
    }
    Ok(current)
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Self::WriteFile { path, contents, .. } => format!(
                "write {} ({} bytes; sha256 {})",
                path.display(),
                contents.len(),
                sha256(contents)
            ),
            Self::Package { manager, packages } => {
                format!("install with {manager}: {}", packages.join(" "))
            }
            Self::Command {
                argv,
                as_user,
                privileged,
            } => format!(
                "run {}{}: {}",
                if *privileged { "privileged " } else { "" },
                as_user.as_deref().unwrap_or("current user"),
                argv.join(" ")
            ),
            Self::SystemdUnit {
                name, privileged, ..
            } => format!(
                "{} systemd unit {name}",
                if *privileged {
                    "install system"
                } else {
                    "install user"
                }
            ),
            Self::Quadlet { name, owner, .. } => format!(
                "install quadlet {name} for {}",
                owner.as_deref().unwrap_or("current user")
            ),
            Self::Image {
                source,
                digest,
                owner,
            } => format!(
                "load image {source} ({}) for {}",
                digest.as_deref().unwrap_or("digest required"),
                owner.as_deref().unwrap_or("current user")
            ),
            Self::Download {
                url,
                sha256,
                destination,
            } => format!(
                "download {url} to {} (sha256 {sha256})",
                destination.display()
            ),
            Self::ConfigEdit { path, values } => format!(
                "edit {} configuration keys in {}",
                values.len(),
                path.display()
            ),
            Self::Secret { name } => {
                format!("store secret {name} in the vault (value not in plan)")
            }
        }
    }

    pub fn check(&self, root: &Path) -> Result<Status> {
        match self {
            Self::WriteFile {
                path,
                contents,
                expected_sha256,
                ..
            } => {
                let dest = destination(root, path)?;
                let existing = match std::fs::read(&dest) {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Status::Missing)
                    }
                    Err(e) => return Err(e.into()),
                };
                if existing == *contents {
                    return Ok(Status::Done);
                }
                if expected_sha256
                    .as_deref()
                    .is_some_and(|h| sha256(&existing) == h)
                {
                    return Ok(Status::Different);
                }
                bail!(
                    "{} has different content not owned by this action; refusing to overwrite",
                    dest.display()
                )
            }
            Self::ConfigEdit { .. } => Ok(Status::Unsupported(
                "configuration edits are planned through veles configedit".into(),
            )),
            _ => Ok(Status::Unsupported(
                "this action needs its component executor; preview only".into(),
            )),
        }
    }

    /// The foundation executes only local regular-file writes. Privileged
    /// commands are deliberately not inferred from an action description.
    /// Execute the explicitly reviewed privileged subset. This never
    /// elevates by itself: the operator starts the CLI with the intended
    /// privileges. Ordinary `Plan::apply` cannot reach this entry point.
    pub fn apply_privileged(&self, root: &Path) -> Result<Status> {
        match self {
            Self::WriteFile { .. } => self.apply(root),
            Self::Command { argv, as_user: None, privileged: true } => {
                let (program, arguments) = argv.split_first().context("privileged command has no argv")?;
                anyhow::ensure!(!program.is_empty(), "privileged command program is empty");
                let status = std::process::Command::new(program).args(arguments)
                    // Diagnostics only: stdout carries the JSON review and report.
                    .stdout(std::process::Stdio::from(std::io::stderr()))
                    .status()
                    .with_context(|| format!("running reviewed privileged command {program}"))?;
                anyhow::ensure!(status.success(), "reviewed privileged command {program} exited with {status}");
                Ok(Status::Done)
            }
            Self::Command { privileged: false, .. } => bail!("reviewed command is not marked privileged"),
            Self::Command { .. } => bail!("privileged account switching requires an explicit reviewed service unit, not an implicit runner"),
            _ => bail!("{}: no privileged executor", self.describe()),
        }
    }

    pub fn apply(&self, root: &Path) -> Result<Status> {
        match self {
            Self::WriteFile {
                path,
                contents,
                mode,
                ..
            } => {
                if self.check(root)? == Status::Done {
                    return Ok(Status::Done);
                }
                let dest = destination(root, path)?;
                let parent = dest.parent().context("action file needs a parent")?;
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let temp = parent.join(format!(".veles-install-{}-{id}.tmp", std::process::id()));
                let result = (|| -> Result<()> {
                    let mut opts = std::fs::OpenOptions::new();
                    opts.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        opts.mode(0o600);
                    }
                    let mut f = opts.open(&temp)?;
                    // Unix already opens 0600. Windows needs an explicit
                    // owner-only DACL instead of inherited permissions.
                    #[cfg(windows)]
                    veles_core::platform::restrict_file(&temp)?;
                    std::io::Write::write_all(&mut f, contents)?;
                    f.sync_all()?;
                    #[cfg(windows)]
                    let _ = mode;
                    #[cfg(unix)]
                    if let Some(mode) = mode {
                        use std::os::unix::fs::PermissionsExt;
                        anyhow::ensure!(mode & !0o7777 == 0, "invalid file mode {mode:o}");
                        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(*mode))?;
                    }
                    veles_core::platform::replace_file(&temp, &dest)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                result?;
                Ok(Status::Done)
            }
            Self::ConfigEdit { .. } => {
                bail!("use the shared configuration plan/apply API, not a file overwrite")
            }
            _ => bail!(
                "{}: no executor yet; this plan is preview-only",
                self.describe()
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub profile: crate::Profile,
    pub actions: Vec<Action>,
    pub privileged_script: Option<String>,
}

impl Plan {
    pub fn preview(&self) -> Vec<String> {
        self.actions.iter().map(Action::describe).collect()
    }
    pub fn apply(&self, root: &Path) -> Result<Vec<Status>> {
        self.actions.iter().map(|a| a.apply(root)).collect()
    }
}
