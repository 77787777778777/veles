//! Extension contribution workflow: review, prepare, publish, then PR.
//!
//! Every step is a separate UI action. `prepare` clones into a new private
//! worktree and commits only the selected files; it never pushes. `publish`
//! performs one non-force push of an existing prepared branch. `pr` requires
//! that push to have completed and invokes `gh` as a separate action.
//!
//! One contribution may carry several extensions. Each extension's files
//! land under `<directory>/<extension name>/` in the chosen repository,
//! never at its root, and a path that already exists there is replaced
//! only after an explicit second confirmation.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

const MAX_FILES: usize = 100;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_BRANCH: &str = "veles-extension-contribution";

/// Repository directory contributed extensions go under, one
/// subdirectory per extension: the layout veles keeps its own Tcl
/// plugin sources in (`plugins/examples/<name>/`).
pub const DEFAULT_DIRECTORY: &str = "plugins/examples";

/// The veles upstream a pull request targets unless the author names
/// another `owner/repository`.
pub const DEFAULT_UPSTREAM_REPO: &str = "77787777778777/veles";

/// Compatibility entry point for the current desktop scaffold. New callers
/// should pass an explicit branch to [`prepare`].
pub fn prepare_export(root: &Path, selected: &[String], destination: &str) -> Result<Value> {
    prepare(root, selected, destination, DEFAULT_BRANCH)
}

/// One extension's part of a contribution: its validated name, its user
/// plugin directory, and the files selected inside it (relative to it).
pub struct Selection {
    pub name: String,
    pub root: PathBuf,
    pub files: Vec<String>,
}

impl Selection {
    /// The single-extension form: the name is the plugin directory's.
    pub fn single(root: &Path, files: &[String]) -> Result<Selection> {
        let name = root
            .file_name()
            .and_then(OsStr::to_str)
            .context("extension directory has no UTF-8 name")?;
        Ok(Selection {
            name: name.to_string(),
            root: root.to_path_buf(),
            files: files.to_vec(),
        })
    }
}

/// One selected source file and where it goes in the repository.
struct Item {
    extension: String,
    root: PathBuf,
    relative: String,
    /// Path inside the upstream repository: `<directory>/<name>/<relative>`.
    target: String,
    after: String,
}

fn validate_directory(directory: &str) -> Result<String> {
    let directory = directory.trim().trim_matches('/');
    if directory.is_empty() {
        bail!("choose a repository directory for the extensions, e.g. {DEFAULT_DIRECTORY}");
    }
    validate_relative(directory)
        .context("the contribution directory must be a plain relative repository path")?;
    Ok(directory.to_string())
}

fn read_items(selections: &[Selection], directory: &str) -> Result<(String, Vec<Item>)> {
    let directory = validate_directory(directory)?;
    if selections.is_empty() {
        bail!("select at least one extension");
    }
    let mut names = BTreeSet::new();
    let mut items = Vec::new();
    for selection in selections {
        let name = crate::plugins::validate_name(&selection.name)?;
        if !names.insert(name.clone()) {
            bail!("select each extension only once");
        }
        for file in read_selected(&selection.root, &selection.files)? {
            let target = format!("{directory}/{name}/{}", file.path);
            validate_relative(&target)?;
            items.push(Item {
                extension: name.clone(),
                root: selection.root.clone(),
                relative: file.path,
                target,
                after: file.after,
            });
        }
        if items.len() > MAX_FILES {
            bail!("too many files selected (maximum {MAX_FILES})");
        }
    }
    Ok((directory, items))
}

pub struct ReviewFile {
    pub path: String,
    /// `None` is a new, uncommitted addition, not an unchanged file.
    pub before: Option<String>,
    pub after: String,
}

fn validate_relative(relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || relative.contains('\\')
        || relative.contains('\0')
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("select a relative file inside the extension, without parent components");
    }
    let parts = path
        .components()
        .map(|part| match part {
            Component::Normal(part) => part
                .to_str()
                .map(str::to_string)
                .context("selected path is not valid UTF-8"),
            _ => unreachable!(),
        })
        .collect::<Result<Vec<_>>>()?;
    for (index, part) in parts.iter().enumerate() {
        let lower = part.to_ascii_lowercase();
        let last = index + 1 == parts.len();
        if part.starts_with('.')
            || lower == ".git"
            || lower == ".env"
            || lower == "secrets.env"
            || lower == "node.toml"
            || lower == "node_modules"
            || lower == "target"
            || lower == "dist"
            || lower == "build"
            || lower == "__pycache__"
            || (!last && matches!(lower.as_str(), "out" | "coverage"))
        {
            bail!("selected path is local state, metadata, or credentials; source files only");
        }
        if last
            && (lower.ends_with(".env")
                || lower.ends_with(".env.local")
                || lower.ends_with(".pem")
                || lower.ends_with(".key")
                || lower.ends_with(".p12")
                || lower.ends_with(".pfx")
                || lower.ends_with(".keystore")
                || lower == "id_rsa"
                || lower == "id_dsa"
                || lower == "id_ecdsa"
                || lower == "id_ed25519"
                || lower == "credentials"
                || lower == "credentials.json"
                || lower == ".netrc"
                || lower.ends_with(".log")
                || lower.ends_with(".sqlite")
                || lower.ends_with(".sqlite3")
                || lower == "state.db")
        {
            bail!("selected file is credentials or local state, not publishable source");
        }
    }
    Ok(path.to_path_buf())
}

fn regular_file(root: &Path, relative: &str) -> Result<(PathBuf, std::fs::Metadata)> {
    let relative_path = validate_relative(relative)?;
    let metadata = root
        .symlink_metadata()
        .with_context(|| format!("inspect extension root {}", root.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("extension root must be a real directory, not a symlink");
    }
    let mut current = root.to_path_buf();
    for part in relative_path.components() {
        current.push(part);
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("inspect selected path {relative}"))?;
        if metadata.file_type().is_symlink() {
            bail!("selected path contains a symlink; publish regular source files only");
        }
    }
    let metadata = std::fs::symlink_metadata(&current)?;
    if !metadata.is_file() {
        bail!("selected path must be a regular UTF-8 source file");
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("selected source file exceeds the 2 MiB contribution limit");
    }
    Ok((current, metadata))
}

fn selected_path(root: &Path, relative: &str) -> Result<PathBuf> {
    regular_file(root, relative).map(|(path, _)| path)
}

fn file_review(root: &Path, relative: &str) -> Result<ReviewFile> {
    let full = selected_path(root, relative)?;
    let after = std::fs::read_to_string(&full)
        .with_context(|| format!("read {relative} as UTF-8 source"))?;
    // The selected extension is a working tree, not the chosen upstream.
    // Never invent a before image by reading `after` twice. The actual
    // upstream base is computed after prepare clones its destination.
    Ok(ReviewFile {
        path: relative.to_string(),
        before: None,
        after,
    })
}

fn read_selected(root: &Path, selected: &[String]) -> Result<Vec<ReviewFile>> {
    if selected.is_empty() {
        bail!("select at least one source file");
    }
    if selected.len() > MAX_FILES {
        bail!("too many files selected (maximum {MAX_FILES})");
    }
    let mut seen = BTreeSet::new();
    selected
        .iter()
        .map(|path| {
            if !seen.insert(path.clone()) {
                bail!("select each source file only once");
            }
            file_review(root, path)
        })
        .collect()
}

fn unified_diff(file: &ReviewFile) -> String {
    let before = file.before.as_deref().unwrap_or("");
    if before == file.after && file.before.is_some() {
        return String::new();
    }
    let old_label = if file.before.is_some() {
        format!("a/{}", file.path)
    } else {
        "/dev/null".to_string()
    };
    similar::TextDiff::from_lines(before, &file.after)
        .unified_diff()
        .context_radius(3)
        .header(&old_label, &format!("b/{}", file.path))
        .to_string()
}

fn registered_secrets() -> HashMap<String, String> {
    crate::secrets::scrubbable()
        .into_iter()
        .filter(|(_, value)| value.len() >= 4)
        .collect()
}

fn secret_findings(text: &str, secrets: &HashMap<String, String>) -> Vec<String> {
    let mut kinds = BTreeSet::new();
    if secrets.values().any(|value| text.contains(value)) {
        kinds.insert("registered credential value");
    }
    if crate::a2a::secret_shaped(text) {
        kinds.insert("credential-shaped value");
    }
    kinds.into_iter().map(str::to_string).collect()
}

/// The contribution secret scan for any other outbound text (AI assist):
/// the kinds found, never the values.
pub(crate) fn outbound_secret_findings(text: &str) -> Vec<String> {
    secret_findings(text, &registered_secrets())
}

/// The contribution path denylist (credentials, keys, local state, build
/// output, hidden files) for a path relative to an extension directory.
pub(crate) fn publishable_relative(relative: &str) -> Result<()> {
    validate_relative(relative).map(|_| ())
}

fn sanitize(text: &str, secrets: &HashMap<String, String>) -> String {
    crate::a2a::redact_outbound(&crate::secrets::scrub(text, secrets))
}

fn check_command_available_in(path: &std::ffi::OsStr, program: &str, os: &str) -> bool {
    let candidates = if os == "windows" {
        // Match the platform's PATHEXT ladder without its cfg: this helper
        // also validates Windows lookup on the Linux development host.
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut names = extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("{program}{}", extension.to_ascii_lowercase()))
            .collect::<Vec<_>>();
        names.push(program.to_string());
        names
    } else {
        vec![program.to_string()]
    };
    std::env::split_paths(path).any(|dir| {
        candidates.iter().any(|candidate| {
            let candidate = dir.join(candidate);
            candidate.is_file() && !candidate.symlink_metadata().is_ok_and(|m| m.is_dir())
        })
    })
}

fn check_command_available(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| check_command_available_in(&path, program, crate::platform::OS))
}

fn checks(items: &[Item]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let suffix = Path::new(&item.relative)
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or("");
            let (program, method, note) = match suffix {
                "tcl" => (
                    "tclsh",
                    "Tcl info complete",
                    "checks command completeness without evaluating commands",
                ),
                "js" => (
                    "node",
                    "node --check",
                    "JavaScript syntax only; the module is not loaded",
                ),
                "ts" => (
                    "tsc",
                    "tsc --noEmit",
                    "TypeScript compiler syntax check; no emitted output",
                ),
                "rkt" => (
                    "",
                    "unavailable",
                    "no non-evaluating Racket parser is configured; manual review required",
                ),
                "lisp" | "lsp" | "cl" | "el" => (
                    "",
                    "unavailable",
                    "no non-evaluating Lisp reader is configured; manual review required",
                ),
                _ => ("", "manual", "no non-evaluating checker is registered"),
            };
            json!({
                "extension": item.extension,
                "path": item.relative,
                "destination": item.target,
                "operation": "syntax",
                "program": program,
                "method": method,
                "available": !program.is_empty() && check_command_available(program),
                "note": note,
                "root": item.root.display().to_string(),
            })
        })
        .collect()
}

/// Review exactly the selected regular UTF-8 source files of one
/// extension, at the default repository directory.
pub fn review(root: &Path, selected: &[String]) -> Result<Value> {
    review_many(&[Selection::single(root, selected)?], DEFAULT_DIRECTORY)
}

