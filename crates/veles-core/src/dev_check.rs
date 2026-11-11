//! dev_check — the Dev pane's **Check**, for every extension language,
//! run on the gateway.
//!
//! Check used to be `exec tclsh tcl-lsp-check.tcl <path>` on the DESKTOP:
//! it read a gateway path from the desktop's own disk (wrong file, or no
//! file, against a remote gateway), wrote its scratch copy to `/tmp`
//! (absent on Windows, W6/dev-ext-13), and for anything but Tcl said
//! "no checker". It now asks the gateway, where the files and the tools
//! are, and the gateway runs the matching checker:
//!
//! | language    | checker                                  | evaluates? |
//! |-------------|------------------------------------------|------------|
//! | Tcl         | tcl-lsp diagnostics, else `info complete`| no         |
//! | TypeScript  | `tsc --noEmit` (the extension's tsconfig, else the host's `veles` declaration) | no |
//! | JavaScript  | `node --check`                           | no (parse only) |
//! | Rust        | `cargo check --message-format=json`      | compiles; build scripts and proc macros DO run |
//! | C / C++     | `clang -fsyntax-only` (or `cc`)          | no         |
//! | Racket      | `read-syntax` over the module            | only the `#lang` reader |
//! | Common Lisp | ECL's reader, `*read-eval*` off          | no         |
//!
//! `cargo check` is the one that runs code — the crate's and its
//! dependencies' build scripts and procedural macros. It is what an IDE
//! runs on save and what the owner named; the reply says it in `note`
//! rather than calling it non-evaluating. (Contribution, which must not
//! run code, does not use this module for Rust.)
//!
//! A missing checker is `unavailable`, with the program it looked for:
//! never a pass.

use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::lsp_servers::Language;

/// One finding, in the editor's coordinates: `line` 1-based, `col`
/// 0-based in characters (Tk text indices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: String,
    pub line: u64,
    pub col: u64,
    pub severity: &'static str,
    pub message: String,
}

impl Diagnostic {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "line": self.line,
            "col": self.col,
            "severity": self.severity,
            "message": self.message,
        })
    }
}

/// How the checker programs are found; tests pass their own.
pub struct Tools<'a> {
    pub which: &'a dyn Fn(&str) -> Option<PathBuf>,
    pub timeout: Duration,
    /// `--target-dir` for cargo check; None is cargo's own choice (the
    /// crate's `target/`, or the gateway's `CARGO_TARGET_DIR`). Tests set
    /// it so a fixture crate never builds into a shared target directory.
    pub cargo_target_dir: Option<&'a Path>,
    /// The engine this gateway runs for a script kind (`racket`, `lisp`),
    /// whose reader checks a file when no checker program is installed
    /// (SMOKE-23). Production: the registered script hosts.
    pub embedded: &'a dyn Fn(&str) -> Option<std::sync::Arc<dyn crate::scripthost::ScriptHost>>,
}

impl Tools<'_> {
    fn find(&self, names: &[&str]) -> Option<(String, PathBuf)> {
        names
            .iter()
            .find_map(|n| (self.which)(n).map(|p| (n.to_string(), p)))
    }
}

/// [`check_file_with`] with this gateway's own program lookup
/// ([`find_program`]).
pub fn check_file(path: &Path) -> Value {
    check_file_with(
        path,
        &Tools {
            which: &find_program,
            timeout: Duration::from_secs(300),
            cargo_target_dir: None,
            embedded: &crate::scripthost::host_for,
        },
    )
}

// ── finding the checker programs ──────────────────────────────────────

/// What a program lookup consults. Production reads this process's
/// ([`LookupEnv::current`]); tests pass their own, so the Windows rules
/// and an empty PATH are exercised on any host without touching the
/// process environment.
pub(crate) struct LookupEnv<'a> {
    /// `windows`, or any other `platform::OS` value.
    pub os: &'a str,
    /// The PATH value.
    pub path: Option<std::ffi::OsString>,
    /// Any other variable (`PATHEXT`, `VELES_TCLSH`, `VELES_TCL_LSP`,
    /// `TCL_LIBRARY`).
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// The running gateway's own directory.
    pub exe_dir: Option<PathBuf>,
}

fn process_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl LookupEnv<'static> {
    pub(crate) fn current() -> LookupEnv<'static> {
        LookupEnv {
            os: crate::platform::OS,
            path: std::env::var_os("PATH"),
            var: &process_var,
            exe_dir: std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
        }
    }
}

/// The file names `name` may have on `env.os`: on Windows the PATHEXT
/// spellings first (`tsc.cmd`, `tclsh.exe`), then the bare name.
fn spellings(name: &str, env: &LookupEnv) -> Vec<String> {
    if env.os != "windows" || Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let exts = (env.var)("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    let mut names: Vec<String> = exts
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| format!("{name}{}", e.to_ascii_lowercase()))
        .collect();
    names.push(name.to_string());
    names
}

/// Programs a veles build ships, each with the variable the build sets
/// to its own copy: the APK's tclsh (lib/libtclsh.so, Engine.java;
/// DEV-4) and the Linux packages' tcl-lsp (velesPkg's wrapper in
/// flake.nix; DEV-6). That copy is the one the build was made and tested
/// with, so [`find_program_in`] looks there before PATH.
const SHIPPED: &[(&str, &str)] = &[("tclsh", "VELES_TCLSH"), ("tcl-lsp", "VELES_TCL_LSP")];

/// This build's own copy of `name`, when it ships one and the file is
/// there. A variable naming a file that is gone is not an answer.
fn shipped_in(name: &str, env: &LookupEnv) -> Option<PathBuf> {
    let named = PathBuf::from((env.var)(shipped_var(name)?)?);
    named.is_file().then_some(named)
}

