use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Packaged builds select the revision through the flake's environment.
    // A local checkout is not a release merely because `git rev-parse`
    // answers: any modified tracked file makes the exact source unknown.
    println!("cargo:rerun-if-env-changed=VELES_SOURCE_REVISION");
    if std::env::var_os("VELES_SOURCE_REVISION").is_some() {
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .ok()
            .filter(|result| result.status.success())
            .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_owned())
    };
    let revision = output(&["rev-parse", "HEAD"])
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
    // --no-optional-locks: a plain status may rewrite the index to
    // refresh its stat cache, and the index is watched below, so that
    // write alone would rerun this script on the next build.
    let clean = output(&[
        "--no-optional-locks",
        "status",
        "--porcelain",
        "--untracked-files=no",
    ])
    .is_some_and(|status| status.is_empty());
    let value = match (revision, clean) {
        (Some(sha), true) => sha,
        _ => "unreported-dirty-build".to_string(),
    };
    println!("cargo:rustc-env=VELES_SOURCE_REVISION={value}");

    // Once a build script prints any rerun-if line, cargo stops rerunning
    // it on package changes, so the env watch above alone stamped the
    // FIRST build's answer forever: a clean build at X kept reporting X
    // after later edits and commits. Rerun when the answer can change:
    // a commit, checkout or reset (HEAD, its reflog, the branch ref),
    // staging (the index), and any edit to a tracked top-level entry
    // (a directory is scanned recursively). Untracked paths such as
    // target/ are not watched, so a build does not invalidate itself.
    let git_path = |name: &str| output(&["rev-parse", "--git-path", name]).map(|p| root.join(p));
    let mut watched: Vec<PathBuf> = ["HEAD", "index", "packed-refs", "logs/HEAD"]
        .iter()
        .filter_map(|name| git_path(name))
        .collect();
    if let Some(branch) = output(&["symbolic-ref", "-q", "HEAD"]) {
        // A packed ref has no loose file until the next commit writes
        // one; watching its directory catches that creation.
        if let Some(reference) = git_path(&branch) {
            if reference.exists() {
                watched.push(reference);
            } else {
                watched.extend(reference.parent().map(Path::to_path_buf));
            }
        }
    }
    if let Some(entries) = output(&["ls-tree", "--name-only", "HEAD"]) {
        watched.extend(entries.lines().map(|name| root.join(name)));
    }
    // A path that does not exist reads as changed on every build.
    for path in watched.into_iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
