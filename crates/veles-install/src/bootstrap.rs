//! Signed release bootstrap. Downloaded metadata never decides its own
//! trust: the operator names the release and workflow identity, then both
//! the manifest and selected archive pass the same cosign verification.
use crate::{ArtifactTrust, BundlePlan, Component, InstallRecord};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MANIFEST_LIMIT: u64 = 2 * 1024 * 1024;
const EVIDENCE_LIMIT: u64 = 16 * 1024 * 1024;
const ARCHIVE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRequest {
    pub release_url: String,
    pub repository: String,
    pub tag: String,
    pub commit: String,
    pub workflow_sha: String,
}

impl ReleaseRequest {
    fn validate_url(&self) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(&self.release_url).context("invalid release URL")?;
        #[cfg(all(any(test, feature = "test-support"), unix))]
        let loopback = std::env::var_os("VELES_TEST_RELEASE_UNIX_SOCKET").is_some()
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        #[cfg(not(all(any(test, feature = "test-support"), unix)))]
        let loopback = false;
        if loopback {
            anyhow::ensure!(
                url.scheme() == "http",
                "test release transport must be loopback HTTP"
            );
            return Ok(url);
        }
        anyhow::ensure!(url.scheme() == "https", "release download requires HTTPS");
        anyhow::ensure!(
            url.host_str() == Some("github.com"),
            "release URL must name github.com"
        );
        Ok(url)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "signed bootstrap currently supports Linux bundles only"
        );
        anyhow::ensure!(
            !self.repository.trim().is_empty()
                && !self.tag.trim().is_empty()
                && !self.commit.trim().is_empty()
                && !self.workflow_sha.trim().is_empty(),
            "signed download requires explicit repository, tag, source commit and workflow SHA"
        );
        let url = self.validate_url()?;
        anyhow::ensure!(url.username().is_empty() && url.password().is_none());
        let path = format!("/{}/releases/download/{}/", self.repository, self.tag);
        anyhow::ensure!(
            url.path() == path,
            "release URL does not match repository and tag"
        );
        anyhow::ensure!(url.query().is_none() && url.fragment().is_none());
        anyhow::ensure!(
            self.repository
                .split('/')
                .collect::<Vec<_>>()
                .iter()
                .all(|part| valid_owner_part(part))
                && self.repository.split('/').count() == 2,
            "invalid release repository"
        );
        anyhow::ensure!(
            valid_sha(&self.commit),
            "source commit must be a full lowercase Git SHA"
        );
        anyhow::ensure!(
            valid_sha(&self.workflow_sha),
            "workflow SHA must be a full lowercase Git SHA"
        );
        anyhow::ensure!(valid_tag(&self.tag), "invalid release tag");
        Ok(())
    }

    fn certificate_identity(&self) -> String {
        format!(
            "https://github.com/{}/.github/workflows/release.yml@refs/tags/{}",
            self.repository, self.tag
        )
    }
}

fn valid_owner_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_tag(tag: &str) -> bool {
    let Some(version) = tag.strip_prefix('v') else {
        return false;
    };
    let mut pieces = version.splitn(3, '.');
    let (Some(major), Some(minor), Some(rest)) = (pieces.next(), pieces.next(), pieces.next())
    else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && !rest.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && minor.bytes().all(|b| b.is_ascii_digit())
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    repository: String,
    tag: String,
    commit: String,
    version: String,
    systems: Vec<String>,
    source: String,
    signing: Signing,
    artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Deserialize)]
