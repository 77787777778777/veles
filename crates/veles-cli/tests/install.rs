//! Exercise the actual installer command in an isolated process home.
//! No live gateway, network, model, user config, or installation is used.

use assert_cmd::Command;

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("veles").unwrap();
    command
        .env("VELES_HOME", home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env_remove("VELES_MODULE_PATH")
        .env_remove("VELES_PLUGIN_PATH")
        .env_remove("VELES_TEST_COSIGN")
        .env_remove("DESKTOP_BEARER_TOKEN");
    command
}

fn process_root(home: &std::path::Path) -> std::path::PathBuf {
    home.join("process-plugins/echo")
}

fn fixture(home: &std::path::Path) {
    let root = process_root(home);
    std::fs::create_dir_all(&root).unwrap();
    // Use the real cross-platform process plugin, not a Unix shell
    // fixture that cannot be spawned by native Windows.
    std::fs::copy(
        veles_core::testenv::process_plugin_echo(),
        root.join("plugin"),
    )
    .unwrap();
    std::fs::write(
        root.join("plugin.toml"),
        "name = \"echo\"\nkind = \"process\"\nentry = \"plugin\"\n",
    )
    .unwrap();
}

#[test]
fn configure_refusal_writes_nothing_and_yes_applies_the_reviewed_plan() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    fixture(home);
    let answers = home.join("answers.json");
    std::fs::write(&answers, r#"{"platforms.echo.rooms.fail_start":true}"#).unwrap();

    command(home)
        .args(["install", "configure", "echo", "--answers"])
        .arg(&answers)
        .write_stdin("no\n")
        .assert()
        .success();
    assert!(
        !home.join("config.toml").exists(),
        "preview without --yes wrote a config"
    );
    assert!(
        !home.join("install.toml").exists(),
        "refusal recorded an install"
    );

    let config_text = "# preserve this comment\n[agent]\nuser_agent = \"unchanged\"\n";
    std::fs::write(home.join("config.toml"), config_text).unwrap();
    command(home)
        .args(["install", "configure", "echo", "--answers"])
        .arg(&answers)
        .arg("--yes")
        .assert()
        .success();
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(config.contains("preserve this comment"), "{config}");
    assert!(config.contains("user_agent = \"unchanged\""), "{config}");
    assert!(config.contains("fail_start = true"), "{config}");
    assert!(home.join("install.toml").exists());

    // The reviewed component path also binds its bytes at apply. This
    // fixture is deliberately discovered only through the managed root,
    // not the caller's process plugin search path.
    let manifest = home.join("process-plugins/echo/plugin.toml");
    let before = std::fs::read(&manifest).unwrap();
    command(home)
        .args(["install", "component", "echo"])
        .write_stdin("no\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cancelled"));
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
}

#[cfg(unix)]
use veles_install::test_support::{
    fixture as signed_bundle_fixture, fixture_with_info as signed_bundle_fixture_with_info,
};

#[cfg(unix)]
fn assert_no_signed_staging(home: &std::path::Path) {
    let root = home.join("install-sources/.signed-download-staging");
    assert!(
        !root.exists() || std::fs::read_dir(root).unwrap().next().is_none(),
        "the staging parent may remain, but no downloaded archive or plan may remain"
    );
}

#[cfg(unix)]
#[test]
fn a_signed_download_is_verified_before_the_existing_bundle_install() {
    let temp = tempfile::tempdir().unwrap();
    let (release, socket) = signed_bundle_fixture(temp.path(), false, false);
    command(temp.path())
        .env("PATH", temp.path())
        .env("VELES_TEST_RELEASE_UNIX_SOCKET", socket)
        .args([
            "install",
            "download",
            "--release",
            &release,
            "--repository",
            "77787777778777/veles",
            "--tag",
            "v1.0.0",
            "--commit",
            "abcdef1234567890abcdef1234567890abcdef12",
            "--workflow-sha",
            "1234567890abcdef1234567890abcdef12345678",
            "--yes",
        ])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("install/current/bin/veles")).unwrap(),
        "signed fixture\n"
    );
    command(temp.path())
        .args(["install", "verify"])
        .assert()
        .success();
    let calls = std::fs::read_to_string(temp.path().join("cosign-calls")).unwrap();
    assert_eq!(calls.lines().count(), 3, "{calls}");
    assert!(calls.contains("--certificate-github-workflow-ref refs/tags/v1.0.0"));
    assert!(calls
        .contains("--certificate-github-workflow-sha 1234567890abcdef1234567890abcdef12345678"));
    assert_no_signed_staging(temp.path());
    command(temp.path())
        .args(["install", "download", "--release", &release, "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "explicit repository, tag, source commit and workflow SHA",
        ));
}

#[cfg(unix)]
#[test]
fn a_signed_archive_for_a_different_version_is_not_selected() {
    let temp = tempfile::tempdir().unwrap();
    let (release, socket) =
        signed_bundle_fixture_with_info(temp.path(), false, false, "2.0.0", "2.0.0", "abcdef1");
    command(temp.path())
        .env("PATH", temp.path())
        .env("VELES_TEST_RELEASE_UNIX_SOCKET", socket)
        .args([
            "install",
            "download",
            "--release",
            &release,
            "--repository",
            "77787777778777/veles",
            "--tag",
            "v1.0.0",
            "--commit",
            "abcdef1234567890abcdef1234567890abcdef12",
            "--workflow-sha",
            "1234567890abcdef1234567890abcdef12345678",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "manifest version does not match release tag",
        ));
    assert!(!temp.path().join("install/current").exists());
    assert!(!temp.path().join("install.toml").exists());
}

