//! Versioned bundle lifecycle. Payload directories are immutable once
//! selected: a running gateway never has its mapped libraries overwritten.
use crate::{Component, HistoryEntry, InstallRecord};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component as PathComponent, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactTrust {
    /// An already verified local directory supplied explicitly by the operator.
    LocalDirectory,
    /// The bootstrap checked the artifact against the signed inventory
    /// AND its Sigstore bundle; this flag is not inferred from a URL.
    SignatureVerified,
}

fn check_architecture(info: &BundleInfo) -> Result<()> {
    let expected = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        std::env::consts::ARCH
    };
    anyhow::ensure!(
        info.architecture == expected,
        "bundle is for {}, this host is {expected}",
        info.architecture
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleInfo {
    pub version: String,
    pub revision: String,
    pub architecture: String,
}

fn version_id(info: &BundleInfo) -> Result<String> {
    crate::valid_version(&info.version)?;
    crate::valid_version(&info.revision)?;
    let s = format!("{}-{}", info.version, info.revision);
    crate::valid_version(&s)?;
    Ok(s)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    anyhow::ensure!(
        !meta.file_type().is_symlink(),
        "bundle root cannot be a symlink"
    );
    std::fs::create_dir(dst)?;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        for item in std::fs::read_dir(&from)? {
            let item = item?;
            let name = item.file_name();
            anyhow::ensure!(name != "." && name != "..", "invalid bundle path");
            let source = item.path();
            let dest = to.join(name);
            let kind = item.file_type()?;
            if kind.is_dir() {
                std::fs::create_dir(&dest)?;
                stack.push((source, dest));
            } else if kind.is_file() {
                std::fs::copy(&source, &dest)?;
                std::fs::set_permissions(&dest, item.metadata()?.permissions())?;
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&source)?;
                let relative_parent = source
                    .parent()
                    .unwrap()
                    .strip_prefix(src)
                    .context("bundle source escaped its root")?;
                safe_link_target(relative_parent, &target)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &dest)?;
                #[cfg(not(unix))]
                bail!("bundle symlinks require a native Windows junction/link implementation");
            } else {
                bail!("unsupported special file in bundle: {}", source.display());
            }
        }
    }
    Ok(())
}

fn safe_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            PathComponent::Normal(s) => out.push(s),
            PathComponent::CurDir => {}
            _ => bail!("unsafe bundle path {}", path.display()),
        }
    }
    anyhow::ensure!(!out.as_os_str().is_empty(), "empty bundle path");
    Ok(out)
}

fn safe_link_target(parent: &Path, target: &Path) -> Result<()> {
    // The portable bundle carries its own Nix closure under `nix/store`,
    // and its launcher bind-mounts that tree on /nix (nix/bundle.nix). The
    // closure's own links name absolute /nix/store paths, which resolve
    // inside the bundle at run time. Only those, only from inside that
    // tree, and only as plain downward paths are accepted.
    if target.is_absolute() {
        let inside_closure = parent.starts_with("nix/store");
        let mut parts = target.components();
        let store = matches!(parts.next(), Some(PathComponent::RootDir))
            && parts.next() == Some(PathComponent::Normal("nix".as_ref()))
            && parts.next() == Some(PathComponent::Normal("store".as_ref()))
            && parts.clone().next().is_some()
            && parts.all(|c| matches!(c, PathComponent::Normal(_)));
        anyhow::ensure!(
            inside_closure && store,
            "absolute bundle symlink target {}",
            target.display()
        );
        return Ok(());
    }
    let mut depth = 0usize;
    for p in parent.components() {
        if matches!(p, PathComponent::Normal(_)) {
            depth += 1;
        }
    }
    for c in target.components() {
        match c {
            PathComponent::Normal(_) | PathComponent::CurDir => {}
            PathComponent::ParentDir => {
                depth = depth
                    .checked_sub(1)
                    .context("bundle symlink escapes its tree")?;
            }
            _ => bail!("unsafe bundle symlink target {}", target.display()),
        }
    }
    Ok(())
}