/// Review the selected files of one or more extensions. Findings always
/// block preparation. A finding returns only a redacted preview; no error or
/// JSON field contains the source text that triggered it. Every row and the
/// diff name the repository path the file would be committed at.
pub fn review_many(selections: &[Selection], directory: &str) -> Result<Value> {
    let (directory, items) = read_items(selections, directory)?;
    let secrets = registered_secrets();
    let mut blocked = false;
    let mut preview = String::new();
    let mut rows = Vec::new();
    for item in &items {
        let findings = secret_findings(&item.after, &secrets);
        blocked |= !findings.is_empty();
        let diff = sanitize(
            &unified_diff(&ReviewFile {
                path: item.target.clone(),
                before: None,
                after: item.after.clone(),
            }),
            &secrets,
        );
        preview.push_str(&diff);
        rows.push(json!({
            "extension": item.extension,
            "path": item.relative,
            "destination": item.target,
            "bytes": item.after.len(),
            "before": "uncommitted-addition",
            "blocked": !findings.is_empty(),
            "findings": findings,
        }));
    }
    let extensions = items
        .iter()
        .map(|item| item.extension.as_str())
        .collect::<BTreeSet<_>>();
    Ok(json!({
        "operation": "review",
        "files": rows,
        "extensions": extensions,
        "directory": directory,
        "diff": preview,
        "redacted": blocked,
        "diff_base": "The selected extension files are uncommitted additions, each under its own extension directory. The before side is /dev/null; the actual upstream diff, including any path that already exists there, is available after prepare.",
        "checks": checks(&items),
        "findings": if blocked {
            vec!["registered or credential-shaped content was redacted; replace or exclude it before preparation"]
        } else {
            vec!["no registered or recognized credential-shaped value matched; manual source review is still required"]
        },
        "prepare_allowed": !blocked,
        "publish_allowed": false,
        "next": "run checks, then explicitly prepare an isolated branch and commit; publish and PR are separate actions",
    }))
}

fn output_text(output: &Output, secrets: &HashMap<String, String>) -> Value {
    let stdout = sanitize(&String::from_utf8_lossy(&output.stdout), secrets);
    let stderr = sanitize(&String::from_utf8_lossy(&output.stderr), secrets);
    let mut text = String::new();
    if !stdout.trim().is_empty() {
        text.push_str(stdout.trim_end());
        text.push('\n');
    }
    if !stderr.trim().is_empty() {
        text.push_str(stderr.trim_end());
    }
    const LIMIT: usize = 16 * 1024;
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[checker output truncated]");
    }
    json!(text)
}

fn command_output(command: &mut Command, description: &str) -> Result<Output> {
    command
        .output()
        .with_context(|| format!("{description}; required program could not be started"))
}

/// Run non-evaluating syntax checks only. Tcl uses `info complete`; JS uses
/// `node --check`. TypeScript is checked by `tsc` only, never passed to the
/// JavaScript checker. Racket and Lisp report honest unavailable status.
pub fn run_checks(root: &Path, selected: &[String]) -> Result<Value> {
    run_checks_many(&[Selection::single(root, selected)?], DEFAULT_DIRECTORY)
}

/// [`run_checks`] over the selected files of one or more extensions.
pub fn run_checks_many(selections: &[Selection], directory: &str) -> Result<Value> {
    let (_, items) = read_items(selections, directory)?;
    let secrets = registered_secrets();
    let mut results = Vec::new();
    for item in &items {
        let full = selected_path(&item.root, &item.relative)?;
        let mut row = check_one(&full, &item.relative, &secrets)?;
        row["extension"] = json!(item.extension);
        row["destination"] = json!(item.target);
        results.push(row);
    }
    Ok(checks_report(results))
}

/// The Tcl completeness checker, run as a script FILE. `tclsh -c` is not
/// a tclsh option: tclsh took it as an argument, read its closed stdin and
/// exited 0, so every Tcl file "passed" a check that never ran
/// (dev-ext-1). This reads the file and never evaluates it.
const TCL_COMPLETE_CHECK: &str = "set f [open [lindex $argv 0] r]
fconfigure $f -encoding utf-8
set text [read $f]
close $f
if {![info complete $text]} {
    puts stderr {incomplete Tcl: an unclosed brace, bracket or quote}
    exit 1
}
exit 0
";

/// The lang-js host's API object for the TypeScript check, typed
/// (`plugins/veles.d.ts`); its contract is docs/plugins.md ("JavaScript /
/// TypeScript").
const TS_HOST_DECLARATION: &str = crate::plugins::VELES_TS_DECLARATION;

/// Run one non-evaluating syntax check. `label` is the path reported.
fn check_one(full: &Path, label: &str, secrets: &HashMap<String, String>) -> Result<Value> {
    let ext = full.extension().and_then(OsStr::to_str).unwrap_or("");
    #[cfg(test)]
    if ext == "js" {
        if let Some(fake) = std::env::var_os("VELES_TEST_CONTRIBUTE_NODE") {
            let output = command_output(
                Command::new(fake).arg("--check").arg(full),
                "test JavaScript syntax check",
            )?;
            return Ok(
                json!({"path": label, "status": if output.status.success() { "passed" } else { "failed" }}),
            );
        }
    }
    // Checker support files live until the checker has exited.
    let mut _scratch = None;
    let mut command = match ext {
        "tcl" => {
            if !check_command_available("tclsh") {
                return Ok(
                    json!({"path": label, "status": "unavailable", "note": "tclsh is not installed"}),
                );
            }
            let scratch = tempfile::tempdir().context("create the checker directory")?;
            let script = scratch.path().join("complete.tcl");
            std::fs::write(&script, TCL_COMPLETE_CHECK).context("write the Tcl checker script")?;
            let mut c = Command::new("tclsh");
            c.arg(&script).arg(full);
            _scratch = Some(scratch);
            c
        }
        "js" => {
            if !check_command_available("node") {
                return Ok(
                    json!({"path": label, "status": "unavailable", "note": "node is not installed; JavaScript was not checked"}),
                );
            }
            let mut c = Command::new("node");
            c.arg("--check").arg(full);
            c
        }
        "ts" => {
            if !check_command_available("tsc") {
                return Ok(
                    json!({"path": label, "status": "unavailable", "note": "no TypeScript compiler is installed; TypeScript was not checked as JavaScript"}),
                );
            }
            // An extension is an ES module run by the lang-js host, which
            // provides the `veles` global. Without saying so, tsc rejected
            // every scaffolded plugin: TS2304 (no `veles`) and TS1375 (top-
            // level await outside a module) (dev-ext-11).
            let scratch = tempfile::tempdir().context("create the checker directory")?;
            let ambient = scratch.path().join("veles-host.d.ts");
            std::fs::write(&ambient, TS_HOST_DECLARATION)
                .context("write the TypeScript host declaration")?;
            let mut c = Command::new("tsc");
            c.args([
                "--noEmit",
                "--pretty",
                "false",
                "--skipLibCheck",
                "--target",
                "es2022",
                "--module",
                "esnext",
                "--moduleDetection",
                "force",
                // The host's globals only: the declaration's `console` and
                // timers would collide with the DOM library's.
                "--lib",
                "es2022",
            ]);
            // A selected `veles.d.ts` IS the declaration; adding it twice
            // is a redeclaration, not a check.
            if !label.ends_with(".d.ts") {
                c.arg(&ambient);
            }
            c.arg(full);
            _scratch = Some(scratch);
            c
        }
        "rkt" => {
            return Ok(
                json!({"path": label, "status": "unavailable", "note": "Racket checking is unavailable: no non-evaluating parser is configured"}),
            );
        }
        "lisp" | "lsp" | "cl" | "el" => {
            return Ok(
                json!({"path": label, "status": "unavailable", "note": "Lisp checking is unavailable: no non-evaluating reader is configured"}),
            );
        }
        _ => {
            return Ok(
                json!({"path": label, "status": "manual", "note": "no non-evaluating checker is registered for this file type"}),
            );
        }
    };
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null());
    let output = command_output(&mut command, &format!("run syntax check for {label}"))?;
    Ok(json!({
        "path": label,
        "status": if output.status.success() { "passed" } else { "failed" },
        "output": output_text(&output, secrets),
    }))
}

fn checks_report(results: Vec<Value>) -> Value {
    let passed = results.iter().all(|row| {
        matches!(
            row["status"].as_str(),
            Some("passed" | "manual" | "unavailable")
        )
    });
    json!({
        "operation": "checks",
        "checks": results,
        "passed": passed,
        "publish_allowed": false,
        "note": "unavailable and manual checks are not passes; they require manual review",
    })
}

/// Pre-selected in the contribution dialog: the manifest, source files in
/// a language a veles extension can be written in, their project files,
/// and a README. Everything else in an extension directory (state,
/// caches, notes, data) is offered unticked.
///
/// "Select all extensions" ticks exactly these, so a language missing
/// here was silently left out of "wrap all my extensions into a PR": a
/// Rust plugin crate (Cargo.toml, src/lib.rs, build.rs), a C plugin and
/// the TypeScript scaffold's tsconfig.json all were.
fn suggested_source(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "plugin.toml"
            | "readme.md"
            | "readme"
            | "cargo.toml"
            | "cargo.lock"
            | "tsconfig.json"
            | "jsconfig.json"
            | "package.json"
            | "info.rkt"
            | "makefile"
            | "cmakelists.txt"
            | "meson.build"
            | "compile_flags.txt"
    ) || lower.starts_with("license")
    {
        return true;
    }
    matches!(
        Path::new(&lower).extension().and_then(OsStr::to_str),
        Some(
            "tcl"
                | "tm"
                | "js"
                | "mjs"
                | "cjs"
                | "ts"
                | "mts"
                | "rkt"
                | "lisp"
                | "lsp"
                | "cl"
                | "asd"
                | "el"
                | "rs"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "hpp"
        )
    )
}

/// A path relative to an extension, as the repository spells it: `/`
/// between components. On Windows the native separator is `\`; anywhere
/// else a `\` is part of a file name (and `validate_relative` refuses it).
fn slash_relative(native: &str, os: &str) -> String {
    if os == "windows" {
        native
            .split(['\\', '/'])
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    } else {
        native.to_string()
    }
}

fn collect_candidates(root: &Path, directory: &Path, out: &mut Vec<Value>) -> Result<()> {
    let mut entries = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        name.to_str()
            .context("extension contains a non-UTF-8 filename; rename or exclude it")?;
        let full = entry.path();
        // Joined with '/' whatever the platform: a Windows `nested\x.tcl`
        // failed validate_relative and silently dropped every nested file
        // from the candidate list (dev-ext-13).
        let native = full
            .strip_prefix(root)
            .context("candidate escaped extension root")?
            .to_str()
            .context("candidate path is not UTF-8")?;
        let relative = slash_relative(native, crate::platform::OS);
        if validate_relative(&relative).is_err() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&full)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_candidates(root, &full, out)?;
        } else if metadata.is_file() && metadata.len() <= MAX_FILE_BYTES {
            // A binary file cannot be reviewed as text; say so up front
            // instead of letting it fail the whole review (dev-ext-12).
            let text = std::fs::read(&full).is_ok_and(|bytes| std::str::from_utf8(&bytes).is_ok());
            out.push(json!({
                "path": relative,
                "bytes": metadata.len(),
                "text": text,
                "suggested": text && suggested_source(&relative),
            }));
        }
    }
    Ok(())
}