/// The program a checker runs, as a path to start: what `Command::new`
/// is given, so availability and the spawn can never disagree (DEV-5).
pub(crate) fn find_program_in(name: &str, env: &LookupEnv) -> Option<PathBuf> {
    let given = Path::new(name);
    if given.is_absolute() || given.components().count() > 1 {
        return given.is_file().then(|| given.to_path_buf());
    }
    // The build's own copy first ([`SHIPPED`]), as executecode::tclsh_path
    // finds tclsh: the APK names its tclsh only there (DEV-4), and the
    // Linux packages' tcl-lsp is on no PATH at all (DEV-6).
    if let Some(shipped) = shipped_in(name, env) {
        return Some(shipped);
    }
    let names = spellings(name, env);
    let in_dir = |dir: &Path| names.iter().map(|n| dir.join(n)).find(|c| c.is_file());
    if let Some(path) = &env.path {
        for dir in std::env::split_paths(path) {
            if let Some(hit) = in_dir(&dir) {
                return Some(hit);
            }
        }
    }
    // Beside the running gateway: the Windows zip's bin\ holds tclsh.exe
    // and wish.exe next to veles.exe, off PATH. The desktop's wish (and
    // Windows' own CreateProcess search) find them there.
    if let Some(hit) = env.exe_dir.as_deref().and_then(in_dir) {
        return Some(hit);
    }
    // A nix-built gateway run without its wrapper: TCL_LIBRARY is
    // <tcl>/lib/tcl8.6, so tclsh is <tcl>/bin/tclsh.
    if name.starts_with("tclsh") {
        if let Some(library) = (env.var)("TCL_LIBRARY") {
            if let Some(prefix) = Path::new(&library).parent().and_then(Path::parent) {
                if let Some(hit) = in_dir(&prefix.join("bin")) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

/// [`find_program_in`] for this process.
pub fn find_program(name: &str) -> Option<PathBuf> {
    find_program_in(name, &LookupEnv::current())
}

/// The variable a shipping build names its own copy of `name` in, if
/// it ships one ([`SHIPPED`]).
pub fn shipped_var(name: &str) -> Option<&'static str> {
    SHIPPED
        .iter()
        .find(|(program, _)| *program == name)
        .map(|(_, var)| *var)
}

/// Check one file with the checker for its language. Never errors: a
/// problem running the checker is a reply the editor shows.
pub fn check_file_with(path: &Path, tools: &Tools) -> Value {
    let shown = crate::lsp_servers::display_path(path);
    let Some(language) = Language::from_path(path) else {
        return json!({
            "path": shown,
            "status": "unavailable",
            "note": "no checker is known for this file type",
            "diagnostics": [],
        });
    };
    if !path.is_file() {
        return json!({
            "path": shown,
            "language": language.id(),
            "status": "failed",
            "note": "the file does not exist on the gateway (save it first)",
            "diagnostics": [],
        });
    }
    let result = match language {
        Language::Tcl => check_tcl(path, tools),
        Language::TypeScript => check_typescript(path, tools),
        Language::JavaScript => check_javascript(path, tools),
        Language::Rust => check_rust(path, tools),
        Language::C | Language::Cpp => check_c(path, language, tools),
        Language::Racket => check_racket(path, tools),
        Language::CommonLisp => check_lisp(path, tools),
    };
    let mut reply = match result {
        Ok(report) => report.into_json(),
        Err(Unavailable { looked_for, hint }) => json!({
            "status": "unavailable",
            "note": format!(
                "no {} checker is installed on the gateway (looked for {}); {}",
                language.display(),
                looked_for.join(", "),
                hint
            ),
            "diagnostics": [],
        }),
    };
    reply["path"] = json!(shown);
    reply["language"] = json!(language.id());
    reply
}

struct Unavailable {
    looked_for: Vec<&'static str>,
    hint: &'static str,
}

struct Report {
    checker: String,
    method: &'static str,
    passed: bool,
    diagnostics: Vec<Diagnostic>,
    output: String,
    note: Option<String>,
}

impl Report {
    fn into_json(self) -> Value {
        let errors = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == "ERROR")
            .count();
        let status = if self.passed && errors == 0 {
            "passed"
        } else {
            "failed"
        };
        let mut v = json!({
            "status": status,
            "checker": self.checker,
            "method": self.method,
            "diagnostics": self.diagnostics.iter().map(Diagnostic::to_json).collect::<Vec<_>>(),
            "output": truncate(&self.output, 16 * 1024),
        });
        if let Some(note) = self.note {
            v["note"] = json!(note);
        }
        v
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[checker output truncated]", &text[..end])
}

/// Run a checker to completion or to the deadline, never longer: a hung
/// compiler must not hold a gateway worker (the lsp_check lesson).
pub(crate) fn run(mut command: Command, timeout: Duration) -> Result<(bool, String), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start the checker: {e}"))?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the checker did not finish within {}s",
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("waiting for the checker: {e}")),
        }
    };
    let mut text = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    if !stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    Ok((status.success(), text))
}

fn severity(word: &str) -> &'static str {
    match word.to_ascii_lowercase().as_str() {
        "error" | "fatal error" | "fatal" => "ERROR",
        "warning" => "WARNING",
        "note" | "info" => "INFO",
        _ => "HINT",
    }
}

/// A byte column (clang and tcl-lsp count bytes) as a character column
/// on `line` (1-based) of `text`.
pub(crate) fn byte_col_to_char(text: &str, line: u64, byte_col: u64) -> u64 {
    let Some(l) = text.split('\n').nth(line.saturating_sub(1) as usize) else {
        return byte_col;
    };
    let mut bytes = 0u64;
    for (chars, ch) in l.chars().enumerate() {
        if bytes >= byte_col {
            return chars as u64;
        }
        bytes += ch.len_utf8() as u64;
    }
    l.chars().count() as u64
}

// ── Tcl ───────────────────────────────────────────────────────────────

/// The completeness check contribution uses, as a script file (see
/// contribute.rs `TCL_COMPLETE_CHECK` for why it is a file).
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

fn check_tcl(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    // The server the lookup found is the one started (DEV-5).
    if let Some((_, server)) = tools.find(&["tcl-lsp"]) {
        let file = path.to_string_lossy();
        match crate::lsp_check::lint_rows(
            &server.to_string_lossy(),
            &file,
            tools.timeout.as_secs().min(60),
        ) {
            Ok(rows) => {
                let diagnostics = rows
                    .into_iter()
                    .map(|(line, col, severity, message)| Diagnostic {
                        path: file.to_string(),
                        line,
                        col,
                        severity,
                        message,
                    })
                    .collect::<Vec<_>>();
                return Ok(Report {
                    checker: "tcl-lsp".into(),
                    method: "tcl-lsp diagnostics",
                    passed: true,
                    diagnostics,
                    output: String::new(),
                    note: None,
                });
            }
            Err(e) => {
                // Fall through to the completeness check, and say so.
                return check_tcl_complete(path, tools)
                    .map(|mut r| {
                        r.note = Some(format!("tcl-lsp failed ({e}); checked completeness only"));
                        r
                    })
                    .or(Ok(Report {
                        checker: "tcl-lsp".into(),
                        method: "tcl-lsp diagnostics",
                        passed: false,
                        diagnostics: vec![],
                        output: e,
                        note: None,
                    }));
            }
        }
    }
    check_tcl_complete(path, tools)
}