struct Signing {
    files: String,
    oci: String,
    android: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtifact {
    name: String,
    sha256: String,
    size: u64,
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignedDownload {
    pub archive_name: String,
    pub archive_sha256: String,
    pub manifest_sha256: String,
    pub version: String,
    pub revision: String,
    pub repository: String,
    pub tag: String,
    pub commit: String,
    pub workflow_sha: String,
    pub verification: String,
}

struct Downloaded {
    manifest_version: String,
    manifest_sha256: String,
    archive_name: String,
    archive_sha256: String,
    archive: PathBuf,
}

fn stage_path(stage: &Path, name: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 240
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')),
        "unsafe release artifact name {name:?}"
    );
    Ok(stage.join(name))
}

async fn fetch(client: &reqwest::Client, url: &str, path: &Path, limit: u64) -> Result<()> {
    let response = tokio::time::timeout(Duration::from_secs(60), client.get(url).send())
        .await
        .context("release download connection timed out")?
        .with_context(|| format!("fetching {url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut response = response;
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("download {url} exceeds its size limit");
    }
    let mut file = std::fs::File::create(path)?;
    let mut length = 0u64;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), response.chunk())
        .await
        .context("release download stalled")??
    {
        length = length
            .checked_add(chunk.len() as u64)
            .context("download size overflow")?;
        anyhow::ensure!(length <= limit, "download {url} exceeds its size limit");
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    Ok(())
}

fn hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn cosign_program() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(path) = std::env::var_os("VELES_TEST_COSIGN") {
        return PathBuf::from(path);
    }
    PathBuf::from("cosign")
}