/// Recursively enumerate regular source files. Hidden files, symlinks, git
/// metadata, credentials, dependency/build trees, and local state are excluded.
pub fn candidates(root: &Path) -> Result<Value> {
    let metadata = root.symlink_metadata()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("extension root must be a real directory, not a symlink");
    }
    let mut out = Vec::new();
    collect_candidates(root, root, &mut out)?;
    Ok(json!({"operation": "candidates", "files": out, "publish_allowed": false}))
}

fn git_command_on(worktree: Option<&Path>, os: &str) -> Command {
    let mut command = Command::new("git");
    let null = if os == "windows" { "NUL" } else { "/dev/null" };
    command
        // The caller's identity remains ambient; execution policy does not.
        // In particular, no repository/global hook, pager, or text rewrite is
        // allowed to turn a contribution mutation into arbitrary execution.
        // Credential helpers stay ambient on purpose: `credential.helper=`
        // reset the whole helper list, including the URL-scoped one
        // `gh auth setup-git` installs for github.com, so an HTTPS push
        // could never authenticate (dev-ext-4). Prompts stay disabled.
        .args([
            "-c".to_string(),
            format!("core.hooksPath={null}"),
            "-c".to_string(),
            "core.pager=cat".to_string(),
            "-c".to_string(),
            "core.autocrlf=false".to_string(),
            "-c".to_string(),
            format!("core.attributesFile={null}"),
            "-c".to_string(),
            "protocol.file.allow=always".to_string(),
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", ":")
        .env("GIT_SEQUENCE_EDITOR", ":")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_COMMITTER_DATE");
    // Fixtures do not inherit the workstation's git identity. This is
    // scoped to this git child; production builds keep ambient identity.
    #[cfg(test)]
    command
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture Author")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test");
    if let Some(worktree) = worktree {
        command.current_dir(worktree);
    }
    command
}

fn git_command(worktree: Option<&Path>) -> Command {
    git_command_on(worktree, crate::platform::OS)
}

fn git_result(worktree: Option<&Path>, args: &[&str]) -> Result<Output> {
    git_command(worktree)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.first().copied().unwrap_or("command")))
}

fn git_stdout(worktree: Option<&Path>, args: &[&str]) -> Result<String> {
    let output = git_result(worktree, args)?;
    if !output.status.success() {
        bail!("git {} failed", args.first().copied().unwrap_or("command"));
    }
    String::from_utf8(output.stdout).context("git returned non-UTF-8 output")
}

fn git_ok(worktree: &Path, args: &[&str]) -> Result<()> {
    git_stdout(Some(worktree), args).map(|_| ())
}

fn validate_destination(destination: &str) -> Result<()> {
    validate_destination_on(destination, crate::platform::OS)
}

/// [`validate_destination`] for a gateway on `os`, so the Windows rules
/// run on every host's tests (dev-ext-13; the `platform::absent_for(os,
/// …)` pattern). On Windows `C:\…`, `C:/…` and a `\\host\share\…` UNC
/// path are local repositories — and `C:` is never an scp-like SSH host.
fn validate_destination_on(destination: &str, os: &str) -> Result<()> {
    let destination = destination.trim();
    if destination.is_empty()
        || destination.starts_with('-')
        || destination.contains('\0')
        || destination.contains('\n')
        || destination.contains('\r')
    {
        bail!("choose a valid upstream or fork URL/path");
    }
    const SUPPORTED: &str = "contribution remotes support HTTPS, git, SSH (ssh://host/path or user@host:path), and local paths only";
    if let Some((scheme, _)) = destination.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "git" | "ssh" | "file"
        ) {
            bail!("{SUPPORTED}");
        }
        return Ok(());
    }
    if destination.starts_with('/')
        || destination.starts_with("./")
        || destination.starts_with("../")
    {
        return Ok(());
    }
    if os == "windows" {
        let bytes = destination.as_bytes();
        if bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
        {
            return Ok(());
        }
        // \\host\share\repo (or //host/share/repo): a local path to git on
        // Windows, never a transport.
        let unc = destination
            .strip_prefix(r"\\")
            .or_else(|| destination.strip_prefix("//"));
        if let Some(rest) = unc {
            let mut parts = rest.split(['\\', '/']);
            let host = parts.next().unwrap_or("");
            let share = parts.next().unwrap_or("");
            if !host.is_empty() && !share.is_empty() && host != "?" && host != "." {
                return Ok(());
            }
        }
        if destination.starts_with(r".\") || destination.starts_with(r"..\") {
            return Ok(());
        }
    }
    if let Some((host, path)) = destination.split_once(':') {
        // scp-like `[user@]host:path`, which git itself treats as SSH when
        // no slash precedes the first colon (`git@github.com:o/r.git`).
        // `transport::address` (an empty path before `:`) is a remote
        // helper, i.e. arbitrary programs, and stays refused.
        let plain = |part: &str| {
            !part.is_empty()
                && !part.starts_with('-')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        };
        let (user, host) = match host.rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, host),
        };
        if plain(host)
            && user.is_none_or(plain)
            && !path.is_empty()
            && !path.starts_with(':')
            && !path.starts_with('-')
        {
            return Ok(());
        }
        bail!("{SUPPORTED}");
    }
    bail!("local test repositories must be explicit absolute or ./ paths");
}

fn valid_branch(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.len() > 200 || branch.starts_with('-') {
        bail!("choose a valid contribution branch name");
    }
    git_stdout(None, &["check-ref-format", "--branch", branch])?;
    if branch == "HEAD" || branch.starts_with("refs/") {
        bail!("contribution branch must be a new, ordinary branch name");
    }
    Ok(())
}

fn confined_worktree(path: &Path) -> Result<PathBuf> {
    let base = crate::config::data_dir().join("contribution-worktrees");
    let base_metadata = base
        .symlink_metadata()
        .context("contribution worktree root is missing")?;
    if !base_metadata.is_dir() || base_metadata.file_type().is_symlink() {
        bail!("contribution worktree root is not a real directory");
    }
    let metadata = path
        .symlink_metadata()
        .context("prepared worktree is missing")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("prepared worktree must be a real directory, not a symlink");
    }
    let canonical_base = base.canonicalize()?;
    let canonical = path.canonicalize()?;
    if canonical.parent() != Some(canonical_base.as_path()) {
        bail!("worktree is outside the isolated contribution directory");
    }
    if canonical.join(".git").symlink_metadata().is_err() {
        bail!("prepared worktree has no git metadata");
    }
    let top = git_stdout(Some(&canonical), &["rev-parse", "--show-toplevel"])?;
    if Path::new(top.trim()).canonicalize()? != canonical {
        bail!("prepared worktree is not confined to its own repository root");
    }
    Ok(canonical)
}

fn preparation_binding(worktree: &Path) -> Result<Value> {
    let path = worktree.join(".git").join("veles-preparation.json");
    let raw = std::fs::read_to_string(path)
        .context("prepared contribution has no preparation binding; prepare it again")?;
    serde_json::from_str(&raw).context("prepared contribution binding is invalid; prepare it again")
}

fn prepared_state(worktree: &Path) -> Result<(String, String, String)> {
    let worktree = confined_worktree(worktree)?;
    let binding = preparation_binding(&worktree)?;
    let branch = git_stdout(
        Some(&worktree),
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )?;
    let branch = branch.trim().to_string();
    valid_branch(&branch)?;
    let commit = git_stdout(Some(&worktree), &["rev-parse", "--verify", "HEAD"])?;
    let commit = commit.trim().to_string();
    if commit.len() < 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("prepared worktree has no valid commit");
    }
    // The URL as configured, not `remote get-url`: get-url EXPANDS
    // `url.<base>.insteadOf`, so anyone with the common
    // `[url "git@github.com:"] insteadOf = https://github.com/` saw the
    // prepared origin "change" under them and could never push or open the
    // PR — and the PR head was derived from the rewritten URL.
    let origin = git_stdout(
        Some(&worktree),
        &["config", "--local", "--get", "remote.origin.url"],
    )?;
    let origin = origin.trim().to_string();
    validate_destination(&origin)?;
    if binding["version"] != 1
        || binding["branch"].as_str() != Some(branch.as_str())
        || binding["commit"].as_str() != Some(commit.as_str())
        || binding["origin"].as_str() != Some(origin.as_str())
    {
        bail!("the prepared branch, commit or origin changed; prepare and review it again");
    }
    Ok((branch, commit, origin))
}

fn prepared_files(worktree: &Path) -> Result<Vec<String>> {
    let base = git_stdout(Some(worktree), &["merge-base", "HEAD", "origin/HEAD"])?;
    let diff = git_stdout(
        Some(worktree),
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            base.trim(),
            "HEAD",
        ],
    )?;
    let files = diff
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if files.is_empty() || files.len() > MAX_FILES {
        bail!("prepared branch must contain between one and {MAX_FILES} selected files");
    }
    Ok(files)
}