fn check_tcl_complete(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    let Some((name, tclsh)) = tools.find(&["tclsh", "tclsh8.6", "tclsh9.0"]) else {
        return Err(Unavailable {
            looked_for: vec!["tcl-lsp", "tclsh"],
            hint: "install tcl-lsp (diagnostics) or Tcl (completeness only)",
        });
    };
    let scratch = tempfile::tempdir().map_err(|_| Unavailable {
        looked_for: vec!["a scratch directory"],
        hint: "the gateway could not create a temporary directory",
    })?;
    let script = scratch.path().join("complete.tcl");
    let _ = std::fs::write(&script, TCL_COMPLETE_CHECK);
    let mut c = Command::new(tclsh);
    c.arg(&script).arg(path);
    let (ok, output) = run(c, tools.timeout).unwrap_or_else(|e| (false, e));
    let diagnostics = if ok {
        vec![]
    } else {
        vec![Diagnostic {
            path: path.to_string_lossy().into_owned(),
            line: 1,
            col: 0,
            severity: "ERROR",
            message: output.trim().to_string(),
        }]
    };
    Ok(Report {
        checker: name,
        method: "Tcl info complete",
        passed: ok,
        diagnostics,
        output,
        note: Some("tcl-lsp is not installed on the gateway: completeness only, no lint".into()),
    })
}

// ── TypeScript / JavaScript ──────────────────────────────────────────

/// A diagnostic path crosses from the gateway to the editor as `/`-joined
/// text, even when the gateway itself is Windows. Keep verbatim prefixes
/// simplified but do not make the client understand host-only separators.
fn diagnostic_path_for_os(path: &Path, os: &str) -> String {
    crate::lsp_servers::simplify_verbatim(&path.to_string_lossy(), os).replace('\\', "/")
}

fn diagnostic_path(path: &Path) -> String {
    diagnostic_path_for_os(path, crate::platform::OS)
}

/// `file(line,col): error TS2304: message` — tsc's `--pretty false` form.
fn parse_tsc(output: &str, base: &Path) -> Vec<Diagnostic> {
    let re = regex::Regex::new(r"^(.+?)\((\d+),(\d+)\): (error|warning|message) (TS\d+: .*)$")
        .expect("tsc pattern");
    output
        .lines()
        .filter_map(|l| re.captures(l.trim_end()))
        .map(|c| {
            let file = PathBuf::from(&c[1]);
            let file = if file.is_absolute() {
                file
            } else {
                base.join(file)
            };
            Diagnostic {
                path: diagnostic_path(&file),
                line: c[2].parse().unwrap_or(1),
                col: c[3].parse::<u64>().unwrap_or(1).saturating_sub(1),
                severity: severity(&c[4]),
                message: c[5].to_string(),
            }
        })
        .collect()
}

/// tsc for one extension file: its tsconfig when the extension has one,
/// else the host's declaration and module settings on the command line.
/// `scratch` must outlive the command (it holds the declaration file).
pub(crate) fn tsc_command(tsc: &Path, file: &Path, scratch: &Path) -> (Command, PathBuf) {
    let root = crate::lsp_servers::project_root(Language::TypeScript, file);
    let mut c = Command::new(tsc);
    if root.join("tsconfig.json").is_file() {
        c.args(["--noEmit", "--pretty", "false", "-p"]).arg(&root);
        c.current_dir(&root);
        return (c, root);
    }
    let ambient = scratch.join("veles-host.d.ts");
    let _ = std::fs::write(&ambient, crate::plugins::VELES_TS_DECLARATION);
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
        "--lib",
        "es2022",
    ]);
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // A declaration file that IS the host declaration would redeclare it.
    if !name.ends_with(".d.ts") {
        c.arg(&ambient);
    }
    c.arg(file);
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    c.current_dir(&dir);
    (c, dir)
}

fn check_typescript(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    let Some((name, tsc)) = tools.find(&["tsc"]) else {
        return Err(Unavailable {
            looked_for: vec!["tsc"],
            hint: "install typescript on the gateway (npm install -g typescript); TypeScript is never checked as JavaScript",
        });
    };
    let scratch = tempfile::tempdir().map_err(|_| Unavailable {
        looked_for: vec!["a scratch directory"],
        hint: "the gateway could not create a temporary directory",
    })?;
    let (command, base) = tsc_command(&tsc, path, scratch.path());
    let project = base.join("tsconfig.json").is_file();
    let (ok, output) = run(command, tools.timeout).unwrap_or_else(|e| (false, e));
    let diagnostics = parse_tsc(&output, &base);
    Ok(Report {
        checker: name,
        method: if project {
            "tsc --noEmit -p (the extension's tsconfig.json)"
        } else {
            "tsc --noEmit (host declaration, ES module)"
        },
        passed: ok,
        diagnostics,
        output,
        note: None,
    })
}

/// node's syntax error report: `<file>:<line>` on the first line, the
/// `SyntaxError: …` line further down.
fn parse_node(output: &str, file: &Path) -> Vec<Diagnostic> {
    let mut line = None;
    let mut caret = None;
    let mut message = None;
    for (i, l) in output.lines().enumerate() {
        if line.is_none() {
            if let Some((_, n)) = l.rsplit_once(':') {
                if let Ok(n) = n.trim().parse::<u64>() {
                    line = Some(n);
                    continue;
                }
            }
        }
        if caret.is_none() && i > 0 && l.trim_start().starts_with('^') {
            caret = Some((l.len() - l.trim_start().len()) as u64);
        }
        if message.is_none() && (l.contains("Error:") || l.starts_with("SyntaxError")) {
            message = Some(l.trim().to_string());
        }
    }
    match (line, message) {
        (Some(line), Some(message)) => vec![Diagnostic {
            path: diagnostic_path(file),
            line,
            col: caret.unwrap_or(0),
            severity: "ERROR",
            message,
        }],
        _ => vec![],
    }
}

fn check_javascript(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    let Some((name, node)) = tools.find(&["node"]) else {
        return Err(Unavailable {
            looked_for: vec!["node"],
            hint: "install Node.js on the gateway for a syntax check",
        });
    };
    let mut c = Command::new(node);
    c.arg("--check").arg(path);
    let (ok, output) = run(c, tools.timeout).unwrap_or_else(|e| (false, e));
    let diagnostics = if ok {
        vec![]
    } else {
        parse_node(&output, path)
    };
    Ok(Report {
        checker: name,
        method: "node --check (syntax only; the module is not run)",
        passed: ok,
        diagnostics,
        output,
        note: None,
    })
}