fn verify_blob(stage: &Path, name: &str, request: &ReleaseRequest) -> Result<()> {
    let file = stage_path(stage, name)?;
    let evidence = stage_path(stage, &format!("{name}.sigstore.json"))?;
    let output = std::process::Command::new(cosign_program())
        .args(["verify-blob", "--bundle"])
        .arg(&evidence)
        .args([
            "--certificate-identity",
            &request.certificate_identity(),
            "--certificate-oidc-issuer",
            "https://token.actions.githubusercontent.com",
            "--certificate-github-workflow-ref",
            &format!("refs/tags/{}", request.tag),
            "--certificate-github-workflow-sha",
            &request.workflow_sha,
        ])
        .arg(&file)
        .output()
        .context("cosign is required for signed bootstrap")?;
    anyhow::ensure!(
        output.status.success(),
        "cosign rejected {name}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn architecture() -> (&'static str, &'static str) {
    if cfg!(target_arch = "x86_64") {
        ("x86_64", "x86_64-linux")
    } else if cfg!(target_arch = "aarch64") {
        ("aarch64", "aarch64-linux")
    } else {
        (std::env::consts::ARCH, "")
    }
}

async fn download(request: &ReleaseRequest, stage: &Path) -> Result<Downloaded> {
    request.validate()?;
    #[cfg(all(any(test, feature = "test-support"), unix))]
    let client = {
        let mut builder = reqwest::Client::builder()
            .https_only(std::env::var_os("VELES_TEST_RELEASE_UNIX_SOCKET").is_none());
        // The real URL and its identity remain unchanged; this test seam
        // only chooses a loopback transport. There is no HTTP/insecure
        // production switch and no ambient test-env shortcut.
        if std::env::var_os("VELES_TEST_RELEASE_UNIX_SOCKET").is_some() {
            builder = builder.unix_socket(PathBuf::from(
                std::env::var_os("VELES_TEST_RELEASE_UNIX_SOCKET").unwrap(),
            ));
        }
        builder.build()?
    };
    #[cfg(not(all(any(test, feature = "test-support"), unix)))]
    let client = reqwest::Client::builder().https_only(true).build()?;
    let base = request.release_url.trim_end_matches('/');
    let manifest_path = stage_path(stage, "release-manifest.json")?;
    fetch(
        &client,
        &format!("{base}/release-manifest.json"),
        &manifest_path,
        MANIFEST_LIMIT,
    )
    .await?;
    let manifest_sig = stage_path(stage, "release-manifest.json.sigstore.json")?;
    fetch(
        &client,
        &format!("{base}/release-manifest.json.sigstore.json"),
        &manifest_sig,
        EVIDENCE_LIMIT,
    )
    .await?;
    verify_blob(stage, "release-manifest.json", request)?;
    let manifest: ReleaseManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    anyhow::ensure!(
        manifest.schema_version == 1,
        "unsupported release manifest schema"
    );
    anyhow::ensure!(
        manifest.repository == request.repository,
        "signed manifest repository differs"
    );
    anyhow::ensure!(manifest.tag == request.tag, "signed manifest tag differs");
    anyhow::ensure!(
        manifest.commit == request.commit,
        "signed manifest source commit differs"
    );
    anyhow::ensure!(
        request.tag == format!("v{}", manifest.version)
            || request.tag.starts_with(&format!("v{}-", manifest.version)),
        "manifest version does not match release tag"
    );
    anyhow::ensure!(
        manifest.source
            == format!(
                "git+https://github.com/{}?rev={}",
                request.repository, request.commit
            ),
        "signed manifest source differs"
    );
    anyhow::ensure!(
        manifest.signing.files == "sigstore-keyless-cosign"
            && manifest.signing.oci == "cosign-by-digest"
            && manifest.signing.android == "explicit-upload-key",
        "manifest does not declare the expected signing methods"
    );
    let (cpu, system) = architecture();
    anyhow::ensure!(
        !system.is_empty() && manifest.systems.iter().any(|s| s == system),
        "manifest has no host system"
    );
    let short = request
        .commit
        .get(..7)
        .context("source commit is too short")?;
    let name = format!("veles-{}-{short}-linux-{cpu}.tar.zst", manifest.version);
    let matching = manifest
        .artifacts
        .iter()
        .filter(|a| a.name == name)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matching.len() == 1,
        "manifest does not select exactly one host archive"
    );
    let artifact = matching[0];
    anyhow::ensure!(artifact.kind == "archive" && crate::valid_hash(&artifact.sha256));
    anyhow::ensure!(
        artifact.size > 0 && artifact.size <= ARCHIVE_LIMIT,
        "archive size is invalid"
    );
    let archive = stage_path(stage, &artifact.name)?;
    let archive_sig = stage_path(stage, &format!("{}.sigstore.json", artifact.name))?;
    fetch(
        &client,
        &format!("{base}/{}", artifact.name),
        &archive,
        artifact.size,
    )
    .await?;
    fetch(
        &client,
        &format!("{base}/{}.sigstore.json", artifact.name),
        &archive_sig,
        EVIDENCE_LIMIT,
    )
    .await?;
    verify_blob(stage, &artifact.name, request)?;
    let actual_hash = hash(&archive)?;
    anyhow::ensure!(
        actual_hash == artifact.sha256,
        "archive bytes differ from the signed manifest"
    );
    anyhow::ensure!(
        std::fs::metadata(&archive)?.len() == artifact.size,
        "archive length differs from the signed manifest"
    );
    Ok(Downloaded {
        manifest_version: manifest.version,
        manifest_sha256: hash(&manifest_path)?,
        archive_name: artifact.name.clone(),
        archive_sha256: actual_hash,
        archive,
    })
}

/// A verified archive and its exact unpacked preview. Staging belongs to
/// the plan: dropping it never leaves a reviewed-looking local source.
#[derive(Debug)]
pub struct SignedPlan {
    pub download: SignedDownload,
    pub bundle: BundlePlan,
    request: ReleaseRequest,
    archive_name: String,
    stage: PathBuf,
    pub(crate) destination_root: PathBuf,
}

impl Drop for SignedPlan {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.stage) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "signed-download staging cleanup failed for {}: {error}",
                self.stage.display()
            ),
        }
    }
}

