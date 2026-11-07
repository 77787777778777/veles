//! The source revision a development build reports (/api/health, which
//! fleet deploy preflight and verify read) must follow the checkout it
//! was built from. build.rs only watched VELES_SOURCE_REVISION, and once
//! a build script prints a rerun-if line cargo stops rerunning it on
//! package changes: a clean build at X kept stamping X after later edits
//! and commits. This drives the real build.rs through cargo in a scratch
//! repository: stamp, edit, rebuild, commit, rebuild.

use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=probe",
            "-c",
            "user.email=probe@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Build and run the probe; it prints the revision its build stamped.
fn stamped(root: &Path, scratch: &Path) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["run", "--quiet", "--offline", "--manifest-path"])
        .arg(root.join("crates/probe/Cargo.toml"))
        .env("CARGO_TARGET_DIR", scratch.join("target"))
        .env("CARGO_HOME", scratch.join("cargo-home"))
        // The nix test derivation exports the flake's revision; the
        // probe is the local-checkout path, which never sees it.
        .env_remove("VELES_SOURCE_REVISION")
        .output()
        .expect("cargo runs");
    assert!(out.status.success(), "probe build: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn a_development_build_restamps_its_revision_after_edits_and_commits() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("repo");
    let probe = root.join("crates/probe");
    std::fs::create_dir_all(probe.join("src")).unwrap();
    std::fs::write(
        probe.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(probe.join("build.rs"), include_str!("../build.rs")).unwrap();
    let main = "fn main() { print!(\"{}\", env!(\"VELES_SOURCE_REVISION\")); }\n";
    std::fs::write(probe.join("src/main.rs"), main).unwrap();
    std::fs::write(root.join(".gitignore"), "/target\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "first"]);
    let first = git(&root, &["rev-parse", "HEAD"]);
    assert_eq!(
        stamped(&root, scratch.path()),
        first,
        "a clean build is exact"
    );

    // An edit after that clean build: the binary is no longer the
    // commit, and must not claim to be.
    std::fs::write(probe.join("src/main.rs"), format!("// edited\n{main}")).unwrap();
    assert_eq!(
        stamped(&root, scratch.path()),
        "unreported-dirty-build",
        "an edited tree is not the clean commit it was first built at"
    );

    // Committing the edit makes the tree clean again, at a NEW commit.
    git(&root, &["commit", "-q", "-am", "second"]);
    let second = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(first, second);
    assert_eq!(
        stamped(&root, scratch.path()),
        second,
        "the new commit, not the first"
    );
}