#[cfg(unix)]
#[test]
fn embedded_bundle_identity_must_match_the_signed_manifest_and_source() {
    let temp = tempfile::tempdir().unwrap();
    let (release, socket) =
        signed_bundle_fixture_with_info(temp.path(), false, false, "1.0.0", "2.0.0", "0000000");
    command(temp.path())
        .env("PATH", temp.path())
        .env("VELES_TEST_RELEASE_UNIX_SOCKET", socket)
        .args([
            "install",
            "download",
            "--release",
            &release,
            "--repository",
            "77787777778777/veles",
            "--tag",
            "v1.0.0",
            "--commit",
            "abcdef1234567890abcdef1234567890abcdef12",
            "--workflow-sha",
            "1234567890abcdef1234567890abcdef12345678",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "bundle metadata differs from signed release identity",
        ));
    assert!(!temp.path().join("install/current").exists());
    assert!(!temp.path().join("install.toml").exists());
    assert_no_signed_staging(temp.path());
}

#[cfg(unix)]
#[test]
fn a_rejected_archive_cannot_be_installed_or_left_as_a_reviewed_source() {
    let temp = tempfile::tempdir().unwrap();
    let cosign = std::env::var_os("VELES_TEST_COSIGN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .into_iter()
                .flatten()
                .map(|dir| dir.join("cosign"))
                .find(|candidate| candidate.is_file())
        })
        .expect("the dev shell must provide the real cosign test dependency");
    let (release, socket) = signed_bundle_fixture(temp.path(), true, false);
    let bindir = temp.path().join("real-cosign-bin");
    std::fs::create_dir(&bindir).unwrap();
    std::os::unix::fs::symlink(&cosign, bindir.join("cosign")).unwrap();
    command(temp.path())
        .env("PATH", &bindir)
        .env("VELES_TEST_COSIGN", &cosign)
        .env("VELES_TEST_RELEASE_UNIX_SOCKET", socket)
        .args([
            "install",
            "download",
            "--release",
            &release,
            "--repository",
            "77787777778777/veles",
            "--tag",
            "v1.0.0",
            "--commit",
            "abcdef1234567890abcdef1234567890abcdef12",
            "--workflow-sha",
            "1234567890abcdef1234567890abcdef12345678",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cosign rejected"));
    assert!(!temp.path().join("install/current").exists());
    assert!(!temp.path().join("install.toml").exists());
    assert_no_signed_staging(temp.path());
    // The lock file is a persistent mutex anchor, not an installation
    // or downloaded artifact. It may exist after the rejected operation.
}

#[cfg(unix)]
#[test]
fn declining_the_verified_preview_leaves_no_install_or_staged_download() {
    let temp = tempfile::tempdir().unwrap();
    let (release, socket) = signed_bundle_fixture(temp.path(), false, false);
    command(temp.path())
        .env("PATH", temp.path())
        .env("VELES_TEST_RELEASE_UNIX_SOCKET", socket)
        .args([
            "install",
            "download",
            "--release",
            &release,
            "--repository",
            "77787777778777/veles",
            "--tag",
            "v1.0.0",
            "--commit",
            "abcdef1234567890abcdef1234567890abcdef12",
            "--workflow-sha",
            "1234567890abcdef1234567890abcdef12345678",
        ])
        .write_stdin("no\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cancelled"));
    assert!(!temp.path().join("install/current").exists());
    assert!(!temp.path().join("install.toml").exists());
    assert_no_signed_staging(temp.path());
    let calls = std::fs::read_to_string(temp.path().join("cosign-calls")).unwrap();
    assert_eq!(
        calls.lines().count(),
        2,
        "declined plans are never applied: {calls}"
    );
}

#[cfg(unix)]
#[test]
fn a_digest_mismatch_is_refused_even_when_the_signature_stub_accepts() {
    let temp = tempfile::tempdir().unwrap();
    let (release, socket) = signed_bundle_fixture(temp.path(), true, false);
    let cosign = temp.path().join("cosign");
    std::fs::write(
        &cosign,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> cosign-calls\n[ \"$1\" = verify-blob ]\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&cosign, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    command(temp.path())
        .env("PATH", temp.path())
        .env("VELES_TEST_RELEASE_UNIX_SOCKET", socket)
        .current_dir(temp.path())
        .args([
            "install",
            "download",
            "--release",
            &release,
            "--repository",
            "77787777778777/veles",
            "--tag",
            "v1.0.0",
            "--commit",
            "abcdef1234567890abcdef1234567890abcdef12",
            "--workflow-sha",
            "1234567890abcdef1234567890abcdef12345678",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "archive bytes differ from the signed manifest",
        ));
    assert!(!temp.path().join("install/current").exists());
    assert!(!temp.path().join("install.toml").exists());
    assert_no_signed_staging(temp.path());
}

#[test]
fn install_state_and_verify_are_real_cli_commands_not_api_only() {
    let temp = tempfile::tempdir().unwrap();
    command(temp.path())
        .args(["install", "state"])
        .assert()
        .success();
    command(temp.path())
        .args(["install", "verify"])
        .assert()
        .success();
    command(temp.path())
        .args(["install", "rollback", "echo", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not installed"));
}