impl SignedPlan {
    pub fn review(&self) -> serde_json::Value {
        let mut review = self.bundle.review();
        review["action"] = serde_json::Value::String("install-signed-bundle".into());
        review["download"] = serde_json::to_value(&self.download).unwrap();
        review["verification"] = serde_json::Value::String(self.download.verification.clone());
        review["actions"] = serde_json::json!([
            format!("install verified archive {}", self.download.archive_name),
            "switch current atomically and retain the previous payload",
        ]);
        review["unsupported"] =
            serde_json::json!(["core selection changes require the separate restart step",]);
        review
    }

    pub fn install(
        self,
        root: &Path,
        record: &mut InstallRecord,
    ) -> Result<(Component, SignedDownload)> {
        // The signature covers the archive's bytes, not its pathname. Bind
        // those exact bytes once more at the install boundary so a staged
        // replacement cannot inherit the earlier verification result.
        let archive = self.stage.join(&self.archive_name);
        verify_blob(&self.stage, &self.archive_name, &self.request)?;
        anyhow::ensure!(
            hash(&archive)? == self.download.archive_sha256,
            "archive changed after signed download; refusing installation"
        );
        let component = crate::bundle::install_bundle_archive_reviewed(
            root,
            &archive,
            ArtifactTrust::SignatureVerified,
            Some(&self.bundle.plan_sig),
            record,
        )?;
        let version = component.version.clone();
        let mut download = self.download.clone();
        download.version = version;
        Ok((component, download))
    }
}