// ── Rust ──────────────────────────────────────────────────────────────

/// `cargo check --message-format=json` lines → diagnostics on their
/// primary spans. Paths in spans are relative to the workspace root.
fn parse_cargo(output: &str, workspace: &Path) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in output.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let msg = &v["message"];
        let level = msg["level"].as_str().unwrap_or("");
        if level.starts_with("failure-note") {
            continue;
        }
        let Some(span) = msg["spans"]
            .as_array()
            .and_then(|spans| spans.iter().find(|s| s["is_primary"] == true))
        else {
            continue;
        };
        let file = PathBuf::from(span["file_name"].as_str().unwrap_or(""));
        let file = if file.is_absolute() {
            file
        } else {
            workspace.join(file)
        };
        let code = msg["code"]["code"]
            .as_str()
            .map(|c| format!("[{c}] "))
            .unwrap_or_default();
        out.push(Diagnostic {
            path: diagnostic_path(&file),
            line: span["line_start"].as_u64().unwrap_or(1),
            // rustc's columns are 1-based and count characters.
            col: span["column_start"].as_u64().unwrap_or(1).saturating_sub(1),
            severity: severity(if level == "error: internal compiler error" {
                "error"
            } else {
                level
            }),
            message: format!("{code}{}", msg["message"].as_str().unwrap_or("")),
        });
    }
    out
}