fn safe_archive_path(path: &Path) -> Result<Option<PathBuf>> {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            PathComponent::Normal(part) => out.push(part),
            PathComponent::CurDir => {}
            _ => bail!("unsafe bundle path {}", path.display()),
        }
    }
    Ok((!out.as_os_str().is_empty()).then_some(out))
}

pub(crate) fn unpack_archive(archive: &Path, stage: &Path) -> Result<()> {
    anyhow::ensure!(
        !stage.exists(),
        "unpack staging already exists: {}",
        stage.display()
    );
    std::fs::create_dir(stage)?;
    let result = (|| {
        let file = std::fs::File::open(archive)?;
        let decoder = zstd::stream::read::Decoder::new(file)?;
        let mut tar = tar::Archive::new(decoder);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let entry_path = entry.path()?.into_owned();
            let kind = entry.header().entry_type();
            let Some(relative) = safe_archive_path(&entry_path)? else {
                anyhow::ensure!(kind.is_dir(), "non-directory archive root");
                continue;
            };
            if kind.is_symlink() {
                let target = entry.link_name()?.context("symlink without a target")?;
                safe_link_target(relative.parent().unwrap_or(Path::new("")), &target)?;
            } else if !(kind.is_file() || kind.is_dir()) {
                bail!(
                    "unsupported archive entry type {:?} at {}",
                    kind,
                    entry.path()?.display()
                );
            }
            entry.unpack_in(stage)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(stage);
    }
    result
}

/// Unpack a signed-verified local archive. Signature verification is the
/// bootstrap's gate, NOT a flag this function invents. The tar crate's
/// unpack_in rejects path traversal; symlink targets get an explicit check.
pub fn install_bundle_archive(
    root: &Path,
    archive: &Path,
    trust: ArtifactTrust,
    record: &mut InstallRecord,
) -> Result<Component> {
    install_bundle_archive_reviewed(root, archive, trust, None, record)
}

/// Archive install with the same reviewed-tree guard as a local bundle.
/// The signed bootstrap computes the preview from these unpacked bytes;
/// no caller can replace that preview and still use its plan signature.
pub(crate) fn install_bundle_archive_reviewed(
    root: &Path,
    archive: &Path,
    trust: ArtifactTrust,
    plan_sig: Option<&str>,
    record: &mut InstallRecord,
) -> Result<Component> {
    anyhow::ensure!(
        trust == ArtifactTrust::SignatureVerified,
        "an archive must pass signed-inventory and Sigstore verification before installation"
    );
    let parent = archive.parent().context("archive needs a parent")?;
    let stage = parent.join(format!(".veles-unpack-{}", std::process::id()));
    let result = (|| -> Result<Component> {
        unpack_archive(archive, &stage)?;
        install_bundle_tree(root, &stage, trust, record, plan_sig)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Public entry point for an already verified archive or an explicitly
/// reviewed directory. Neither can infer trust from an HTTP request.
fn switch_current(root: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let temp = root.join(format!(".current-{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(target, &temp)?;
        std::fs::rename(&temp, root.join("current"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (root, target);
        bail!("Windows bundle selection needs its native junction implementation; not claiming Unix semantics")
    }
}

fn read_component(record: &InstallRecord, name: &str) -> Result<Component> {
    record
        .components
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .with_context(|| format!("component {name} is not installed"))
}

fn component_from_tree(
    root: &Path,
    info: &BundleInfo,
    tree: &Path,
    id: &str,
    previous: Option<Box<Component>>,
) -> Result<Component> {
    let mut artifacts = std::collections::BTreeMap::new();
    let mut files = Vec::new();
    collect_files(tree, tree, &mut files)?;
    files.sort();
    for file in files {
        let rel = file
            .strip_prefix(tree)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&file)?;
        artifacts.insert(rel, crate::sha256(&bytes));
    }
    Ok(Component {
        name: "core".into(),
        kind: None,
        version: format!("{}-{}", info.version, info.revision),
        options: Default::default(),
        artifacts,
        source: Some(root.join("releases").join(id).display().to_string()),
        previous,
    })
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_dir() {
            collect_files(root, &item.path(), out)?;
        } else if kind.is_file() {
            out.push(item.path());
        } else if kind.is_symlink() {
            let path = item.path();
            let relative = path
                .parent()
                .unwrap()
                .strip_prefix(root)
                .context("bundle tree escaped its root")?;
            safe_link_target(relative, &std::fs::read_link(&path)?)?;
        } else {
            bail!(
                "unsupported special file in installed bundle: {}",
                item.path().display()
            );
        }
    }
    Ok(())
}

/// The exact reviewed local bundle. A signature covers the metadata,
/// every regular file and every link target, not just its version label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundlePlan {
    pub info: BundleInfo,
    pub plan_sig: String,
    pub actions: Vec<String>,
    pub unsupported: Vec<String>,
}

impl BundlePlan {
    pub fn review(&self) -> serde_json::Value {
        serde_json::json!({
            "preview": true,
            "confirm_required": true,
            "action": "install-bundle",
            "bundle": self.info,
            "plan_sig": self.plan_sig,
            "actions": self.actions,
            "unsupported": self.unsupported,
            "verification": "reviewed local directory; no signed-download or runtime-health claim",
        })
    }
}

pub fn review_bundle(source: &Path, install_root: &Path) -> Result<BundlePlan> {
    anyhow::ensure!(
        source.is_absolute(),
        "bundle source must be an absolute local path"
    );
    let source = source
        .canonicalize()
        .context("bundle directory is unavailable")?;
    let info: BundleInfo = serde_json::from_slice(
        &std::fs::read(source.join("BUNDLE-INFO.json")).context("not a bundle directory")?,
    )?;
    check_architecture(&info)?;
    version_id(&info)?;
    anyhow::ensure!(source.join("bin/veles").is_file(), "bundle lacks bin/veles");
    let tree_sig = tree_signature(&source)?;
    Ok(BundlePlan {
        plan_sig: crate::sha256(&serde_json::to_vec(&(&info, tree_sig))?),
        actions: vec![
            format!(
                "install reviewed local bundle {}-{} into {}",
                info.version,
                info.revision,
                install_root.display()
            ),
            "switch current atomically and retain the previous payload".into(),
        ],
        unsupported: vec![
            "a local directory is not a signed-download claim; restart is separate".into(),
        ],
        info,
    })
}

/// Deterministic inventory of the bytes and links that copy_tree will
/// preserve. A version label alone is not an artifact identity.
fn tree_signature(root: &Path) -> Result<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String, String)>) -> Result<()> {
        let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            match entry.file_type()? {
                kind if kind.is_dir() => {
                    out.push((relative, "directory".into(), String::new()));
                    walk(root, &path, out)?;
                }
                kind if kind.is_file() => {
                    out.push((
                        relative,
                        "file".into(),
                        crate::sha256(&std::fs::read(&path)?),
                    ));
                }
                kind if kind.is_symlink() => {
                    let target = std::fs::read_link(&path)?;
                    safe_link_target(path.parent().unwrap().strip_prefix(root)?, &target)?;
                    out.push((
                        relative,
                        "link".into(),
                        target.to_string_lossy().into_owned(),
                    ));
                }
                _ => bail!(
                    "unsupported special file in reviewed bundle: {}",
                    path.display()
                ),
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    Ok(crate::sha256(&serde_json::to_vec(&entries)?))
}

pub(crate) fn install_reviewed_bundle(
    root: &Path,
    source: &Path,
    plan_sig: &str,
    record: &mut InstallRecord,
) -> Result<Component> {
    install_bundle_tree(
        root,
        source,
        ArtifactTrust::LocalDirectory,
        record,
        Some(plan_sig),
    )
}

pub fn install_bundle(
    root: &Path,
    source: &Path,
    trust: ArtifactTrust,
    record: &mut InstallRecord,
) -> Result<Component> {
    anyhow::ensure!(
        trust == ArtifactTrust::LocalDirectory,
        "install_bundle takes only an explicitly reviewed local directory"
    );
    install_bundle_tree(root, source, trust, record, None)
}

fn install_bundle_tree(
    root: &Path,
    source: &Path,
    trust: ArtifactTrust,
    record: &mut InstallRecord,
    expected_sig: Option<&str>,
) -> Result<Component> {
    anyhow::ensure!(
        matches!(
            trust,
            ArtifactTrust::LocalDirectory | ArtifactTrust::SignatureVerified
        ),
        "bundle trust must be decided by the caller"
    );
    let source = source
        .canonicalize()
        .context("bundle directory is unavailable")?;
    let info: BundleInfo = serde_json::from_slice(
        &std::fs::read(source.join("BUNDLE-INFO.json")).context("bundle lacks BUNDLE-INFO.json")?,
    )?;
    check_architecture(&info)?;
    let id = version_id(&info)?;
    anyhow::ensure!(source.join("bin/veles").is_file(), "bundle lacks bin/veles");
    std::fs::create_dir_all(root.join("releases"))?;
    let destination = root.join("releases").join(&id);
    if destination.exists() {
        // Existing version must be identical; never silently reuse an
        // id for different bytes or overwrite a running release.
        let candidate = component_from_tree(root, &info, &source, &id, None)?;
        let existing = component_from_tree(root, &info, &destination, &id, None)?;
        if let Some(expected) = expected_sig {
            anyhow::ensure!(
                review_bundle(&destination, root)
                    .with_context(|| format!(
                        "reviewing retained release {}",
                        destination.display()
                    ))?
                    .plan_sig
                    == expected,
                "retained release differs from the reviewed bundle"
            );
        }
        anyhow::ensure!(
            candidate.artifacts == existing.artifacts,
            "release {id} already exists with different content"
        );
    } else {
        let stage = root
            .join("releases")
            .join(format!(".{id}.staging-{}", std::process::id()));
        anyhow::ensure!(!stage.exists(), "stale staging path {}", stage.display());
        copy_tree(&source, &stage)?;
        crate::bundle_copy_hook();
        if let Some(expected) = expected_sig {
            let staged = review_bundle(&stage, root)
                .with_context(|| format!("reviewing staged bundle {}", stage.display()))?;
            anyhow::ensure!(
                staged.plan_sig == expected,
                "bundle changed during staged copy; refusing to select unreviewed bytes"
            );
        }
        std::fs::rename(&stage, &destination)?;
        record.note_payload(&destination)?;
    }
    let old = record.components.iter().find(|c| c.name == "core").cloned();
    let component = component_from_tree(root, &info, &destination, &id, old.clone().map(Box::new))?;
    switch_current(root, &destination)?;
    record.components.retain(|c| c.name != "core");
    record.components.push(component.clone());
    if old.as_ref().is_some_and(|o| o.version != component.version) {
        record.history.push(HistoryEntry {
            operation: "upgrade".into(),
            component: "core".into(),
            from_version: old.map(|o| o.version),
            to_version: component.version.clone(),
            config_before_sha256: None,
            config_after_sha256: None,
            config_snapshot: None,
            options_before: Default::default(),
            config_file_existed: None,
        });
    }
    Ok(component)
}

pub fn verify_component(root: &Path, component: &Component) -> Result<Vec<String>> {
    let source = component
        .source
        .as_ref()
        .map(PathBuf::from)
        .context("component has no installed payload")?;
    if source.is_file() && component.artifacts.len() == 1 {
        component
            .kind
            .context("a single-file artifact must be a module or process component")?;
        let canonical = source
            .canonicalize()
            .with_context(|| format!("canonicalizing installed component {}", source.display()))?;
        let expected = root
            .join("components")
            .join(&component.name)
            .canonicalize()
            .with_context(|| format!("resolving managed component root for {}", component.name))?;
        anyhow::ensure!(
            canonical.starts_with(&expected),
            "{} payload is outside its managed component root",
            component.name
        );
        let (name, hash) = component.artifacts.first_key_value().unwrap();
        let bytes = std::fs::read(&canonical)?;
        if crate::sha256(&bytes) != *hash {
            return Ok(vec![format!(
                "{}: {name} SHA-256 differs",
                source.display()
            )]);
        }
        return Ok(Vec::new());
    }
    let dir = source;
    anyhow::ensure!(
        dir.starts_with(root.join("releases")) && dir.is_dir(),
        "installed payload is missing or outside the release root"
    );
    let mut problems = Vec::new();
    for (relative, expected) in &component.artifacts {
        let relative = safe_path(Path::new(relative))?;
        let path = dir.join(relative);
        match std::fs::read(&path) {
            Ok(bytes) if crate::sha256(&bytes) == *expected => {}
            Ok(_) => problems.push(format!("{}: SHA-256 differs", path.display())),
            Err(e) => problems.push(format!("{}: {e}", path.display())),
        }
    }
    let mut actual = Vec::new();
    collect_files(&dir, &dir, &mut actual)?;
    if actual.len() != component.artifacts.len() {
        problems.push(format!(
            "{}: unexpected file count {} (recorded {})",
            dir.display(),
            actual.len(),
            component.artifacts.len()
        ));
    }
    Ok(problems)
}

pub fn rollback(root: &Path, record: &mut InstallRecord, name: &str) -> Result<Component> {
    crate::valid_component_name(name)?;
    let current = read_component(record, name)?;
    let previous = current
        .previous
        .clone()
        .context("no previous version is recorded")?;
    let target = previous
        .source
        .as_ref()
        .map(PathBuf::from)
        .context("previous version has no retained payload")?;
    anyhow::ensure!(
        target.starts_with(root.join("releases")) && target.is_dir(),
        "previous payload is missing or outside the release root"
    );
    let problems = verify_component(root, &previous)?;
    anyhow::ensure!(
        problems.is_empty(),
        "retained payload failed verification: {}",
        problems.join("; ")
    );
    switch_current(root, &target)?;
    let to = previous.version.clone();
    let mut restored = *previous;
    restored.previous = Some(Box::new(current.clone()));
    record.components.retain(|c| c.name != name);
    record.components.push(restored.clone());
    record.history.push(HistoryEntry {
        operation: "rollback".into(),
        component: name.into(),
        from_version: Some(current.version),
        to_version: to,
        config_before_sha256: None,
        config_after_sha256: None,
        config_snapshot: None,
        options_before: Default::default(),
        config_file_existed: None,
    });
    Ok(restored)
}

#[cfg(all(test, unix))]
mod link_tests {
    use super::safe_link_target;
    use std::path::Path;

    #[test]
    fn only_the_bundled_closure_may_link_into_nix_store() {
        let closure = Path::new("nix/store/abc-gcc/lib");
        assert!(
            safe_link_target(closure, Path::new("/nix/store/def-libgcc/lib/libgcc_s.so")).is_ok()
        );
        assert!(safe_link_target(closure, Path::new("../lib64")).is_ok());
        // Outside the closure, or anywhere but a plain /nix/store path: no.
        assert!(safe_link_target(Path::new("bin"), Path::new("/nix/store/def/bin/veles")).is_err());
        assert!(safe_link_target(closure, Path::new("/etc/passwd")).is_err());
        assert!(safe_link_target(closure, Path::new("/nix/store/../../etc/passwd")).is_err());
        assert!(safe_link_target(closure, Path::new("/nix/store")).is_err());
        assert!(safe_link_target(Path::new("bin"), Path::new("../../escape")).is_err());
    }
}