/// Verify before confirmation. The returned plan retains the exact
/// downloaded archive and the identities used to verify it.
pub fn preview(request: &ReleaseRequest, root: &Path, stage: &Path) -> Result<SignedPlan> {
    request.validate()?;
    let parent = stage.parent().context("signed staging needs a parent")?;
    std::fs::create_dir_all(parent)?;
    let meta = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        meta.file_type().is_dir() && !meta.file_type().is_symlink(),
        "signed staging parent must be a directory, not a symlink"
    );
    veles_core::platform::private_dir(parent)?;
    #[cfg(unix)]
    for ancestor in parent.ancestors().skip(1).take(2) {
        let meta = std::fs::symlink_metadata(ancestor)
            .with_context(|| format!("checking signed staging parent {}", ancestor.display()))?;
        anyhow::ensure!(
            meta.file_type().is_dir() && !meta.file_type().is_symlink(),
            "signed staging ancestors must be directories, not symlinks"
        );
    }
    anyhow::ensure!(
        !stage.exists(),
        "signed staging already exists: {}",
        stage.display()
    );
    std::fs::create_dir(stage)?;
    veles_core::platform::private_dir(stage)?;
    let result = (|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let downloaded = runtime.block_on(download(request, stage))?;
        let unpack = stage.join("preview-tree");
        crate::bundle::unpack_archive(&downloaded.archive, &unpack)?;
        let bundle = crate::review_bundle(&unpack, root)?;
        anyhow::ensure!(
            bundle.info.version == downloaded.manifest_version
                && bundle.info.revision == request.commit[..7],
            "bundle metadata differs from signed release identity"
        );
        Ok((downloaded, bundle))
    })();
    let (downloaded, bundle) = match result {
        Ok(value) => value,
        Err(error) => {
            std::fs::remove_dir_all(stage)
                .with_context(|| format!("cleaning failed signed staging after: {error:#}"))?;
            return Err(error);
        }
    };
    let download = SignedDownload {
        archive_name: downloaded.archive_name.clone(),
        archive_sha256: downloaded.archive_sha256,
        manifest_sha256: downloaded.manifest_sha256,
        version: bundle.info.version.clone(),
        revision: bundle.info.revision.clone(),
        repository: request.repository.clone(),
        tag: request.tag.clone(),
        commit: request.commit.clone(),
        workflow_sha: request.workflow_sha.clone(),
        verification: "cosign verified manifest and archive under the named GitHub tag workflow identity; exact archive SHA-256 matched".into(),
    };
    Ok(SignedPlan {
        download,
        bundle,
        request: request.clone(),
        archive_name: downloaded.archive_name,
        stage: stage.to_path_buf(),
        destination_root: root.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ReleaseRequest {
        ReleaseRequest {
            release_url: "https://github.com/77787777778777/veles/releases/download/v1.0.0/".into(),
            repository: "77787777778777/veles".into(),
            tag: "v1.0.0".into(),
            commit: "abcdef1234567890abcdef1234567890abcdef12".into(),
            workflow_sha: "1234567890abcdef1234567890abcdef12345678".into(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_apply_plan_cannot_bless_replaced_staged_archive_bytes() {
        let cosign = cosign_program();
        assert!(
            cosign.is_file(),
            "real cosign test dependency is missing: {}",
            cosign.display()
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("install");
        let stage = temp.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let info = crate::BundleInfo {
            version: "1.0.0".into(),
            revision: "abcdef1".into(),
            architecture: "x86_64".into(),
        };
        let bundle = crate::BundlePlan {
            plan_sig: "reviewed-tree".into(),
            info,
            actions: vec![],
            unsupported: vec![],
        };
        let archive_name = "veles-1.0.0-abcdef1-linux-x86_64.tar.zst";
        std::fs::write(stage.join(archive_name), b"replaced after preview\n").unwrap();
        std::fs::write(
            stage.join(format!("{archive_name}.sigstore.json")),
            b"not a valid bundle\n",
        )
        .unwrap();
        let plan = SignedPlan {
            download: SignedDownload {
                archive_name: archive_name.into(),
                archive_sha256: crate::sha256(b"originally reviewed bytes"),
                manifest_sha256: crate::sha256(b"manifest"),
                version: "1.0.0".into(),
                revision: "abcdef1".into(),
                repository: request().repository,
                tag: request().tag,
                commit: request().commit,
                workflow_sha: request().workflow_sha,
                verification: "fixture".into(),
            },
            bundle,
            request: request(),
            archive_name: archive_name.into(),
            stage: stage.clone(),
            destination_root: root.clone(),
        };
        let mut record = InstallRecord {
            schema: 1,
            profile: crate::Profile::Workstation,
            components: vec![],
            history: vec![],
            recovery: None,
        };
        let error = plan.install(&root, &mut record).unwrap_err();
        assert!(
            format!("{error:#}").contains("cosign rejected"),
            "{error:#}"
        );
        assert!(!root.join("current").exists());
        assert!(record.components.is_empty());
        assert!(!stage.exists(), "the failed plan did not clean its staging");
    }

    #[cfg(unix)]
    #[test]
    fn signed_staging_refuses_symlinked_install_sources() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("actual");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("install-sources")).unwrap();
        let error = preview(
            &request(),
            &temp.path().join("install"),
            &temp
                .path()
                .join("install-sources/.signed-download-staging/plan"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("signed staging ancestors must be directories, not symlinks"),
            "{error:#}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn signed_bootstrap_refuses_non_linux_hosts() {
        assert!(request().validate().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn release_identity_and_archive_names_are_not_taken_from_arbitrary_urls() {
        let request = ReleaseRequest {
            release_url: "https://github.com/77787777778777/veles/releases/download/v1.0.0/".into(),
            repository: "77787777778777/veles".into(),
            tag: "v1.0.0".into(),
            commit: "abcdef1234567890abcdef1234567890abcdef12".into(),
            workflow_sha: "1234567890abcdef1234567890abcdef12345678".into(),
        };
        request.validate().unwrap();
        for url in [
            "http://github.com/77787777778777/veles/releases/download/v1.0.0/",
            "https://evil.example/77787777778777/veles/releases/download/v1.0.0/",
            "https://github.com/77787777778777/veles/releases/download/v2.0.0/",
        ] {
            let mut bad = request.clone();
            bad.release_url = url.into();
            assert!(bad.validate().is_err());
        }
        assert!(stage_path(Path::new("/tmp"), "../escape").is_err());
    }
}