fn check_rust(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    let Some((name, cargo)) = tools.find(&["cargo"]) else {
        return Err(Unavailable {
            looked_for: vec!["cargo"],
            hint: "install a Rust toolchain on the gateway (rustup)",
        });
    };
    let start = path.parent().unwrap_or(Path::new("."));
    let Some(package) = start
        .ancestors()
        .find(|d| d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
    else {
        return Ok(Report {
            checker: name,
            method: "cargo check",
            passed: false,
            diagnostics: vec![],
            output: String::new(),
            note: Some(
                "this .rs file is not inside a cargo package (no Cargo.toml above it)".into(),
            ),
        });
    };
    let workspace = crate::lsp_servers::project_root(Language::Rust, path);
    let mut c = Command::new(cargo);
    c.args([
        "check",
        "--message-format=json",
        "--color",
        "never",
        "--manifest-path",
    ])
    .arg(package.join("Cargo.toml"))
    .current_dir(&workspace)
    .env("CARGO_TERM_COLOR", "never");
    if let Some(target) = tools.cargo_target_dir {
        c.arg("--target-dir").arg(target);
    }
    let (ok, output) = run(c, tools.timeout).unwrap_or_else(|e| (false, e));
    let diagnostics = parse_cargo(&output, &workspace);
    // The JSON stream is for this parser; the human part is cargo's stderr.
    let human = output
        .lines()
        .filter(|l| !l.starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Report {
        checker: name,
        method: "cargo check",
        passed: ok,
        diagnostics,
        output: human,
        note: Some(
            "cargo check compiles without producing the plugin; the crate's and its dependencies' build scripts and procedural macros do run".into(),
        ),
    })
}

// ── C / C++ ───────────────────────────────────────────────────────────

fn parse_clang(output: &str, file: &Path, text: &str) -> Vec<Diagnostic> {
    let re = regex::Regex::new(r"^(.+?):(\d+):(\d+): (fatal error|error|warning|note): (.*)$")
        .expect("clang pattern");
    let same = |p: &Path| {
        p == file
            || p.canonicalize().ok() == file.canonicalize().ok()
            || p.file_name() == file.file_name() && p.is_relative()
    };
    output
        .lines()
        .filter_map(|l| re.captures(l))
        .map(|c| {
            let p = PathBuf::from(&c[1]);
            let line: u64 = c[2].parse().unwrap_or(1);
            let byte_col = c[3].parse::<u64>().unwrap_or(1).saturating_sub(1);
            let col = if same(&p) {
                byte_col_to_char(text, line, byte_col)
            } else {
                byte_col
            };
            Diagnostic {
                path: if same(&p) {
                    crate::lsp_servers::display_path(file)
                } else {
                    crate::lsp_servers::display_path(&p)
                },
                line,
                col,
                severity: severity(&c[4]),
                message: c[5].to_string(),
            }
        })
        .collect()
}

fn check_c(path: &Path, language: Language, tools: &Tools) -> Result<Report, Unavailable> {
    let names: &[&str] = if language == Language::Cpp {
        &["clang++", "c++", "g++"]
    } else {
        &["clang", "cc", "gcc"]
    };
    let Some((name, compiler)) = tools.find(names) else {
        return Err(Unavailable {
            looked_for: names.to_vec(),
            hint: "install clang or gcc on the gateway",
        });
    };
    let root = crate::lsp_servers::project_root(language, path);
    let mut c = Command::new(compiler);
    // Flags clang and gcc both take; the caret lines that follow a
    // diagnostic simply do not match the parser.
    c.args(["-fsyntax-only", "-fdiagnostics-color=never"]);
    // The flags clangd would use, when the extension wrote them down.
    if let Ok(flags) = std::fs::read_to_string(root.join("compile_flags.txt")) {
        c.args(flags.lines().map(str::trim).filter(|l| !l.is_empty()));
    }
    if let Some(dir) = path.parent() {
        c.arg("-I").arg(dir);
    }
    c.arg("-I").arg(&root).arg(path).current_dir(&root);
    let (ok, output) = run(c, tools.timeout).unwrap_or_else(|e| (false, e));
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let diagnostics = parse_clang(&output, path, &text);
    Ok(Report {
        checker: name,
        method: "-fsyntax-only",
        passed: ok,
        diagnostics,
        output,
        note: None,
    })
}

// ── the engine the gateway runs ───────────────────────────────────────

/// The read check of the engine this gateway runs for `kind`, when no
/// checker program is installed. The lang-racket and lang-lisp modules
/// embed the very reader the external check starts, and Check said "no
/// Racket checker is installed" on a gateway whose log had just said
/// "Racket CS 9.3 booted" (SMOKE-23). Nothing is expanded or evaluated.
/// `None` when the gateway runs no such engine (or this thread cannot
/// wait for it).
fn check_embedded(kind: &str, engine: &str, path: &Path, tools: &Tools) -> Option<Report> {
    let host = (tools.embedded)(kind)?;
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let read = host.read_check(path.to_path_buf())?;
    let answer = handle.block_on(tokio::time::timeout(tools.timeout, read));
    let (passed, diagnostics, output) = match answer {
        Ok(Ok(None)) => (true, Vec::new(), String::new()),
        Ok(Ok(Some(e))) => {
            let line = format!(
                "{}:{}:{}: error: {}",
                path.display(),
                e.line,
                e.col,
                e.message
            );
            let d = Diagnostic {
                path: diagnostic_path(path),
                line: e.line,
                col: e.col,
                severity: "ERROR",
                message: e.message,
            };
            (false, vec![d], line)
        }
        Ok(Err(e)) => (false, Vec::new(), e),
        Err(_) => (
            false,
            Vec::new(),
            format!(
                "the gateway's {engine} engine did not answer within {}s",
                tools.timeout.as_secs()
            ),
        ),
    };
    Some(Report {
        checker: format!("the gateway's embedded {engine} (lang-{kind})"),
        method: "the language's own reader in the running engine (nothing is expanded or run)",
        passed,
        diagnostics,
        output,
        note: Some(format!(
            "reads the file only: the lang-{kind} engine this gateway runs did the reading"
        )),
    })
}

// ── Racket ────────────────────────────────────────────────────────────

/// Read every form with `read-syntax`; nothing is expanded or run. The
/// `#lang` line does load that language's READER (racket/base's for the
/// scaffold), which is the one piece of code reading involves.
const RACKET_READ_CHECK: &str = r#"#lang racket/base
(define path (vector-ref (current-command-line-arguments) 0))
(define (fail e)
  (define-values (line col)
    (if (exn:fail:read? e)
        (let ([locs (exn:fail:read-srclocs e)])
          (if (pair? locs)
              (values (or (srcloc-line (car locs)) 1) (or (srcloc-column (car locs)) 0))
              (values 1 0)))
        (values 1 0)))
  (eprintf "~a:~a:~a: error: ~a\n" path line col (exn-message e))
  (exit 1))
(with-handlers ([exn:fail? fail])
  (parameterize ([read-accept-reader #t] [read-accept-lang #t])
    (call-with-input-file path
      (lambda (in)
        (port-count-lines! in)
        (let loop ()
          (unless (eof-object? (read-syntax path in))
            (loop)))))))
"#;

fn parse_reader_errors(output: &str, file: &Path) -> Vec<Diagnostic> {
    let re = regex::Regex::new(r"^(.*?):(\d+):(\d+): error: (.*)$").expect("reader pattern");
    output
        .lines()
        .filter_map(|l| re.captures(l))
        .map(|c| Diagnostic {
            path: diagnostic_path(file),
            line: c[2].parse().unwrap_or(1),
            col: c[3].parse().unwrap_or(0),
            severity: "ERROR",
            message: c[4].to_string(),
        })
        .collect()
}

fn check_racket(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    let Some((name, racket)) = tools.find(&["racket"]) else {
        if let Some(report) = check_embedded("racket", "Racket", path, tools) {
            return Ok(report);
        }
        return Err(Unavailable {
            looked_for: vec!["racket"],
            hint: "install Racket on the gateway for a read check",
        });
    };
    let scratch = tempfile::tempdir().map_err(|_| Unavailable {
        looked_for: vec!["a scratch directory"],
        hint: "the gateway could not create a temporary directory",
    })?;
    let script = scratch.path().join("read-check.rkt");
    let _ = std::fs::write(&script, RACKET_READ_CHECK);
    let mut c = Command::new(racket);
    c.arg(&script).arg(path);
    let (ok, output) = run(c, tools.timeout).unwrap_or_else(|e| (false, e));
    let diagnostics = parse_reader_errors(&output, path);
    Ok(Report {
        checker: name,
        method: "read-syntax (the module is not expanded or run)",
        passed: ok,
        diagnostics,
        output,
        note: Some(
            "reads the module only: unbound identifiers and the veles collection are not checked"
                .into(),
        ),
    })
}

// ── Common Lisp ───────────────────────────────────────────────────────

/// Read every form with `*read-eval*` off (no `#.`), in a scratch
/// package that uses COMMON-LISP — the shape of the package the lang-lisp
/// host loads a plugin into, minus VELES, whose symbols are then simply
/// interned. A reader error names the file position; the line is
/// counted from it.
const LISP_READ_CHECK: &str = r#"
(defpackage :veles-read-check (:use :common-lisp))
(in-package :veles-read-check)
(let* ((args (ext:command-args))
       (path (car (last args)))
       (*read-eval* nil)
       (*package* (find-package :veles-read-check)))
  (with-open-file (in path :external-format :utf-8)
    (handler-case
        (loop for form = (read in nil in)
              until (eq form in))
      (error (e)
        (let* ((pos (or (ignore-errors (file-position in)) 0))
               (line 1) (col 0))
          (with-open-file (again path :external-format :utf-8)
            (loop repeat pos
                  for ch = (read-char again nil nil)
                  while ch
                  do (if (char= ch #\Newline) (setf line (1+ line) col 0) (incf col))))
          (format *error-output* "~a:~a:~a: error: ~a~%" path line col
                  (substitute #\Space #\Newline (princ-to-string e)))
          (ext:quit 1)))))
  (ext:quit 0))
"#;

fn check_lisp(path: &Path, tools: &Tools) -> Result<Report, Unavailable> {
    let Some((name, ecl)) = tools.find(&["ecl"]) else {
        if let Some(report) = check_embedded("lisp", "Common Lisp (ECL)", path, tools) {
            return Ok(report);
        }
        return Err(Unavailable {
            looked_for: vec!["ecl"],
            hint: "install ECL (the Common Lisp the lang-lisp host embeds) on the gateway for a read check",
        });
    };
    let scratch = tempfile::tempdir().map_err(|_| Unavailable {
        looked_for: vec!["a scratch directory"],
        hint: "the gateway could not create a temporary directory",
    })?;
    let script = scratch.path().join("read-check.lisp");
    let _ = std::fs::write(&script, LISP_READ_CHECK);
    let mut c = Command::new(ecl);
    c.args(["--norc", "--shell"]).arg(&script).arg(path);
    let (ok, output) = run(c, tools.timeout).unwrap_or_else(|e| (false, e));
    let diagnostics = parse_reader_errors(&output, path);
    Ok(Report {
        checker: name,
        method: "the Lisp reader, *read-eval* off (nothing is evaluated)",
        passed: ok,
        diagnostics,
        output,
        note: Some(
            "reads the file only: undefined functions and VELES symbols are not checked".into(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tools as the gateway finds them (find_program: the build's own
    /// copy, then PATH) plus `VELES_TEST_LSP_PATH` (the devShell's extra
    /// tool directories). `VELES_TEST_LSP_REQUIRE` names the tools whose
    /// absence is a failure, not a skip — so a devShell run cannot go
    /// green by not having them.
    fn lookup(name: &str) -> Option<PathBuf> {
        if let Some(extra) = std::env::var_os("VELES_TEST_LSP_PATH") {
            for dir in std::env::split_paths(&extra) {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        find_program(name)
    }

    fn need(name: &str) -> bool {
        if lookup(name).is_some() {
            return true;
        }
        let required = std::env::var("VELES_TEST_LSP_REQUIRE").unwrap_or_default();
        assert!(
            !required.split(',').any(|r| r.trim() == name),
            "{name} is required by VELES_TEST_LSP_REQUIRE but is not on PATH or VELES_TEST_LSP_PATH"
        );
        eprintln!("SKIP: {name} is not installed");
        false
    }

    fn tools() -> Tools<'static> {
        Tools {
            which: &lookup,
            timeout: Duration::from_secs(240),
            cargo_target_dir: None,
            embedded: &|_| None,
        }
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn diag_lines(v: &Value) -> Vec<u64> {
        v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["line"].as_u64().unwrap())
            .collect()
    }

    /// DEV-4: the shipped tclsh is off PATH on two platforms. Android
    /// names it only in VELES_TCLSH (lib/libtclsh.so); the Windows zip
    /// puts tclsh.exe in bin\ beside veles.exe without adding bin\ to
    /// PATH; a nix-built gateway run bare finds it from TCL_LIBRARY. The
    /// engine's own locator (executecode::tclsh_path) and the desktop's
    /// wish lookup look there; Check looked on PATH only and said
    /// "unavailable".
    #[test]
    fn the_checker_lookup_finds_tclsh_where_the_shipped_builds_put_it() {
        let dir = tempfile::tempdir().unwrap();
        let android = write(dir.path(), "apk/lib/libtclsh.so", "fixture");
        let zip = dir.path().join("zip/bin");
        write(&zip, "tclsh.exe", "fixture");
        let nix = dir.path().join("tcl-8.6");
        write(&nix, "bin/tclsh", "fixture");
        std::fs::create_dir_all(nix.join("lib/tcl8.6")).unwrap();
        let empty = Some(std::ffi::OsString::new());
        let named = android.display().to_string();
        let var = |k: &str| (k == "VELES_TCLSH").then(|| named.clone());
        let env = LookupEnv {
            os: crate::platform::OS,
            path: empty.clone(),
            var: &var,
            exe_dir: None,
        };
        assert_eq!(find_program_in("tclsh", &env), Some(android.clone()));
        // VELES_TCLSH names tclsh, not every program.
        assert_eq!(find_program_in("node", &env), None);
        let none = |_: &str| None;
        let env = LookupEnv {
            os: "windows",
            path: empty.clone(),
            var: &none,
            exe_dir: Some(zip.clone()),
        };
        assert_eq!(find_program_in("tclsh", &env), Some(zip.join("tclsh.exe")));
        let library = nix.join("lib/tcl8.6").display().to_string();
        let var = |k: &str| (k == "TCL_LIBRARY").then(|| library.clone());
        let env = LookupEnv {
            os: "linux",
            path: empty,
            var: &var,
            exe_dir: None,
        };
        assert_eq!(find_program_in("tclsh", &env), Some(nix.join("bin/tclsh")));
    }

    /// DEV-6: the Linux packages ship tcl-lsp and name it in
    /// VELES_TCL_LSP, as the APK names its tclsh in VELES_TCLSH. The
    /// build's own copy is the one it was built and tested with, so it
    /// wins over one on PATH; PATH is second, and a name whose file is
    /// gone falls through to it instead of being returned.
    #[test]
    fn the_shipped_tcl_lsp_is_found_before_the_one_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = write(dir.path(), "veles/bin/tcl-lsp", "fixture");
        let on_path = dir.path().join("path");
        write(&on_path, "tcl-lsp", "fixture");
        let search = Some(on_path.clone().into_os_string());
        let named = shipped.display().to_string();
        let gone = dir.path().join("removed/tcl-lsp").display().to_string();
        let shipping = |k: &str| (k == "VELES_TCL_LSP").then(|| named.clone());
        let stale = |k: &str| (k == "VELES_TCL_LSP").then(|| gone.clone());
        let none = |_: &str| None;
        let find = |name: &str, var: &dyn Fn(&str) -> Option<String>, path| {
            find_program_in(
                name,
                &LookupEnv {
                    os: "linux",
                    path,
                    var,
                    exe_dir: None,
                },
            )
        };
        assert_eq!(
            find("tcl-lsp", &shipping, search.clone()),
            Some(shipped.clone())
        );
        // VELES_TCL_LSP names tcl-lsp, not tclsh.
        assert_eq!(find("tclsh", &shipping, None), None);
        assert_eq!(
            find("tcl-lsp", &none, search.clone()),
            Some(on_path.join("tcl-lsp"))
        );
        assert_eq!(
            find("tcl-lsp", &stale, search),
            Some(on_path.join("tcl-lsp"))
        );
        assert_eq!(find("tcl-lsp", &none, None), None);
    }

    /// DEV-4, end to end: Check on a .tcl file with tclsh reachable only
    /// through VELES_TCLSH (no PATH at all) runs the completeness check.
    #[test]
    fn tcl_check_finds_the_engine_tclsh_off_path() {
        let Some(real) = lookup("tclsh").or_else(|| {
            let p = PathBuf::from(crate::executecode::tclsh_path());
            p.is_file().then_some(p)
        }) else {
            eprintln!("SKIP: no tclsh on this host");
            return;
        };
        let shown = real.display().to_string();
        let var = |k: &str| (k == "VELES_TCLSH").then(|| shown.clone());
        let env = LookupEnv {
            os: crate::platform::OS,
            path: Some(std::ffi::OsString::new()),
            var: &var,
            exe_dir: None,
        };
        let find = |name: &str| find_program_in(name, &env);
        let tools = Tools {
            which: &find,
            timeout: Duration::from_secs(60),
            cargo_target_dir: None,
            embedded: &|_| None,
        };
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), "plugin.tcl", "proc demo {} {return 1}\n");
        let v = check_file_with(&good, &tools);
        assert_eq!(v["status"], "passed", "{v}");
        assert_eq!(v["method"], "Tcl info complete", "{v}");
        let bad = write(dir.path(), "bad.tcl", "proc demo {} {\n");
        let v = check_file_with(&bad, &tools);
        assert_eq!(v["status"], "failed", "{v}");
    }

    /// SMOKE-23: a gateway with no `racket` or `ecl` program but with the
    /// lang-racket and lang-lisp engines running said "no Racket checker
    /// is installed". The engine's own reader checks the file now, and
    /// its error is a diagnostic. (With neither, the case below holds.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_engine_the_gateway_runs_reads_when_no_checker_program_is_installed() {
        use crate::scripthost::{BoxFut, ReaderError, ScriptHost, ScriptRef};
        use std::sync::Arc;
        struct Reader(&'static str);
        impl ScriptHost for Reader {
            fn kind(&self) -> &'static str {
                self.0
            }
            fn load(&self, _: ScriptRef) -> BoxFut<'_, Result<String, String>> {
                Box::pin(async { Err("not in this test".to_string()) })
            }
            fn unload(&self, _: &str) -> BoxFut<'_, Result<String, String>> {
                Box::pin(async { Ok(String::new()) })
            }
            fn invoke(&self, _: &str, _: u64, _: String) -> BoxFut<'_, Result<String, String>> {
                Box::pin(async { Err("not in this test".to_string()) })
            }
            fn read_check(
                &self,
                path: PathBuf,
            ) -> Option<BoxFut<'_, Result<Option<ReaderError>, String>>> {
                Some(Box::pin(async move {
                    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                    Ok(text.contains("(oops").then(|| ReaderError {
                        line: 2,
                        col: 0,
                        message: "expected a `)` to close `(`".to_string(),
                    }))
                }))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "bad.rkt", "#lang racket/base\n(oops\n");
        let good = write(dir.path(), "good.lisp", "(defun ok () 1)\n");
        let check = |path: PathBuf| {
            crate::rt::spawn_blocking(move || {
                let engines = |kind: &str| -> Option<Arc<dyn ScriptHost>> {
                    match kind {
                        "racket" => Some(Arc::new(Reader("racket"))),
                        "lisp" => Some(Arc::new(Reader("lisp"))),
                        _ => None,
                    }
                };
                check_file_with(
                    &path,
                    &Tools {
                        which: &|_| None,
                        timeout: Duration::from_secs(10),
                        cargo_target_dir: None,
                        embedded: &engines,
                    },
                )
            })
        };
        let v = check(bad).await.unwrap();
        assert_eq!(v["status"], "failed", "{v}");
        assert_eq!(diag_lines(&v), vec![2], "{v}");
        assert!(
            v["checker"]
                .as_str()
                .unwrap_or("")
                .contains("embedded Racket"),
            "{v}"
        );
        let v = check(good).await.unwrap();
        assert_eq!(v["status"], "passed", "{v}");
        assert!(
            v["checker"]
                .as_str()
                .unwrap_or("")
                .contains("embedded Common Lisp"),
            "{v}"
        );
    }

    #[test]
    fn a_missing_checker_is_unavailable_and_never_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        let none = Tools {
            which: &|_| None,
            timeout: Duration::from_secs(5),
            cargo_target_dir: None,
            embedded: &|_| None,
        };
        for (file, program) in [
            ("a.ts", "tsc"),
            ("a.js", "node"),
            ("src/lib.rs", "cargo"),
            ("a.c", "clang"),
            ("a.rkt", "racket"),
            ("a.lisp", "ecl"),
            ("a.tcl", "tcl-lsp"),
        ] {
            let p = write(dir.path(), file, "x\n");
            let v = check_file_with(&p, &none);
            assert_eq!(v["status"], "unavailable", "{file}: {v}");
            assert!(
                v["note"].as_str().unwrap().contains(program),
                "{file} must name {program}: {v}"
            );
            assert!(v["note"].as_str().unwrap().contains("on the gateway"));
        }
        let v = check_file_with(&write(dir.path(), "notes.md", "x\n"), &none);
        assert_eq!(v["status"], "unavailable");
    }

    #[test]
    fn windows_diagnostic_paths_keep_forward_slashes_and_drop_verbatim_prefixes() {
        assert_eq!(
            diagnostic_path_for_os(Path::new(r"\\?\C:\ext\plugin.ts"), "windows"),
            "C:/ext/plugin.ts"
        );
        let base = Path::new(if cfg!(windows) { r"C:\ext" } else { "/ext" });
        let d = parse_tsc(
            "plugin.ts(3,7): error TS2304: Cannot find name 'velez'.\n",
            base,
        );
        assert_eq!(
            d[0].path,
            if cfg!(windows) {
                "C:/ext/plugin.ts"
            } else {
                "/ext/plugin.ts"
            }
        );
        assert!(!d[0].path.contains('\\'));
    }

    #[test]
    fn tsc_c_and_cargo_output_parse_into_editor_coordinates() {
        let base = Path::new("/ext");
        let d = parse_tsc(
            "plugin.ts(3,7): error TS2304: Cannot find name 'velez'.\nnoise\n",
            base,
        );
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].line, d[0].col, d[0].severity), (3, 6, "ERROR"));
        assert_eq!(d[0].path, "/ext/plugin.ts");
        assert!(d[0].message.starts_with("TS2304"));
        // clang counts BYTES: "é" is two, so byte column 12 is char 11.
        let text = "int x;\nint é = y;\n";
        let d = parse_clang(
            "/ext/a.c:2:11: error: use of undeclared identifier 'y'\n",
            Path::new("/ext/a.c"),
            text,
        );
        assert_eq!((d[0].line, d[0].col), (2, 9), "{d:?}");
        let cargo = r#"{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `y`","code":{"code":"E0425"},"spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":4,"column_start":5}]}}
{"reason":"build-finished","success":false}"#;
        let d = parse_cargo(cargo, Path::new("/ws"));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "/ws/src/lib.rs");
        assert_eq!((d[0].line, d[0].col), (4, 4));
        assert_eq!(d[0].message, "[E0425] cannot find value `y`");
    }

    #[test]
    fn tcl_check_lints_on_the_gateway_with_tcl_lsp() {
        if !need("tcl-lsp") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), "good.tcl", "set x 1\nputs $x\n");
        let v = check_file_with(&good, &tools());
        assert_eq!(v["status"], "passed", "{v}");
        assert_eq!(v["checker"], "tcl-lsp");
        let bad = write(dir.path(), "bad.tcl", "proc f {a {\n  return $a\n}\n");
        let v = check_file_with(&bad, &tools());
        assert_eq!(v["status"], "failed", "{v}");
        assert!(!v["diagnostics"].as_array().unwrap().is_empty(), "{v}");
        // Pitfall 105 on the Check path: tcl-lsp puts the unterminated
        // `{` after "éé€" at BYTE 29; the editor's column is character 25.
        let wide = write(dir.path(), "wide.tcl", "set s \"éé€\"; proc broken {a {\n");
        let v = check_file_with(&wide, &tools());
        let cols: Vec<u64> = v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["message"].as_str().unwrap_or("").contains("unterminated"))
            .map(|d| d["col"].as_u64().unwrap())
            .collect();
        assert_eq!(cols, vec![25], "{v}");
    }

    #[test]
    fn the_typescript_scaffold_checks_clean_with_and_without_its_tsconfig() {
        if !need("tsc") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let files = crate::plugins::scaffold_files("demo", "", "js").unwrap();
        let ext = dir.path().join("demo");
        for f in &files {
            write(&ext, &f.name, &f.text);
        }
        assert!(ext.join("tsconfig.json").is_file() && ext.join("veles.d.ts").is_file());
        let v = check_file_with(&ext.join("plugin.ts"), &tools());
        assert_eq!(v["status"], "passed", "{v}");
        assert!(v["method"].as_str().unwrap().contains("tsconfig"), "{v}");
        // An extension from before 0.2.0: no tsconfig, no declaration.
        let old = dir.path().join("old");
        write(&old, "plugin.toml", "name = \"old\"\n");
        let entry = files.iter().find(|f| f.name == "plugin.ts").unwrap();
        let p = write(&old, "plugin.ts", &entry.text);
        let v = check_file_with(&p, &tools());
        assert_eq!(v["status"], "passed", "{v}");
        // A real type error is found and placed.
        let p = write(
            &old,
            "plugin.ts",
            "const n: number = \"x\";\nawait veles.nope();\n",
        );
        let v = check_file_with(&p, &tools());
        assert_eq!(v["status"], "failed", "{v}");
        assert_eq!(diag_lines(&v), vec![1, 2], "{v}");
    }

    #[test]
    fn a_c_plugin_is_syntax_checked_without_being_built() {
        if !need("clang") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let good = write(
            dir.path(),
            "plugin.c",
            "int veles_plugin_abi_version(void) { return 4; }\n",
        );
        let v = check_file_with(&good, &tools());
        assert_eq!(v["status"], "passed", "{v}");
        let bad = write(dir.path(), "bad.c", "int f(void) {\n  return missing;\n}\n");
        let v = check_file_with(&bad, &tools());
        assert_eq!(v["status"], "failed", "{v}");
        assert_eq!(diag_lines(&v), vec![2], "{v}");
        assert!(!dir.path().join("a.out").exists() && !dir.path().join("bad.o").exists());
    }

    #[test]
    fn a_rust_plugin_crate_is_cargo_checked() {
        if !need("cargo") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let krate = dir.path().join("rplug");
        write(
            &krate,
            "Cargo.toml",
            "[package]\nname = \"rplug\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[workspace]\n",
        );
        let lib = write(&krate, "src/lib.rs", "pub fn f() -> i32 {\n    1\n}\n");
        // Never the CARGO_TARGET_DIR this test binary was built with.
        let target = dir.path().join("target");
        let private = Tools {
            cargo_target_dir: Some(&target),
            embedded: &|_| None,
            ..tools()
        };
        let v = check_file_with(&lib, &private);
        assert_eq!(v["status"], "passed", "{v}");
        assert!(v["note"].as_str().unwrap().contains("build scripts"));
        assert!(
            target.join("debug").is_dir(),
            "cargo used the private target dir"
        );
        write(
            &krate,
            "src/lib.rs",
            "pub fn f() -> i32 {\n    missing\n}\n",
        );
        let v = check_file_with(&lib, &private);
        assert_eq!(v["status"], "failed", "{v}");
        assert_eq!(diag_lines(&v), vec![2], "{v}");
        assert!(v["diagnostics"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("lib.rs"));
    }

    #[test]
    fn racket_and_lisp_files_are_read_not_run() {
        let dir = tempfile::tempdir().unwrap();
        if need("racket") {
            let good = write(dir.path(), "good.rkt", "#lang racket/base\n(define x 1)\n");
            let v = check_file_with(&good, &tools());
            assert_eq!(v["status"], "passed", "{v}");
            let marker = dir.path().join("ran");
            let sneaky = write(
                dir.path(),
                "run.rkt",
                &format!(
                    "#lang racket/base\n(with-output-to-file {:?} (lambda () (display 1)))\n",
                    marker.display().to_string()
                ),
            );
            assert_eq!(check_file_with(&sneaky, &tools())["status"], "passed");
            assert!(!marker.exists(), "the Racket check ran the module");
            let bad = write(
                dir.path(),
                "bad.rkt",
                "#lang racket/base\n(define x\n  (list 1 2)\n",
            );
            let v = check_file_with(&bad, &tools());
            assert_eq!(v["status"], "failed", "{v}");
            assert_eq!(v["diagnostics"].as_array().unwrap().len(), 1, "{v}");
        }
        if need("ecl") {
            let good = write(
                dir.path(),
                "good.lisp",
                "(register-tool \"x\" \"y\" nil (lambda (a s) a))\n",
            );
            let v = check_file_with(&good, &tools());
            assert_eq!(v["status"], "passed", "{v}");
            let marker = dir.path().join("lisp-ran");
            let sneaky = write(
                dir.path(),
                "run.lisp",
                &format!(
                    "#.(with-open-file (s {:?} :direction :output) (print 1 s))\n",
                    marker.display().to_string()
                ),
            );
            assert_eq!(check_file_with(&sneaky, &tools())["status"], "failed");
            assert!(!marker.exists(), "the Lisp check evaluated #.");
            let bad = write(dir.path(), "bad.lisp", "(defun f ()\n  (list 1 2)\n");
            let v = check_file_with(&bad, &tools());
            assert_eq!(v["status"], "failed", "{v}");
        }
    }
}