fn ensure_clean_prepared(worktree: &Path) -> Result<Vec<String>> {
    let status = git_stdout(
        Some(worktree),
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !status.trim().is_empty() {
        bail!("prepared worktree changed after preparation; review and prepare it again");
    }
    let files = prepared_files(worktree)?;
    let binding = preparation_binding(worktree)?;
    let bound = binding["files"]
        .as_object()
        .context("preparation binding has no reviewed file hashes")?;
    if bound.len() != files.len() {
        bail!("prepared file selection changed; review and prepare it again");
    }
    let secrets = registered_secrets();
    for relative in &files {
        let full = selected_path(worktree, relative)?;
        let text =
            std::fs::read_to_string(&full).context("read prepared source before publishing")?;
        let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
        if bound.get(relative).and_then(Value::as_str) != Some(digest.as_str()) {
            bail!("prepared source differs from the reviewed bytes; prepare it again");
        }
        if !secret_findings(&text, &secrets).is_empty() {
            bail!(
                "prepared source contains a registered or credential-shaped value; remove it before publishing"
            );
        }
    }
    Ok(files)
}

/// Clone the chosen upstream/fork into a new isolated worktree, create a
/// validated branch, copy only the selected files of one extension under
/// `DEFAULT_DIRECTORY/<name>/`, check them, stage exactly those paths, and
/// commit with the ambient git identity. This never pushes.
pub fn prepare(root: &Path, selected: &[String], destination: &str, branch: &str) -> Result<Value> {
    prepare_many(
        &[Selection::single(root, selected)?],
        destination,
        branch,
        DEFAULT_DIRECTORY,
        false,
    )
}

/// [`prepare`] for one or more extensions in ONE branch and commit. Each
/// extension's files go under `<directory>/<name>/`, never at the
/// repository root. A selected path that already exists in the clone is
/// not overwritten: the reply is `prepared: false, conflict: true` with the
/// `existing` paths and nothing is left behind, unless `replace_existing`
/// is the caller's explicit, separately confirmed choice. A failed
/// preparation removes its partial clone.
pub fn prepare_many(
    selections: &[Selection],
    destination: &str,
    branch: &str,
    directory: &str,
    replace_existing: bool,
) -> Result<Value> {
    validate_destination(destination)?;
    valid_branch(branch)?;
    // Scan before any mutation. No clone or destination copy happens when the
    // selected sources contain credentials.
    let (directory, items) = read_items(selections, directory)?;
    let secrets = registered_secrets();
    for item in &items {
        if !secret_findings(&item.after, &secrets).is_empty() {
            bail!(
                "selected source contains a registered or credential-shaped value; replace or exclude it before preparation"
            );
        }
    }

    let base = crate::config::data_dir().join("contribution-worktrees");
    std::fs::create_dir_all(&base).context("create isolated contribution worktree root")?;
    let metadata = base.symlink_metadata()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("contribution worktree root must be a real directory");
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let worktree = base.join(format!("contribution-{id}"));
    if worktree.exists() {
        bail!("isolated contribution destination already exists; retry with a new worktree");
    }
    // A branch name is a durable request identity. The content digest
    // gives a one-to-one reservation filename, including slashes and
    // Unicode. It survives success and is released only on failure.
    let reserved = base.join(format!(
        ".branch-{:x}.lock",
        sha2::Sha256::digest(branch.as_bytes())
    ));
    if reserved.exists() {
        bail!("contribution branch already exists in a prepared worktree; choose another branch");
    }
    crate::platform::write_private(&reserved, branch.as_bytes())?;
    struct BranchReservation(PathBuf);
    impl Drop for BranchReservation {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let reservation = BranchReservation(reserved);
    let remote = git_result(
        None,
        &[
            "ls-remote",
            "--heads",
            destination,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    if !remote.status.success() {
        bail!("could not inspect the chosen origin before preparation; no clone was made");
    }
    if !remote.stdout.is_empty() {
        bail!("contribution branch already exists on origin; choose another branch");
    }
    // From the clone on, every early return removes the partial worktree
    // (dev-ext-15). The path is this call's own fresh uuid directory.
    struct PartialWorktree(Option<PathBuf>);
    impl Drop for PartialWorktree {
        fn drop(&mut self) {
            if let Some(path) = self.0.take() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
    let mut partial = PartialWorktree(Some(worktree.clone()));
    git_stdout(
        None,
        &[
            "clone",
            "--no-local",
            "--no-hardlinks",
            "--origin",
            "origin",
            destination,
            worktree.to_str().context("worktree path is not UTF-8")?,
        ],
    )
    .context("clone the chosen upstream/fork; no branch or commit was created")?;
    let worktree = confined_worktree(&worktree)?;
    git_ok(&worktree, &["switch", "--no-track", "-c", branch])?;
    if git_result(
        Some(&worktree),
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/origin/{branch}"),
        ],
    )
    .is_ok_and(|output| output.status.success())
    {
        bail!("contribution branch already exists on origin; choose another branch");
    }

    // Never overwrite what the repository already has without an explicit
    // second confirmation: a plugin README.md once replaced veles' own
    // top-level README.md in the commit (dev-ext-2).
    let mut existing = Vec::new();
    for item in &items {
        match worktree.join(&item.target).symlink_metadata() {
            Ok(_) => existing.push(item.target.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !existing.is_empty() && !replace_existing {
        return Ok(json!({
            "operation": "prepare",
            "prepared": false,
            "conflict": true,
            "existing": existing,
            "directory": directory,
            "pushed": false,
            "publish_allowed": false,
            "pr_allowed": false,
            "next": "these paths already exist in the chosen repository; confirm replacing them, choose another directory, or deselect them. Nothing was committed and the clone was removed.",
        }));
    }

    for item in &items {
        // Re-read and revalidate after clone/branch creation: this is the copy
        // boundary, and a changed or replaced source must not slip through.
        let current = file_review(&item.root, &item.relative)?;
        if current.after != item.after {
            bail!("selected source changed after review; review and prepare it again");
        }
        if !secret_findings(&current.after, &secrets).is_empty() {
            bail!("selected source changed to include credentials; preparation stopped");
        }
        let target = selected_path_for_copy(&worktree, &item.target)?;
        if let Some(parent) = target.parent() {
            ensure_safe_parents(&worktree, parent)?;
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, current.after.as_bytes())
            .with_context(|| format!("copy selected source {}", item.target))?;
        let destination_text = std::fs::read_to_string(&target)?;
        if destination_text != current.after {
            bail!("copied source does not match the reviewed source; preparation stopped");
        }
        if !secret_findings(&destination_text, &secrets).is_empty() {
            bail!("credential content detected in destination; preparation stopped before staging");
        }
    }

    let mut check_rows = Vec::new();
    let mut failures = Vec::new();
    for item in &items {
        let full = selected_path(&worktree, &item.target)?;
        let mut row = check_one(&full, &item.relative, &secrets)?;
        if row["status"] == "failed" {
            failures.push(item.target.clone());
        }
        row["extension"] = json!(item.extension);
        row["destination"] = json!(item.target);
        check_rows.push(row);
    }
    if !failures.is_empty() {
        bail!("syntax checks failed for: {}", failures.join(", "));
    }
    let targets = items
        .iter()
        .map(|item| item.target.clone())
        .collect::<Vec<_>>();
    let mut add = vec!["add", "--"];
    add.extend(targets.iter().map(String::as_str));
    git_ok(&worktree, &add)?;
    let staged = git_stdout(
        Some(&worktree),
        &[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACDMRTUXB",
            "-z",
        ],
    )?;
    let staged_names = staged
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>();
    let expected_names = targets.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if staged_names != expected_names {
        bail!("staged file set differs from the reviewed selection; preparation stopped");
    }
    let cached = git_stdout(
        Some(&worktree),
        &["diff", "--cached", "--no-ext-diff", "--no-textconv"],
    )?;
    let cached = sanitize(&cached, &secrets);
    let names = items
        .iter()
        .map(|item| item.extension.as_str())
        .collect::<BTreeSet<_>>();
    let subject = if names.len() <= 4 {
        format!(
            "Add extension sources: {} ({} files)",
            names.iter().copied().collect::<Vec<_>>().join(", "),
            targets.len()
        )
    } else {
        format!(
            "Add extension sources for {} extensions ({} files)",
            names.len(),
            targets.len()
        )
    };
    git_ok(&worktree, &["commit", "--no-verify", "-m", &subject])?;
    let commit = git_stdout(Some(&worktree), &["rev-parse", "HEAD"])?;
    let commit = commit.trim().to_string();
    let actual_diff = git_stdout(
        Some(&worktree),
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD^",
            "HEAD",
            "--",
        ],
    )?;
    let prepared_files = items
        .iter()
        .map(|item| {
            (
                item.target.clone(),
                format!("{:x}", sha2::Sha256::digest(item.after.as_bytes())),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let binding = serde_json::to_vec(&json!({
        "version": 1,
        "branch": branch,
        "commit": commit,
        "origin": destination,
        "files": prepared_files,
    }))?;
    crate::platform::write_private(
        &worktree.join(".git").join("veles-preparation.json"),
        &binding,
    )?;
    partial.0 = None;
    std::mem::forget(reservation);
    let extensions = names
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "files": items
                    .iter()
                    .filter(|item| item.extension == *name)
                    .map(|item| json!({"path": item.relative, "destination": item.target}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "operation": "prepare",
        "prepared": true,
        "worktree": worktree.display().to_string(),
        "origin": destination,
        "branch": branch,
        "commit": commit,
        "directory": directory,
        "extensions": extensions,
        "files": targets,
        "replaced": existing,
        "diff": sanitize(&actual_diff, &secrets),
        "staged_diff": cached,
        "checks": check_rows,
        "pushed": false,
        "publish_allowed": true,
        "pr_allowed": false,
        "next": "inspect the actual commit and diff; call publish explicitly to push this branch to origin",
    }))
}

fn ensure_safe_parents(root: &Path, parent: &Path) -> Result<()> {
    let relative = parent
        .strip_prefix(root)
        .context("destination parent escaped worktree")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!("destination parent has an unsafe component");
        };
        current.push(part);
        match current.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!("destination parent is not a real directory");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn selected_path_for_copy(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = validate_relative(relative)?;
    let target = root.join(path);
    match target.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("destination selected path is not a regular file")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(target)
}

/// Non-force push of an existing prepared branch to its configured `origin`.
/// A clean worktree, unchanged commit shape, safe selected paths, and a fresh
/// destination secret scan are mandatory on every call.
pub fn publish(worktree: &Path) -> Result<Value> {
    let worktree = confined_worktree(worktree)?;
    let (branch, commit, origin) = prepared_state(&worktree)?;
    let files = ensure_clean_prepared(&worktree)?;
    let remote_ref = format!("refs/heads/{branch}");
    let existing = git_result(Some(&worktree), &["ls-remote", "origin", &remote_ref])?;
    if !existing.status.success() {
        bail!("could not inspect origin before publishing; no push was attempted");
    }
    let remote_before = String::from_utf8_lossy(&existing.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    git_ok(
        &worktree,
        &[
            "push",
            "--no-force",
            "--no-verify",
            "origin",
            &format!("{branch}:{branch}"),
        ],
    )?;
    let after = git_result(Some(&worktree), &["ls-remote", "origin", &remote_ref])?;
    if !after.status.success() {
        bail!("push command returned, but the remote branch could not be verified");
    }
    let remote_after = String::from_utf8_lossy(&after.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    if remote_after != commit {
        bail!("origin branch does not match the prepared commit after publishing");
    }
    let receipt = serde_json::to_string(&json!({
        "version": 1,
        "branch": branch,
        "commit": commit,
        "files": files,
        "origin": origin.trim(),
        "remote_before": remote_before,
    }))?;
    let receipt_path = crate::config::data_dir()
        .join("contribution-worktrees")
        .join(format!(
            ".published-{}.json",
            worktree.file_name().unwrap_or_default().to_string_lossy()
        ));
    crate::platform::write_private(&receipt_path, receipt.as_bytes())?;
    Ok(json!({
        "operation": "publish",
        "worktree": worktree.display().to_string(),
        "origin": origin.trim(),
        "branch": branch,
        "commit": commit,
        "remote_before": remote_before,
        "remote_commit": remote_after,
        "forced": false,
        "pushed": true,
        "pr_allowed": true,
        "next": "open a PR only through a separate explicit pr call",
    }))
}

/// `owner/repository` of a GitHub remote URL, in any of the forms git
/// accepts for github.com (HTTPS, ssh://, scp-like, git://).
fn github_repository(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let rest = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git://github.com/",
        "git@github.com:",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

/// The `--head` for `gh pr create`. A branch pushed to a fork must be named
/// `owner:branch`; a bare branch is looked up in the BASE repository, so
/// the usual fork-to-veles PR found no head (dev-ext-5).
fn pr_head(origin: &str, repo: &str, branch: &str) -> String {
    match github_repository(origin) {
        Some((owner, name)) if !format!("{owner}/{name}").eq_ignore_ascii_case(repo.trim()) => {
            format!("{owner}:{branch}")
        }
        _ => branch.to_string(),
    }
}

/// Invoke `gh pr create` only after `publish` completed for this exact
/// worktree, branch, commit, and configured origin. No push is performed here.
pub fn pr(worktree: &Path, repo: &str, base: &str, title: &str, body: &str) -> Result<Value> {
    let worktree = confined_worktree(worktree)?;
    let (branch, commit, origin) = prepared_state(&worktree)?;
    let _files = ensure_clean_prepared(&worktree)?;
    if repo.trim().is_empty()
        || !repo
            .trim()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        || repo.split('/').count() != 2
        || base.trim().is_empty()
        || base.starts_with('-')
        || base.contains(['\n', '\r', '\0'])
        || title.trim().is_empty()
        || title.contains(['\n', '\r', '\0'])
        || body.contains('\0')
    {
        bail!("PR requires an explicit owner/repository, base, title, and body");
    }
    let receipt_path = crate::config::data_dir()
        .join("contribution-worktrees")
        .join(format!(
            ".published-{}.json",
            worktree.file_name().unwrap_or_default().to_string_lossy()
        ));
    let raw = std::fs::read_to_string(&receipt_path)
        .context("publish this prepared branch before requesting a PR")?;
    let receipt: Value =
        serde_json::from_str(&raw).context("invalid publish receipt; publish again")?;
    if receipt["version"] != 1
        || receipt["branch"].as_str() != Some(branch.as_str())
        || receipt["commit"].as_str() != Some(commit.as_str())
        || receipt["origin"].as_str() != Some(origin.as_str())
    {
        bail!("the prepared branch or origin changed after publish; publish it again before a PR");
    }
    let gh = std::env::var_os("VELES_CONTRIBUTE_GH").unwrap_or_else(|| "gh".into());
    if gh.is_empty() {
        bail!("gh is unavailable; PR was not created");
    }
    let head = pr_head(&origin, repo, &branch);
    let output = Command::new(&gh)
        .args([
            "pr", "create", "--repo", repo, "--base", base, "--head", &head, "--title", title,
            "--body", body,
        ])
        .current_dir(&worktree)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .context("gh is unavailable; install it or create the PR manually")?;
    let secrets = registered_secrets();
    if !output.status.success() {
        bail!(
            "gh could not create the PR: {}",
            output_text(&output, &secrets)
                .as_str()
                .unwrap_or("unknown error")
        );
    }
    let stdout = sanitize(&String::from_utf8_lossy(&output.stdout), &secrets);
    Ok(json!({
        "operation": "pr",
        "repo": repo,
        "base": base,
        "head": head,
        "commit": commit,
        "url": stdout.trim(),
        "pushed": false,
        "note": "PR was a separate gh action; this call did not push",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    fn fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        PathBuf,
    ) {
        let (cfg, data, guard) = crate::testenv::scratch_xdg();
        let root = crate::config::config_dir().join("plugins/demo");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("plugin.tcl"), "proc demo {} {return ok}\n").unwrap();
        std::fs::write(
            root.join("nested/helper.tcl"),
            "proc helper {} {return ok}\n",
        )
        .unwrap();
        (cfg, data, guard, root)
    }

    fn git(dir: &Path, args: &[&str]) -> Output {
        let output = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_AUTHOR_NAME", "Fixture Author")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
            .env("GIT_COMMITTER_NAME", "Fixture Author")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn upstream(base: &Path) -> PathBuf {
        let repo = base.join("upstream");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Fixture Author"]);
        git(&repo, &["config", "user.email", "fixture@example.test"]);
        std::fs::write(repo.join("README.md"), "local fixture\n").unwrap();
        git(&repo, &["add", "--", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "fixture base"]);
        repo
    }

    #[test]
    fn candidates_are_recursive_and_reject_local_state() {
        let (_cfg, _data, _guard, root) = fixture();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/dependency")).unwrap();
        for path in [
            ".env",
            "secrets.env",
            "node.toml",
            ".git/config",
            "node_modules/dependency/index.js",
            "nested/debug.log",
        ] {
            std::fs::write(root.join(path), "local only\n").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("plugin.tcl"), root.join("nested/link.tcl"))
                .unwrap();
            std::os::unix::fs::symlink(root.join("nested"), root.join("linked-dir")).unwrap();
        }
        let report = candidates(&root).unwrap();
        let paths = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["nested/helper.tcl", "plugin.tcl"]);
        for path in [
            "../outside.tcl",
            "/absolute.tcl",
            "nested/../../outside.tcl",
            "nested/link.tcl",
            "nested/.hidden.tcl",
            "node.toml",
        ] {
            assert!(review(&root, &strings(&[path])).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn review_redacts_and_blocks_without_leaking_values() {
        let (_cfg, _data, _guard, root) = fixture();
        let canary = "contribution-test-canary-not-a-live-credential-8291";
        std::fs::write(
            crate::config::config_dir().join("secrets.env"),
            format!("TEST_CONTRIBUTION_CANARY={canary}\n"),
        )
        .unwrap();
        std::fs::write(root.join("nested/helper.tcl"), format!("# {canary}\n")).unwrap();
        let encoded =
            serde_json::to_string(&review(&root, &strings(&["nested/helper.tcl"])).unwrap())
                .unwrap();
        assert!(!encoded.contains(canary));
        assert!(encoded.contains("REDACTED"));
        let report = review(&root, &strings(&["nested/helper.tcl"])).unwrap();
        assert_eq!(report["prepare_allowed"], false);
        assert_eq!(report["files"][0]["blocked"], true);
        let shaped = review(&root, &strings(&["plugin.tcl"])).unwrap();
        assert_eq!(shaped["prepare_allowed"], true);
        assert_eq!(shaped["files"][0]["before"], "uncommitted-addition");
        std::fs::write(
            root.join("plugin.tcl"),
            "set key ghp_abcdefghijklmnopqrstuvwxyz123456\n",
        )
        .unwrap();
        let error = format!(
            "{:#}",
            prepare(&root, &strings(&["plugin.tcl"]), "/unused", "topic").unwrap_err()
        );
        assert!(error.contains("credential"));
        assert!(!error.contains("ghp_abcdefghijklmnopqrstuvwxyz123456"));
    }

    #[test]
    fn windows_checker_lookup_accepts_an_executable_suffix() {
        let (_cfg, _data, _guard, _root) = fixture();
        let tools = crate::config::config_dir().join("windows-check-tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("tclsh.exe"), b"fixture").unwrap();
        std::fs::write(tools.join("node.exe"), b"fixture").unwrap();
        let path = std::env::join_paths([tools]).unwrap();
        assert!(
            check_command_available_in(&path, "tclsh", "windows"),
            "Windows PATH lookup missed tclsh.exe"
        );
        assert!(
            check_command_available_in(&path, "node", "windows"),
            "Windows PATH lookup missed node.exe"
        );
        assert!(!check_command_available_in(&path, "tsc", "windows"));
    }

    #[test]
    fn windows_git_policy_uses_the_windows_null_device_on_any_host() {
        let windows = git_command_on(None, "windows");
        let args = windows
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for setting in ["core.hooksPath=NUL", "core.attributesFile=NUL"] {
            assert_eq!(
                args.iter().filter(|arg| *arg == setting).count(),
                1,
                "missing or duplicate {setting}: {args:?}"
            );
        }
        assert!(
            !args.iter().any(|arg| arg.contains("/dev/null")),
            "{args:?}"
        );
        for (index, arg) in args.iter().enumerate() {
            if arg == "core.hooksPath=NUL" || arg == "core.attributesFile=NUL" {
                assert_eq!(args[index - 1], "-c");
            }
        }

        let unix = git_command_on(None, "linux");
        let args = unix
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"core.hooksPath=/dev/null".to_string()));
        assert!(args.contains(&"core.attributesFile=/dev/null".to_string()));
    }

    #[test]
    fn checks_never_evaluate_and_typescript_never_uses_node() {
        let (_cfg, _data, _guard, root) = fixture();
        let marker = crate::config::config_dir().join("evaluated-marker");
        let tcl = format!(
            "set f [open {{{}}} w]; puts $f evaluated; close $f\n",
            marker.display()
        );
        std::fs::write(root.join("plugin.tcl"), tcl).unwrap();
        std::fs::write(
            root.join("nested/plugin.js"),
            "require('node:fs').writeFileSync(process.env.CONTRIBUTE_MARKER, 'evaluated')\n",
        )
        .unwrap();
        std::fs::write(
            root.join("nested/plugin.ts"),
            "const value: string = 'typescript';\n",
        )
        .unwrap();
        std::fs::write(root.join("nested/plugin.rkt"), "#lang racket\n").unwrap();
        std::fs::write(root.join("nested/plugin.lisp"), "(defun demo () t)\n").unwrap();
        let old_marker = std::env::var_os("CONTRIBUTE_MARKER");
        let old_node = std::env::var_os("VELES_TEST_CONTRIBUTE_NODE");
        std::env::set_var("CONTRIBUTE_MARKER", &marker);
        let tools = crate::config::config_dir().join("test-tools");
        std::fs::create_dir_all(&tools).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                tools.join("node"),
                "#!/bin/sh\necho javascript-must-not-receive-typescript >&2\nexit 91\n",
            )
            .unwrap();
            std::fs::set_permissions(tools.join("node"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::env::set_var("VELES_TEST_CONTRIBUTE_NODE", tools.join("node"));
        }
        let checked = run_checks(
            &root,
            &strings(&[
                "plugin.tcl",
                "nested/plugin.js",
                "nested/plugin.ts",
                "nested/plugin.rkt",
                "nested/plugin.lisp",
            ]),
        )
        .unwrap();
        match old_node {
            Some(path) => std::env::set_var("VELES_TEST_CONTRIBUTE_NODE", path),
            None => std::env::remove_var("VELES_TEST_CONTRIBUTE_NODE"),
        }
        match old_marker {
            Some(value) => std::env::set_var("CONTRIBUTE_MARKER", value),
            None => std::env::remove_var("CONTRIBUTE_MARKER"),
        }
        assert!(!marker.exists(), "a checker evaluated extension code");
        let rows = checked["checks"].as_array().unwrap();
        assert_eq!(rows[0]["status"], "passed");
        #[cfg(unix)]
        assert_eq!(rows[1]["status"], "failed");
        #[cfg(windows)]
        assert_eq!(
            rows[1]["status"],
            if check_command_available("node") {
                "passed"
            } else {
                "unavailable"
            },
            "{checked}"
        );
        // TypeScript goes to tsc when installed, never to the node stub.
        let typescript = if check_command_available("tsc") {
            "passed"
        } else {
            "unavailable"
        };
        assert_eq!(rows[2]["status"], typescript, "{checked}");
        assert_eq!(rows[3]["status"], "unavailable");
        assert_eq!(rows[4]["status"], "unavailable");
    }

    #[test]
    fn prepare_commits_only_selected_files_and_does_not_push() {
        let (_cfg, _data, _guard, root) = fixture();
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let upstream = upstream(&base);
        std::fs::write(root.join("ignored.tcl"), "# must not be committed\n").unwrap();
        let report = prepare(
            &root,
            &strings(&["plugin.tcl", "nested/helper.tcl"]),
            upstream.to_str().unwrap(),
            "safe/topic",
        )
        .unwrap();
        let worktree = PathBuf::from(report["worktree"].as_str().unwrap());
        assert_eq!(report["operation"], "prepare");
        assert_eq!(report["pushed"], false);
        assert_eq!(report["branch"], "safe/topic");
        assert!(report["diff"]
            .as_str()
            .unwrap()
            .contains("nested/helper.tcl"));
        let files = git(
            &worktree,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        );
        assert_eq!(
            String::from_utf8_lossy(&files.stdout),
            "plugins/examples/demo/nested/helper.tcl\nplugins/examples/demo/plugin.tcl\n"
        );
        assert!(!worktree.join("ignored.tcl").exists());
        assert!(!worktree.join("plugins/examples/demo/ignored.tcl").exists());
        let author = git(&worktree, &["show", "-s", "--format=%ae", "HEAD"]);
        assert_eq!(
            String::from_utf8_lossy(&author.stdout).trim(),
            "fixture@example.test"
        );
        let branches = git(&upstream, &["branch", "--list", "safe/topic"]);
        assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
        let second = prepare(
            &root,
            &strings(&["plugin.tcl"]),
            upstream.to_str().unwrap(),
            "safe/topic",
        )
        .unwrap_err();
        assert!(second.to_string().contains("already exists"));
    }

    #[cfg(unix)]
    #[test]
    fn inherited_git_execution_policy_cannot_replace_contribution_safeguards() {
        let (_cfg, _data, _guard, root) = fixture();
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let upstream = upstream(&base);
        let marker = base.join("unexpected-hook");
        let hooks = base.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = hooks.join("pre-commit");
            std::fs::write(&hook, "#!/bin/sh\ntouch \"$CONTRIBUTE_TEST_MARKER\"\n").unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let global = base.join("global.gitconfig");
        std::fs::write(
            &global,
            format!("[core]\nhooksPath = {}\n", hooks.display()),
        )
        .unwrap();
        let old_global = std::env::var_os("GIT_CONFIG_GLOBAL");
        let old_count = std::env::var_os("GIT_CONFIG_COUNT");
        let old_key = std::env::var_os("GIT_CONFIG_KEY_0");
        let old_value = std::env::var_os("GIT_CONFIG_VALUE_0");
        let old_marker = std::env::var_os("CONTRIBUTE_TEST_MARKER");
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "core.hooksPath");
        std::env::set_var("GIT_CONFIG_VALUE_0", &hooks);
        std::env::set_var("CONTRIBUTE_TEST_MARKER", &marker);
        let result = prepare(
            &root,
            &strings(&["plugin.tcl"]),
            upstream.to_str().unwrap(),
            "git-policy",
        );
        match old_global {
            Some(value) => std::env::set_var("GIT_CONFIG_GLOBAL", value),
            None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
        }
        for (key, old) in [
            ("GIT_CONFIG_COUNT", old_count),
            ("GIT_CONFIG_KEY_0", old_key),
            ("GIT_CONFIG_VALUE_0", old_value),
        ] {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        match old_marker {
            Some(value) => std::env::set_var("CONTRIBUTE_TEST_MARKER", value),
            None => std::env::remove_var("CONTRIBUTE_TEST_MARKER"),
        }
        result.unwrap();
        assert!(
            !marker.exists(),
            "an ambient git hook ran during contribution"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_contribution_git_uses_the_explicit_no_hook_policy() {
        let command = git_command_on(None, "windows");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"core.hooksPath=NUL".to_string()));
        assert!(args.contains(&"core.attributesFile=NUL".to_string()));
        assert!(args.contains(&"-c".to_string()));
        // On Windows a shell hook is not a portable marker for "git ran
        // a hook". Assert the policy that every contribution git command
        // supplies, while the Unix fixture actually executes the hook test.
        assert!(args.contains(&"core.pager=cat".to_string()));
        for (name, value) in command.get_envs() {
            if name == "GIT_DIR" || name == "GIT_WORK_TREE" || name == "GIT_INDEX_FILE" {
                assert!(value.is_none(), "inherited {name:?} survived");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepare_refuses_a_source_edit_between_review_and_copy() {
        let (_cfg, _data, _guard, root) = fixture();
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let upstream = upstream(&base);
        let tools = base.join("clone-tools");
        std::fs::create_dir_all(&tools).unwrap();
        // `which` is not part of a clean build sandbox. Resolve the real
        // fixture through PATH before installing the race wrapper; do not
        // add a second host-tool dependency just to find another one.
        let path = std::env::var_os("PATH").expect("fixture git needs PATH");
        let real_git = std::env::split_paths(&path)
            .map(|dir| dir.join("git"))
            .find(|candidate| candidate.is_file())
            .expect("locate fixture git in PATH")
            .canonicalize()
            .expect("canonical fixture git");
        let wrapper = tools.join("git");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = clone ]; then\n    \"$VELES_TEST_REAL_GIT\" \"$@\" || exit $?\n    printf '%s\\n' changed-after-review > \"$VELES_TEST_CONTRIBUTE_SOURCE\"\n    exit 0\n  fi\ndone\nexec \"$VELES_TEST_REAL_GIT\" \"$@\"\n"
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let old_git = std::env::var_os("VELES_TEST_REAL_GIT");
        let old_source = std::env::var_os("VELES_TEST_CONTRIBUTE_SOURCE");
        std::env::set_var("VELES_TEST_REAL_GIT", real_git);
        std::env::set_var("VELES_TEST_CONTRIBUTE_SOURCE", root.join("plugin.tcl"));
        std::env::set_var(
            "PATH",
            format!("{}:{}", tools.display(), old_path.to_string_lossy()),
        );
        let result = prepare(
            &root,
            &strings(&["plugin.tcl"]),
            upstream.to_str().unwrap(),
            "changed-source",
        );
        std::env::set_var("PATH", old_path);
        match old_git {
            Some(value) => std::env::set_var("VELES_TEST_REAL_GIT", value),
            None => std::env::remove_var("VELES_TEST_REAL_GIT"),
        }
        match old_source {
            Some(value) => std::env::set_var("VELES_TEST_CONTRIBUTE_SOURCE", value),
            None => std::env::remove_var("VELES_TEST_CONTRIBUTE_SOURCE"),
        }
        let error = result.unwrap_err().to_string();
        assert!(error.contains("changed after review"), "{error}");
        // dev-ext-15: the failed preparation's clone is gone, and its
        // branch reservation with it.
        assert_eq!(
            partial_worktrees(&base),
            0,
            "a failed prepare left its clone"
        );
    }

    fn partial_worktrees(base: &Path) -> usize {
        std::fs::read_dir(base.join("contribution-worktrees"))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("contribution-")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn tclsh_present() -> bool {
        if check_command_available("tclsh") {
            return true;
        }
        eprintln!("SKIP: tclsh is not on PATH; the Tcl checker cannot be exercised here");
        false
    }

    #[test]
    fn an_incomplete_tcl_file_fails_the_check_and_blocks_prepare() {
        // dev-ext-1: `tclsh -c <script>` was never a check; every Tcl file
        // "passed". A file with an unclosed brace must fail and stop
        // preparation before anything is committed.
        if !tclsh_present() {
            return;
        }
        let (_cfg, _data, _guard, root) = fixture();
        std::fs::write(
            root.join("nested/helper.tcl"),
            "proc broken {} {\n    return 1\n",
        )
        .unwrap();
        let report = run_checks(&root, &strings(&["plugin.tcl", "nested/helper.tcl"])).unwrap();
        let rows = report["checks"].as_array().unwrap();
        assert_eq!(rows[0]["status"], "passed", "{report}");
        assert_eq!(rows[1]["status"], "failed", "{report}");
        assert_eq!(report["passed"], false);
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let upstream = upstream(&base);
        let error = prepare(
            &root,
            &strings(&["plugin.tcl", "nested/helper.tcl"]),
            upstream.to_str().unwrap(),
            "broken-tcl",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("syntax checks failed"), "{error}");
        assert_eq!(
            partial_worktrees(&base),
            0,
            "a failed prepare left its clone"
        );
    }

    #[test]
    fn the_typescript_scaffold_passes_the_tsc_check() {
        // dev-ext-11: tsc without the host's `veles` global and module
        // settings failed every scaffolded TypeScript extension.
        if !check_command_available("tsc") {
            eprintln!("SKIP: tsc is not on PATH");
            return;
        }
        let (_cfg, _data, _guard, root) = fixture();
        let files = crate::plugins::scaffold_files("demo", "", "js").unwrap();
        let entry = files.iter().find(|f| f.name == "plugin.ts").unwrap();
        std::fs::write(root.join("plugin.ts"), &entry.text).unwrap();
        std::fs::write(root.join("broken.ts"), "await veles.call(\n").unwrap();
        let report = run_checks(&root, &strings(&["plugin.ts", "broken.ts"])).unwrap();
        assert_eq!(report["checks"][0]["status"], "passed", "{report}");
        assert_eq!(report["checks"][1]["status"], "failed", "{report}");
    }

    #[test]
    fn several_extensions_go_into_one_branch_each_under_its_own_directory() {
        // dev-ext-2 / dev-ext-3: files were copied at their plugin-relative
        // path into the repository ROOT, so a plugin README.md replaced the
        // upstream README.md, and only one extension fit in a PR.
        let (_cfg, _data, _guard, root) = fixture();
        std::fs::write(root.join("README.md"), "demo extension readme\n").unwrap();
        let other = crate::config::config_dir().join("plugins/other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("plugin.tcl"), "proc other {} {return ok}\n").unwrap();
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let upstream = upstream(&base);
        let selections = vec![
            Selection {
                name: "demo".into(),
                root: root.clone(),
                files: strings(&["README.md", "plugin.tcl"]),
            },
            Selection {
                name: "other".into(),
                root: other.clone(),
                files: strings(&["plugin.tcl"]),
            },
        ];
        let reviewed = review_many(&selections, DEFAULT_DIRECTORY).unwrap();
        assert_eq!(
            reviewed["files"][0]["destination"],
            "plugins/examples/demo/README.md"
        );
        assert!(reviewed["diff"]
            .as_str()
            .unwrap()
            .contains("b/plugins/examples/other/plugin.tcl"));
        let prepared = prepare_many(
            &selections,
            upstream.to_str().unwrap(),
            "bundle",
            DEFAULT_DIRECTORY,
            false,
        )
        .unwrap();
        assert_eq!(prepared["prepared"], true, "{prepared}");
        let worktree = PathBuf::from(prepared["worktree"].as_str().unwrap());
        let files = git(
            &worktree,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        );
        assert_eq!(
            String::from_utf8_lossy(&files.stdout),
            "plugins/examples/demo/README.md\nplugins/examples/demo/plugin.tcl\nplugins/examples/other/plugin.tcl\n"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("README.md")).unwrap(),
            "local fixture\n",
            "the upstream README.md must be untouched"
        );
        let subject = git(&worktree, &["show", "-s", "--format=%s", "HEAD"]);
        assert_eq!(
            String::from_utf8_lossy(&subject.stdout).trim(),
            "Add extension sources: demo, other (3 files)"
        );

        // The upstream already carries this extension: an update must not
        // silently overwrite, and the refused attempt leaves nothing behind.
        std::fs::create_dir_all(upstream.join("plugins/examples/demo")).unwrap();
        std::fs::write(
            upstream.join("plugins/examples/demo/plugin.tcl"),
            "proc demo {} {return old}\n",
        )
        .unwrap();
        git(
            &upstream,
            &["add", "--", "plugins/examples/demo/plugin.tcl"],
        );
        git(&upstream, &["commit", "-q", "-m", "older demo"]);
        let before = partial_worktrees(&base);
        let single = vec![Selection {
            name: "demo".into(),
            root: root.clone(),
            files: strings(&["plugin.tcl"]),
        }];
        let conflict = prepare_many(
            &single,
            upstream.to_str().unwrap(),
            "update",
            DEFAULT_DIRECTORY,
            false,
        )
        .unwrap();
        assert_eq!(conflict["prepared"], false, "{conflict}");
        assert_eq!(conflict["conflict"], true);
        assert_eq!(
            conflict["existing"],
            json!(["plugins/examples/demo/plugin.tcl"])
        );
        assert_eq!(
            partial_worktrees(&base),
            before,
            "the refused clone was kept"
        );
        let replaced = prepare_many(
            &single,
            upstream.to_str().unwrap(),
            "update",
            DEFAULT_DIRECTORY,
            true,
        )
        .unwrap();
        assert_eq!(replaced["prepared"], true, "{replaced}");
        assert_eq!(
            replaced["replaced"],
            json!(["plugins/examples/demo/plugin.tcl"])
        );
        let duplicate = prepare_many(
            &[
                Selection::single(&root, &strings(&["plugin.tcl"])).unwrap(),
                Selection::single(&root, &strings(&["plugin.tcl"])).unwrap(),
            ],
            upstream.to_str().unwrap(),
            "duplicate",
            DEFAULT_DIRECTORY,
            false,
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("only once"));
        assert!(review_many(&selections, "../outside").is_err());
        assert!(review_many(&selections, "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ambient_url_scoped_credential_helpers_survive_the_git_policy() {
        // dev-ext-4: `-c credential.helper=` reset every helper, including
        // the github.com-scoped one `gh auth setup-git` installs, so an
        // HTTPS push could never authenticate.
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let (_cfg, _data, _guard, _root) = fixture();
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let helper = base.join("credential-helper");
        std::fs::write(
            &helper,
            "#!/bin/sh\necho username=fixture\necho password=fixture-helper-value\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let global = base.join("credential.gitconfig");
        std::fs::write(
            &global,
            format!(
                "[credential \"https://forge.invalid\"]\n\thelper = !{}\n",
                helper.display()
            ),
        )
        .unwrap();
        let old_global = std::env::var_os("GIT_CONFIG_GLOBAL");
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        let mut child = git_command(None)
            .args(["credential", "fill"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=forge.invalid\n\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        match old_global {
            Some(value) => std::env::set_var("GIT_CONFIG_GLOBAL", value),
            None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
        }
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .contains("password=fixture-helper-value"),
            "the URL-scoped helper did not answer: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn destinations_accept_scp_like_ssh_and_refuse_remote_helpers() {
        for ok in [
            "git@github.com:owner/veles.git",
            "github.com:owner/veles",
            "ssh://git@github.com/owner/veles.git",
            "https://github.com/owner/veles.git",
            "/srv/git/veles.git",
            "./veles.git",
        ] {
            assert!(validate_destination(ok).is_ok(), "refused {ok}");
        }
        for bad in [
            "ext::sh -c touch% /tmp/pwned",
            "fd::3",
            "xyz://host/repo",
            "-uupload-pack=x",
            "git@github.com:-oProxyCommand=x",
            "git@:repo",
            "relative/path",
        ] {
            assert!(validate_destination(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn a_fork_pr_names_the_fork_owner_in_its_head() {
        // dev-ext-5: a bare `--head branch` is resolved in the BASE repo.
        for (origin, expected) in [
            ("https://github.com/alice/veles.git", "alice:topic"),
            ("git@github.com:alice/veles.git", "alice:topic"),
            ("ssh://git@github.com/alice/veles", "alice:topic"),
            ("https://github.com/77787777778777/veles.git", "topic"),
            ("/srv/git/veles.git", "topic"),
        ] {
            assert_eq!(
                pr_head(origin, DEFAULT_UPSTREAM_REPO, "topic"),
                expected,
                "{origin}"
            );
        }
    }

    #[test]
    fn nested_candidates_use_forward_slashes_and_flag_binary_and_state_files() {
        let (_cfg, _data, _guard, root) = fixture();
        std::fs::write(root.join("state.json"), "{}\n").unwrap();
        std::fs::write(root.join("icon.bin"), [0xff_u8, 0xfe, 0x00, 0x81]).unwrap();
        // Every extension language's sources are suggested ("Select all
        // extensions" ticks exactly these): a Rust crate and a C plugin
        // were left out before 0.2.0.
        std::fs::create_dir_all(root.join("src")).unwrap();
        for (name, text) in [
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            ("src/lib.rs", "pub fn x() {}\n"),
            ("build.rs", "fn main() {}\n"),
            ("native.c", "int x;\n"),
            ("native.h", "int x;\n"),
            ("tsconfig.json", "{}\n"),
            ("plugin.mts", "export {};\n"),
            ("LICENSE", "MIT\n"),
        ] {
            std::fs::write(root.join(name), text).unwrap();
        }
        let report = candidates(&root).unwrap();
        let rows = report["files"].as_array().unwrap();
        let row = |path: &str| {
            rows.iter()
                .find(|row| row["path"] == path)
                .unwrap_or_else(|| panic!("{path} missing from {report}"))
                .clone()
        };
        assert_eq!(row("nested/helper.tcl")["suggested"], true);
        for source in [
            "Cargo.toml",
            "src/lib.rs",
            "build.rs",
            "native.c",
            "native.h",
            "tsconfig.json",
            "plugin.mts",
            "LICENSE",
        ] {
            assert_eq!(row(source)["suggested"], true, "{source}");
        }
        assert_eq!(row("state.json")["suggested"], false);
        assert_eq!(row("icon.bin")["text"], false);
        assert_eq!(row("icon.bin")["suggested"], false);
    }

    #[test]
    fn publish_and_pr_are_separate_local_actions() {
        let (_cfg, _data, _guard, root) = fixture();
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let upstream_repo = upstream(&base);
        let remote = base.join("remote.git");
        git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                upstream_repo.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        let prepared = prepare(
            &root,
            &strings(&["plugin.tcl"]),
            remote.to_str().unwrap(),
            "topic",
        )
        .unwrap();
        let worktree = PathBuf::from(prepared["worktree"].as_str().unwrap());
        let no_push = pr(&worktree, "owner/repo", "main", "title", "body").unwrap_err();
        assert!(no_push.to_string().contains("publish"));
        let published = publish(&worktree).unwrap();
        assert_eq!(published["operation"], "publish");
        assert_eq!(published["forced"], false);
        let remote_commit = git(&remote, &["rev-parse", "refs/heads/topic"]);
        assert_eq!(
            String::from_utf8_lossy(&remote_commit.stdout).trim(),
            published["commit"].as_str().unwrap()
        );
        let other = upstream(&base.join("other"));
        git(
            &worktree,
            &["remote", "set-url", "origin", other.to_str().unwrap()],
        );
        let changed_origin = publish(&worktree).unwrap_err().to_string();
        assert!(
            changed_origin.contains("origin changed"),
            "a changed destination must not silently inherit the prepared commit: {changed_origin}"
        );
        let pushed_elsewhere = git(
            &other,
            &["ls-remote", other.to_str().unwrap(), "refs/heads/topic"],
        );
        assert!(pushed_elsewhere.stdout.is_empty());
        git(
            &worktree,
            &["remote", "set-url", "origin", remote.to_str().unwrap()],
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tools = base.join("tools");
            std::fs::create_dir_all(&tools).unwrap();
            let log = base.join("gh-args");
            std::fs::write(tools.join("gh"), "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$GH_LOG\"\nprintf 'https://example.invalid/local-pr\\n'\n").unwrap();
            std::fs::set_permissions(tools.join("gh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            let old_path = std::env::var_os("PATH");
            let old_log = std::env::var_os("GH_LOG");
            let old_gh = std::env::var_os("VELES_CONTRIBUTE_GH");
            std::env::set_var("VELES_CONTRIBUTE_GH", tools.join("gh"));
            std::env::set_var("GH_LOG", &log);
            let opened = pr(&worktree, "owner/repo", "main", "title", "body").unwrap();
            assert_eq!(opened["url"], "https://example.invalid/local-pr");
            match old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            match old_log {
                Some(value) => std::env::set_var("GH_LOG", value),
                None => std::env::remove_var("GH_LOG"),
            }
            match old_gh {
                Some(value) => std::env::set_var("VELES_CONTRIBUTE_GH", value),
                None => std::env::remove_var("VELES_CONTRIBUTE_GH"),
            }
            let args = std::fs::read_to_string(&log).unwrap();
            let args = args.lines().collect::<Vec<_>>();
            assert_eq!(
                args,
                [
                    "pr",
                    "create",
                    "--repo",
                    "owner/repo",
                    "--base",
                    "main",
                    "--head",
                    "topic",
                    "--title",
                    "title",
                    "--body",
                    "body",
                ]
            );
        }
        std::fs::write(
            worktree.join("plugin.tcl"),
            "# uncommitted secret attempt\n",
        )
        .unwrap();
        assert!(publish(&worktree).is_err());
    }

    #[test]
    fn windows_destinations_and_nested_paths_follow_windows_rules_on_any_host() {
        // dev-ext-13 / release W6, exercised on Linux with the platform
        // as a parameter.
        for ok in [
            r"C:\Users\Ann\src\veles",
            "C:/Users/Ann/src/veles.git",
            r"\\nas\git\veles.git",
            r".\veles.git",
            "git@github.com:ann/veles.git",
            "https://github.com/ann/veles.git",
        ] {
            assert!(
                validate_destination_on(ok, "windows").is_ok(),
                "refused {ok}"
            );
        }
        for bad in [r"\\?\C:\x", r"\\host", "ext::sh -c x", "C:"] {
            assert!(
                validate_destination_on(bad, "windows").is_err(),
                "accepted {bad}"
            );
        }
        // Off Windows a drive path is not a local repository.
        assert!(validate_destination_on(r".\veles.git", "linux").is_err());
        // A nested candidate in Windows spelling becomes a repository path;
        // the same text off Windows is one file name with a backslash,
        // which is refused rather than misread.
        let nested = slash_relative(r"nested\deeper\helper.tcl", "windows");
        assert_eq!(nested, "nested/deeper/helper.tcl");
        assert!(validate_relative(&nested).is_ok());
        assert!(validate_relative(&slash_relative(r"nested\helper.tcl", "linux")).is_err());
        assert_eq!(
            slash_relative("nested/helper.tcl", "linux"),
            "nested/helper.tcl"
        );
    }

    /// Every request the fixture server got: `METHOD target` and the
    /// Authorization header it carried.
    #[cfg(unix)]
    type SeenRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// A smart-HTTP git server on loopback that demands Basic auth, in
    /// front of `git http-backend`. It records the Authorization header
    /// of every request it served.
    #[cfg(unix)]
    struct AuthGitServer {
        port: u16,
        seen: SeenRequests,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(unix)]
    impl AuthGitServer {
        fn start(project_root: PathBuf, expected: String) -> AuthGitServer {
            use std::io::{BufRead, Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (seen2, stop2) = (seen.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    let (sock, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            continue;
                        }
                        Err(_) => break,
                    };
                    sock.set_nonblocking(false).unwrap();
                    let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
                    let mut out = sock;
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.is_empty() {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let target = parts.next().unwrap_or("").to_string();
                    let mut headers = HashMap::new();
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    let auth = headers.get("authorization").cloned();
                    seen2
                        .lock()
                        .unwrap()
                        .push((format!("{method} {target}"), auth.clone()));
                    if auth.as_deref() != Some(expected.as_str()) {
                        let _ = out.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        continue;
                    }
                    if headers
                        .get("expect")
                        .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
                    {
                        let _ = out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                    }
                    let mut body = Vec::new();
                    if headers
                        .get("transfer-encoding")
                        .is_some_and(|t| t.contains("chunked"))
                    {
                        loop {
                            let mut size = String::new();
                            reader.read_line(&mut size).unwrap();
                            let n =
                                usize::from_str_radix(size.trim().split(';').next().unwrap(), 16)
                                    .unwrap_or(0);
                            if n == 0 {
                                let mut end = String::new();
                                let _ = reader.read_line(&mut end);
                                break;
                            }
                            let mut chunk = vec![0u8; n];
                            reader.read_exact(&mut chunk).unwrap();
                            body.extend_from_slice(&chunk);
                            let mut crlf = [0u8; 2];
                            reader.read_exact(&mut crlf).unwrap();
                        }
                    } else if let Some(n) = headers
                        .get("content-length")
                        .and_then(|n| n.parse::<usize>().ok())
                    {
                        body.resize(n, 0);
                        reader.read_exact(&mut body).unwrap();
                    }
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    let mut cgi = Command::new("git");
                    cgi.arg("http-backend")
                        .env_clear()
                        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("GIT_PROJECT_ROOT", &project_root)
                        .env("GIT_HTTP_EXPORT_ALL", "1")
                        .env("REQUEST_METHOD", &method)
                        .env("PATH_INFO", path)
                        .env("QUERY_STRING", query)
                        .env(
                            "CONTENT_TYPE",
                            headers.get("content-type").cloned().unwrap_or_default(),
                        )
                        .env("CONTENT_LENGTH", body.len().to_string())
                        .env("REMOTE_USER", "alice")
                        .env("REMOTE_ADDR", "127.0.0.1")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped());
                    if let Some(enc) = headers.get("content-encoding") {
                        cgi.env("HTTP_CONTENT_ENCODING", enc);
                    }
                    if let Some(proto) = headers.get("git-protocol") {
                        cgi.env("HTTP_GIT_PROTOCOL", proto);
                    }
                    let mut child = cgi.spawn().unwrap();
                    child.stdin.take().unwrap().write_all(&body).unwrap();
                    let output = child.wait_with_output().unwrap();
                    let raw = output.stdout;
                    let split = raw
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| (i, 4))
                        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)))
                        .unwrap_or((raw.len(), 0));
                    let head = String::from_utf8_lossy(&raw[..split.0]).to_string();
                    let mut status = "200 OK".to_string();
                    let mut response = String::new();
                    for h in head.lines() {
                        if let Some(s) = h.strip_prefix("Status:") {
                            status = s.trim().to_string();
                        } else if !h.trim().is_empty() {
                            response.push_str(h.trim_end());
                            response.push_str("\r\n");
                        }
                    }
                    let _ = out.write_all(
                        format!("HTTP/1.1 {status}\r\n{response}Connection: close\r\n\r\n")
                            .as_bytes(),
                    );
                    let _ = out.write_all(&raw[split.0 + split.1..]);
                }
            });
            AuthGitServer { port, seen, stop }
        }
    }

    #[cfg(unix)]
    impl Drop for AuthGitServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Owner ask 287976.2: "wrap all of their existing extensions into a PR
    /// to veles". Every user extension — Tcl with nested files, a
    /// TypeScript one from the scaffold, a Rust plugin crate — is selected
    /// the way "Select all extensions" does (every suggested source), lands
    /// under plugins/examples/<name>/ in ONE branch, is pushed to the
    /// author's fork with the credential their git is configured with, and
    /// `gh pr create` gets `--head <fork owner>:<branch>`. The fork is a
    /// loopback HTTPS-shaped remote: `https://github.com/alice/veles.git`,
    /// rewritten by the author's own `insteadOf` — which is also how the
    /// insteadOf bug in `prepared_state` was found.
    #[cfg(unix)]
    #[test]
    fn every_user_extension_goes_to_the_fork_in_one_branch_with_the_configured_credential() {
        use base64::Engine;
        use std::os::unix::fs::PermissionsExt;
        let (_cfg, _data, _guard, demo) = fixture();
        let plugins = crate::config::config_dir().join("plugins");
        std::fs::write(demo.join("plugin.toml"), "name = \"demo\"\n").unwrap();
        std::fs::write(demo.join("notes.log"), "local\n").unwrap();
        // A TypeScript extension as the New extension scaffold writes it.
        let ts = plugins.join("tsx");
        std::fs::create_dir_all(&ts).unwrap();
        for f in crate::plugins::scaffold_files("tsx", "", "js").unwrap() {
            std::fs::write(ts.join(f.name), f.text).unwrap();
        }
        // An out-of-tree Rust plugin crate, with build output and an .env.
        let rs = plugins.join("rustx");
        std::fs::create_dir_all(rs.join("src")).unwrap();
        std::fs::create_dir_all(rs.join("target/debug")).unwrap();
        std::fs::write(
            rs.join("plugin.toml"),
            "name = \"rustx\"\nkind = \"rust\"\n",
        )
        .unwrap();
        std::fs::write(
            rs.join("Cargo.toml"),
            "[package]\nname = \"rustx\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(rs.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(rs.join("target/debug/librustx.so"), "binary\n").unwrap();
        std::fs::write(rs.join(".env"), "TOKEN=nope\n").unwrap();

        // "Select all extensions": every suggested source of every one.
        let mut names: Vec<String> = std::fs::read_dir(&plugins)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("plugin.toml").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["demo", "rustx", "tsx"]);
        let selections: Vec<Selection> = names
            .iter()
            .map(|name| {
                let root = plugins.join(name);
                let files = candidates(&root).unwrap()["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|f| f["suggested"] == true)
                    .map(|f| f["path"].as_str().unwrap().to_string())
                    .collect();
                Selection {
                    name: name.clone(),
                    root,
                    files,
                }
            })
            .collect();

        // The fork: a bare repository behind an authenticating server.
        let base = crate::config::data_dir();
        std::fs::create_dir_all(&base).unwrap();
        let seed = upstream(&base);
        let served = base.join("served");
        std::fs::create_dir_all(served.join("alice")).unwrap();
        let bare = served.join("alice/veles.git");
        git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(&bare, &["config", "http.receivepack", "true"]);
        let password = "fixture-helper-token-7781";
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("alice:{password}"))
        );
        let server = AuthGitServer::start(served.clone(), expected.clone());
        let helper = base.join("credential-helper");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\necho username=alice\necho password={password}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let global = base.join("author.gitconfig");
        std::fs::write(
            &global,
            format!(
                "[url \"http://127.0.0.1:{port}/\"]\n\tinsteadOf = https://github.com/\n[credential \"http://127.0.0.1:{port}\"]\n\thelper = !{}\n",
                helper.display(),
                port = server.port
            ),
        )
        .unwrap();
        // A fake gh, found on PATH (not through VELES_CONTRIBUTE_GH).
        let tools = base.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let gh_log = base.join("gh-args");
        std::fs::write(
            tools.join("gh"),
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf 'https://github.com/77787777778777/veles/pull/1\\n'\n",
                gh_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(tools.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["GIT_CONFIG_GLOBAL", "PATH", "VELES_CONTRIBUTE_GH"]
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect();
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        let mut path = std::ffi::OsString::from(tools.as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        std::env::set_var("PATH", &path);
        std::env::remove_var("VELES_CONTRIBUTE_GH");

        let fork = "https://github.com/alice/veles.git";
        let outcome = (|| -> Result<(Value, Value, Value)> {
            let prepared = prepare_many(
                &selections,
                fork,
                "all-my-extensions",
                DEFAULT_DIRECTORY,
                false,
            )?;
            let worktree = PathBuf::from(prepared["worktree"].as_str().unwrap());
            let published = publish(&worktree)?;
            let opened = pr(
                &worktree,
                DEFAULT_UPSTREAM_REPO,
                "master",
                "Contribute extensions: demo, rustx, tsx",
                "All my extensions.",
            )?;
            Ok((prepared, published, opened))
        })();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let (prepared, published, opened) = outcome.expect("prepare, push and PR");
        assert_eq!(prepared["origin"], fork);
        assert_eq!(published["pushed"], true);

        // One branch on the fork, every extension in its own directory,
        // nested files included, build output and secrets not.
        let tree = git(
            &bare,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                "refs/heads/all-my-extensions",
            ],
        );
        let tree = String::from_utf8_lossy(&tree.stdout).to_string();
        let mut want = vec![
            "README.md",
            "plugins/examples/demo/nested/helper.tcl",
            "plugins/examples/demo/plugin.tcl",
            "plugins/examples/demo/plugin.toml",
            "plugins/examples/rustx/Cargo.toml",
            "plugins/examples/rustx/plugin.toml",
            "plugins/examples/rustx/src/lib.rs",
            "plugins/examples/tsx/plugin.toml",
            "plugins/examples/tsx/plugin.ts",
            "plugins/examples/tsx/tsconfig.json",
            "plugins/examples/tsx/veles.d.ts",
        ];
        want.sort();
        let mut got: Vec<&str> = tree.lines().collect();
        got.sort();
        assert_eq!(got, want, "the fork's branch");
        let log = git(
            &bare,
            &["log", "--format=%s", "refs/heads/all-my-extensions"],
        );
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).lines().next(),
            Some("Add extension sources: demo, rustx, tsx (10 files)")
        );

        // The push carried the helper's credential: an unauthenticated
        // receive-pack was refused (401) and the authenticated one served.
        let seen = server.seen.lock().unwrap().clone();
        let receive: Vec<_> = seen
            .iter()
            .filter(|(req, _)| req.starts_with("POST") && req.contains("git-receive-pack"))
            .collect();
        assert!(
            receive
                .iter()
                .any(|(_, auth)| auth.as_deref() == Some(expected.as_str())),
            "the push was not authenticated with the configured credential: {seen:?}"
        );

        // gh, from PATH, named the fork owner in the head.
        let args = std::fs::read_to_string(&gh_log).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(
            args,
            [
                "pr",
                "create",
                "--repo",
                DEFAULT_UPSTREAM_REPO,
                "--base",
                "master",
                "--head",
                "alice:all-my-extensions",
                "--title",
                "Contribute extensions: demo, rustx, tsx",
                "--body",
                "All my extensions.",
            ]
        );
        assert_eq!(opened["head"], "alice:all-my-extensions");
        assert_eq!(
            opened["url"],
            "https://github.com/77787777778777/veles/pull/1"
        );
    }
}
