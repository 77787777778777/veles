//! lsp_servers — which language server, rooted where, for a file the
//! Dev pane opens.
//!
//! `lsp_proxy` is a framed pipe and stays one: it does not know a
//! language. This file is the other half of `/api/lsp/start`, the part
//! that used to be a constant (`tcl-lsp`, rooted at the nearest `.git`).
//!
//! WHY IT EXISTS
//!
//! A veles extension is not only Tcl. `docs/plugins.md` lists the tiers:
//! Tcl, JavaScript/TypeScript (lang-js), Racket (lang-racket), Common
//! Lisp (lang-lisp), out-of-tree Rust (`kind = "rust"`, a cdylib on the
//! ABI 4 SDK), a module (a Rust dylib), a process plugin (any executable,
//! usually Rust), and the C ABI itself for "a plugin in another
//! language". Until 0.2.0 every one of those buffers was either diagnosed
//! as Tcl (dev-ext-8) or, after that fix, gated off with "no language
//! server". Owner ask 287976.2 wants full LSP for each.
//!
//! WHAT IS DECIDED HERE, PER LANGUAGE
//!
//!   * the server: a short candidate list, resolved on the GATEWAY's
//!     PATH (that is where the files are), overridable per language
//!     with `VELES_LSP_<KEY>`;
//!   * the session key: TypeScript and JavaScript share one tsserver,
//!     C and C++ one clangd, so a tab switch between them is a reuse;
//!   * the project root, by that language's own rule — the nearest
//!     `Cargo.toml` (lifted to the workspace that lists it), the nearest
//!     `compile_commands.json`, the nearest `tsconfig.json` or the
//!     extension directory, the VCS root for Tcl;
//!   * the message when nothing is installed: which program, on which
//!     machine, and how to install it. Silence was the old failure.
//!
//! Paths cross machines here (a Linux desktop can edit a Windows
//! gateway's files), so the `file://` URI is built on the gateway, for
//! the gateway's platform, by [`file_uri`]; [`uri_path`] is its inverse.
//! Both take the platform as a parameter so the Windows rules are
//! exercised on Linux (the `platform::absent_for(os, …)` pattern).

use std::path::{Path, PathBuf};

/// A language a veles extension can be written in, as the editor sees
/// it: one per LSP `languageId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Tcl,
    TypeScript,
    JavaScript,
    Rust,
    C,
    Cpp,
    Racket,
    CommonLisp,
}

impl Language {
    /// Every language, in the order the docs list the tiers.
    pub const ALL: [Language; 8] = [
        Language::Tcl,
        Language::TypeScript,
        Language::JavaScript,
        Language::Racket,
        Language::CommonLisp,
        Language::Rust,
        Language::C,
        Language::Cpp,
    ];

    /// The language of a file, by extension. `None` is "no language
    /// server is known for this file", which the caller reports as such
    /// rather than guessing.
    pub fn from_path(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "tcl" | "tm" | "test" | "tk" => Language::Tcl,
            "ts" | "mts" | "cts" | "tsx" => Language::TypeScript,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "rs" => Language::Rust,
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Language::Cpp,
            "rkt" => Language::Racket,
            "lisp" | "lsp" | "cl" | "asd" => Language::CommonLisp,
            _ => return None,
        })
    }

    /// The name a request or a reply carries.
    pub fn parse(name: &str) -> Option<Language> {
        Language::ALL.into_iter().find(|l| l.id() == name)
    }

    /// The LSP `languageId` for `textDocument/didOpen`.
    pub fn id(self) -> &'static str {
        match self {
            Language::Tcl => "tcl",
            Language::TypeScript => "typescript",
            Language::JavaScript => "javascript",
            Language::Rust => "rust",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Racket => "racket",
            Language::CommonLisp => "commonlisp",
        }
    }

    /// The gateway session this language shares with its siblings: one
    /// tsserver answers `.ts` and `.js`, one clangd answers C and C++.
    pub fn server_key(self) -> &'static str {
        match self {
            Language::Tcl => "tcl",
            Language::TypeScript | Language::JavaScript => "typescript",
            Language::Rust => "rust",
            Language::C | Language::Cpp => "c",
            Language::Racket => "racket",
            Language::CommonLisp => "lisp",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Language::Tcl => "Tcl",
            Language::TypeScript => "TypeScript",
            Language::JavaScript => "JavaScript",
            Language::Rust => "Rust",
            Language::C => "C",
            Language::Cpp => "C++",
            Language::Racket => "Racket",
            Language::CommonLisp => "Common Lisp",
        }
    }

    /// The servers tried in order. The first one found on the gateway's
    /// PATH is used.
    pub fn candidates(self) -> &'static [(&'static str, &'static [&'static str])] {
        match self.server_key() {
            "tcl" => &[("tcl-lsp", &[])],
            "typescript" => &[("typescript-language-server", &["--stdio"])],
            "rust" => &[("rust-analyzer", &[])],
            "c" => &[("clangd", &[]), ("ccls", &[])],
            // racket-langserver is a Racket package run by `racket -l`:
            // `racket` on PATH without the package exits at once with
            // "collection not found", which the proxy reports verbatim.
            "racket" => &[("racket", &["-l", "racket-langserver"])],
            // There is no one standard Common Lisp server; cl-lsp is the
            // stdio one. VELES_LSP_LISP names any other.
            _ => &[("cl-lsp", &[])],
        }
    }

    /// How to make a server available, said in the reply when none is.
    pub fn install_hint(self) -> &'static str {
        match self.server_key() {
            "tcl" => "install tcl-lsp on the gateway's PATH",
            "typescript" => {
                "install typescript-language-server and typescript on the gateway (npm install -g typescript-language-server typescript, or the nixpkgs packages of the same names)"
            }
            "rust" => "install rust-analyzer on the gateway (rustup component add rust-analyzer)",
            "c" => "install clangd (LLVM) on the gateway",
            "racket" => {
                "install Racket and the racket-langserver package on the gateway (raco pkg install racket-langserver)"
            }
            _ => {
                "install a Common Lisp language server on the gateway (for example cl-lsp), or set VELES_LSP_LISP to its command line"
            }
        }
    }

    /// The environment variable that replaces the candidate list.
    pub fn override_var(self) -> String {
        format!("VELES_LSP_{}", self.server_key().to_ascii_uppercase())
    }
}

/// What `/api/lsp/start` runs for a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub language: Language,
    pub program: String,
    pub args: Vec<String>,
    pub root: PathBuf,
    /// A known limit of this setup the editor should say out loud (a
    /// TypeScript extension without tsconfig.json does not know the
    /// `veles` global).
    pub note: Option<String>,
}

/// No server could be found. The message names the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub language: Language,
    pub tried: Vec<String>,
}

impl Missing {
    pub fn message(&self) -> String {
        format!(
            "no {} language server is installed on the gateway (tried {}); {}",
            self.language.display(),
            self.tried.join(", "),
            self.language.install_hint()
        )
    }
}

/// The candidate list, after `VELES_LSP_<KEY>` (a whitespace-separated
/// command line) has had its say.
pub fn candidate_commands(
    language: Language,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, Vec<String>)> {
    if let Some(line) = env(&language.override_var()) {
        let mut words = line.split_whitespace().map(str::to_string);
        if let Some(program) = words.next() {
            return vec![(program, words.collect())];
        }
    }
    language
        .candidates()
        .iter()
        .map(|(p, a)| (p.to_string(), a.iter().map(|s| s.to_string()).collect()))
        .collect()
}

/// Pick the server and root for `path`. `which` resolves a program name
/// on the gateway (tests pass their own); `env` reads the override.
pub fn select_with(
    path: &Path,
    which: &dyn Fn(&str) -> Option<PathBuf>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Selection, Option<Missing>> {
    let language = Language::from_path(path).ok_or(None)?;
    let commands = candidate_commands(language, env);
    let Some((program, args)) = commands
        .iter()
        .find(|(program, _)| which(program).is_some())
        .cloned()
    else {
        return Err(Some(Missing {
            language,
            tried: commands.into_iter().map(|(p, _)| p).collect(),
        }));
    };
    let root = project_root(language, path);
    let note = match language {
        Language::TypeScript | Language::JavaScript
            if !has_any(&root, &["tsconfig.json", "jsconfig.json"]) =>
        {
            Some(
                "no tsconfig.json in this extension: the language server does not know the `veles` global or top-level await; the New extension TypeScript scaffold writes tsconfig.json and veles.d.ts".to_string(),
            )
        }
        _ => None,
    };
    Ok(Selection {
        language,
        program,
        args,
        root,
        note,
    })
}

/// [`select_with`] against this process's PATH and environment.
pub fn select(path: &Path) -> Result<Selection, Option<Missing>> {
    select_with(path, &|name| crate::shell::which(name), &|var| {
        std::env::var(var).ok().filter(|v| !v.trim().is_empty())
    })
}

fn has_any(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|n| dir.join(n).exists())
}

/// The directory a file's search starts from.
fn start_dir(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// The nearest ancestor (including `start`) holding any of `markers`.
fn nearest(start: &Path, markers: &[&str]) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| has_any(dir, markers))
        .map(Path::to_path_buf)
}

const VCS_MARKERS: [&str; 3] = [".git", ".hg", ".jj"];

/// The extension directory a file belongs to: the nearest ancestor with a
/// `plugin.toml`.
pub fn extension_dir(path: &Path) -> Option<PathBuf> {
    nearest(&start_dir(path), &["plugin.toml"])
}

/// The project a language server should index for `path`.
///
/// The rule is the language's own, because each server has one: rust-
/// analyzer loads a cargo workspace, clangd a compilation database,
/// tsserver a tsconfig. Rooting all of them at "the nearest .git" (the
/// tcl-lsp rule, and the only one this pane had) puts rust-analyzer at
/// the top of a repository it cannot load and gives tsserver a root with
/// no configuration.
pub fn project_root(language: Language, path: &Path) -> PathBuf {
    let start = start_dir(path);
    match language {
        // Unchanged since 2026-08-25: the user plugin tree and this repo
        // are both git work trees, and tcl-lsp indexes the whole thing.
        Language::Tcl => {
            nearest(&start, &[".git", ".hg", ".jj", "Cargo.toml", "flake.nix"]).unwrap_or(start)
        }
        Language::Rust => cargo_root(&start).unwrap_or(start),
        Language::C | Language::Cpp => nearest(
            &start,
            &["compile_commands.json", "compile_flags.txt", ".clangd"],
        )
        .or_else(|| {
            nearest(
                &start,
                &["CMakeLists.txt", "meson.build", "Makefile", "plugin.toml"],
            )
        })
        .or_else(|| nearest(&start, &VCS_MARKERS))
        .unwrap_or(start),
        Language::TypeScript | Language::JavaScript => {
            bounded_nearest(&start, &["tsconfig.json", "jsconfig.json", "package.json"])
                .or_else(|| extension_dir(path))
                .or_else(|| nearest(&start, &VCS_MARKERS))
                .unwrap_or(start)
        }
        Language::Racket => bounded_nearest(&start, &["info.rkt"])
            .or_else(|| extension_dir(path))
            .or_else(|| nearest(&start, &VCS_MARKERS))
            .unwrap_or(start),
        Language::CommonLisp => nearest_with_extension(&start, "asd")
            .or_else(|| extension_dir(path))
            .or_else(|| nearest(&start, &VCS_MARKERS))
            .unwrap_or(start),
    }
}

/// [`nearest`], but not above the extension directory or the VCS root:
/// a `package.json` in the user's home is not this extension's project.
fn bounded_nearest(start: &Path, markers: &[&str]) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if has_any(dir, markers) {
            return Some(dir.to_path_buf());
        }
        if dir.join("plugin.toml").exists() || has_any(dir, &VCS_MARKERS) {
            return None;
        }
    }
    None
}

fn nearest_with_extension(start: &Path, ext: &str) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let entries = std::fs::read_dir(dir).ok()?;
        entries
            .flatten()
            .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
            .then(|| dir.to_path_buf())
    })
}

/// The cargo workspace a Rust file is part of, the way cargo itself
/// decides: the nearest `Cargo.toml` is the package; an ancestor
/// manifest with `[workspace]` is the root only when it lists the
/// package as a member (and does not exclude it). A plugin crate that
/// merely sits inside some other workspace's directory — an extension
/// in `plugins/examples/` of this repository — is its own root, as
/// `cargo check` would treat it.
fn cargo_root(start: &Path) -> Option<PathBuf> {
    let package = nearest(start, &["Cargo.toml"])?;
    if manifest_has_workspace(&package.join("Cargo.toml")) {
        return Some(package);
    }
    for dir in package.ancestors().skip(1) {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        let Some(ws) = doc.get("workspace").and_then(toml::Value::as_table) else {
            continue;
        };
        let Ok(rel) = package.strip_prefix(dir) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let listed = |key: &str| {
            ws.get(key)
                .and_then(toml::Value::as_array)
                .is_some_and(|globs| {
                    globs
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .any(|g| member_glob_matches(g, &rel))
                })
        };
        if listed("members") && !listed("exclude") {
            return Some(dir.to_path_buf());
        }
        // A workspace that does not list the package: cargo stops here
        // too (it is an error to be inside one unlisted), so the package
        // is the root the server can load at all.
        return Some(package);
    }
    Some(package)
}

fn manifest_has_workspace(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .is_some_and(|doc| doc.contains_key("workspace"))
}

/// Cargo's member globs, as far as manifests use them: an exact path,
/// `*` within one component, and a trailing `/*`.
fn member_glob_matches(glob: &str, rel: &str) -> bool {
    let glob = glob.trim_end_matches('/');
    let g: Vec<&str> = glob.split('/').collect();
    let r: Vec<&str> = rel.split('/').collect();
    if g.len() != r.len() {
        return false;
    }
    g.iter().zip(&r).all(|(g, r)| component_matches(g, r))
}

fn component_matches(glob: &str, name: &str) -> bool {
    match glob.split_once('*') {
        None => glob == name,
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post)
        }
    }
}

// ── paths that cross machines ───────────────────────────────────────

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\host\share\x` → `\\host\share\x`.
///
/// `canonicalize` answers verbatim paths on Windows. They are correct and
/// useless here: a `file://` URI built from one names no file any server
/// opens, and a verbatim root is never a prefix of the plain path the
/// desktop sent, so every "is this inside the user plugin directory"
/// test failed on Windows.
pub fn simplify_verbatim(path: &str, os: &str) -> String {
    if os != "windows" {
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// `path` relative to `root`, `/`-joined, when it is inside it — on a
/// host running `os`. Windows paths compare without case and either
/// separator, after the verbatim prefix `canonicalize` adds; the desktop
/// sends `C:\Users\…` while a canonical root is `\\?\C:\Users\…`, so the
/// plain `strip_prefix` refused every user plugin on Windows.
pub fn relative_to_root(path: &str, root: &str, os: &str) -> Option<String> {
    if os == "windows" {
        let norm = |p: &str| {
            simplify_verbatim(p, os)
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_string()
        };
        let (p, r) = (norm(path), norm(root));
        if p.len() <= r.len() + 1 || !p.is_char_boundary(r.len()) {
            return None;
        }
        let (head, tail) = p.split_at(r.len());
        if !head.eq_ignore_ascii_case(&r) || !tail.starts_with('/') {
            return None;
        }
        let rel: Vec<&str> = tail.split('/').filter(|c| !c.is_empty()).collect();
        return (!rel.is_empty()).then(|| rel.join("/"));
    }
    let rel = Path::new(path).strip_prefix(root).ok()?;
    let parts = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// [`simplify_verbatim`] for a `Path` on this host.
pub fn display_path(path: &Path) -> String {
    simplify_verbatim(&path.to_string_lossy(), crate::platform::OS)
}

/// Canonicalize, then drop a verbatim prefix — a path both the desktop
/// and a language server can use.
pub fn canonical(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(p) => PathBuf::from(display_path(&p)),
        Err(_) => path.to_path_buf(),
    }
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'-'
                    | b'_'
                    | b'.'
                    | b'~'
                    | b':'
                    | b'@'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
            );
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `file://` URI of a path on a host running `os`.
///
/// Built by the gateway, not the desktop: the client used to prepend
/// `file://` to whatever it had, which names `file://C:\Users\…` for a
/// Windows gateway (not a URI) and leaves a space or `#` in a Linux path
/// unencoded, which rust-analyzer and clangd reject.
pub fn file_uri(path: &str, os: &str) -> String {
    let path = simplify_verbatim(path, os);
    if os == "windows" {
        let slashed = path.replace('\\', "/");
        if let Some(unc) = slashed.strip_prefix("//") {
            // \\host\share\x → file://host/share/x
            let (host, rest) = unc.split_once('/').unwrap_or((unc, ""));
            return format!("file://{host}/{}", percent_encode_path(rest));
        }
        return format!("file:///{}", percent_encode_path(&slashed));
    }
    format!("file://{}", percent_encode_path(&path))
}

/// The path a `file://` URI names on a host running `os`; `None` for any
/// other scheme. Inverse of [`file_uri`], and lenient about what servers
/// send back (`file:///c%3A/x`, a `localhost` authority).
pub fn uri_path(uri: &str, os: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let (host, path) = if rest.starts_with('/') {
        ("", rest)
    } else {
        match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        }
    };
    let host = if host.eq_ignore_ascii_case("localhost") {
        ""
    } else {
        host
    };
    let path = percent_decode(path);
    if os == "windows" {
        let b = path.as_bytes();
        if host.is_empty()
            && b.len() >= 3
            && b[0] == b'/'
            && b[1].is_ascii_alphabetic()
            && b[2] == b':'
        {
            let drive = (b[1] as char).to_ascii_uppercase();
            return Some(format!("{drive}{}", path[2..].replace('/', "\\")));
        }
        if !host.is_empty() {
            return Some(format!(r"\\{host}{}", path.replace('/', "\\")));
        }
        return Some(path.replace('/', "\\"));
    }
    if !host.is_empty() {
        return Some(format!("//{host}{path}"));
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none_env(_: &str) -> Option<String> {
        None
    }

    fn touch(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn every_extension_language_has_a_server_and_a_language_id() {
        // The tiers docs/plugins.md lists, by the files they are written
        // in. A buffer in any of them used to get "no language server".
        for (file, id, key) in [
            ("plugin.tcl", "tcl", "tcl"),
            ("lib.tm", "tcl", "tcl"),
            ("plugin.ts", "typescript", "typescript"),
            ("plugin.mts", "typescript", "typescript"),
            ("plugin.js", "javascript", "typescript"),
            ("plugin.mjs", "javascript", "typescript"),
            ("src/lib.rs", "rust", "rust"),
            ("plugin.c", "c", "c"),
            ("veles_plugin.h", "c", "c"),
            ("plugin.cpp", "cpp", "c"),
            ("plugin.rkt", "racket", "racket"),
            ("plugin.lisp", "commonlisp", "lisp"),
            ("plugin.asd", "commonlisp", "lisp"),
        ] {
            let lang = Language::from_path(Path::new(file)).unwrap_or_else(|| panic!("{file}"));
            assert_eq!(lang.id(), id, "{file}");
            assert_eq!(lang.server_key(), key, "{file}");
            assert!(!lang.candidates().is_empty());
            assert_eq!(Language::parse(id), Some(lang));
        }
        assert_eq!(Language::from_path(Path::new("README.md")), None);
        assert_eq!(Language::from_path(Path::new("Makefile")), None);
    }

    #[test]
    fn the_server_is_picked_by_file_type_and_found_on_the_gateway_path() {
        let dir = tempfile::tempdir().unwrap();
        let found = |name: &str| {
            matches!(
                name,
                "tcl-lsp" | "typescript-language-server" | "rust-analyzer" | "clangd"
            )
            .then(|| PathBuf::from(format!("/bin/{name}")))
        };
        for (file, program, args) in [
            ("a.tcl", "tcl-lsp", vec![]),
            ("a.ts", "typescript-language-server", vec!["--stdio"]),
            ("a.js", "typescript-language-server", vec!["--stdio"]),
            ("a.rs", "rust-analyzer", vec![]),
            ("a.c", "clangd", vec![]),
        ] {
            let sel = select_with(&dir.path().join(file), &found, &none_env).unwrap();
            assert_eq!(sel.program, program, "{file}");
            assert_eq!(sel.args, args, "{file}");
        }
        // A second C candidate is tried when the first is absent.
        let only_ccls = |name: &str| (name == "ccls").then(|| PathBuf::from("/bin/ccls"));
        let sel = select_with(&dir.path().join("a.c"), &only_ccls, &none_env).unwrap();
        assert_eq!(sel.program, "ccls");
    }

    #[test]
    fn a_missing_server_says_which_program_on_which_machine_and_how_to_install_it() {
        let dir = tempfile::tempdir().unwrap();
        let nothing = |_: &str| None;
        for (file, program, hint) in [
            ("a.ts", "typescript-language-server", "npm install"),
            ("a.rs", "rust-analyzer", "rustup component add"),
            ("a.rkt", "racket", "raco pkg install racket-langserver"),
            ("a.lisp", "cl-lsp", "VELES_LSP_LISP"),
            ("a.c", "clangd", "clangd"),
        ] {
            let missing = select_with(&dir.path().join(file), &nothing, &none_env)
                .unwrap_err()
                .expect("a known language");
            let text = missing.message();
            assert!(text.contains("on the gateway"), "{text}");
            assert!(text.contains(program), "{text}");
            assert!(text.contains(hint), "{text}");
        }
        // An unknown file type is not "missing", it is unknown.
        assert_eq!(
            select_with(&dir.path().join("notes.md"), &nothing, &none_env),
            Err(None)
        );
    }

    #[test]
    fn the_override_variable_replaces_the_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let env =
            |var: &str| (var == "VELES_LSP_LISP").then(|| "alive-lsp --stdio  -v".to_string());
        let any = |name: &str| Some(PathBuf::from(format!("/opt/{name}")));
        let sel = select_with(&dir.path().join("plugin.lisp"), &any, &env).unwrap();
        assert_eq!(sel.program, "alive-lsp");
        assert_eq!(sel.args, vec!["--stdio", "-v"]);
        let nothing = |_: &str| None;
        let missing = select_with(&dir.path().join("plugin.lisp"), &nothing, &env)
            .unwrap_err()
            .unwrap();
        assert_eq!(missing.tried, vec!["alive-lsp".to_string()]);
    }

    #[test]
    fn a_rust_plugin_crate_is_rooted_at_its_package_or_the_workspace_that_lists_it() {
        let base = tempfile::tempdir().unwrap();
        let b = base.path();
        // A workspace that lists one plugin crate and not another.
        touch(
            &b.join("Cargo.toml"),
            "[workspace]\nmembers = [\"plugins/*\"]\nexclude = [\"plugins/skipped\"]\n",
        );
        touch(&b.join(".git/HEAD"), "ref: refs/heads/main\n");
        for name in ["member", "skipped"] {
            touch(
                &b.join(format!("plugins/{name}/Cargo.toml")),
                &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            );
            touch(&b.join(format!("plugins/{name}/src/lib.rs")), "\n");
        }
        // A crate inside the tree that the workspace does not list at all.
        touch(
            &b.join("extra/stray/Cargo.toml"),
            "[package]\nname = \"stray\"\nversion = \"0.1.0\"\n",
        );
        touch(&b.join("extra/stray/src/lib.rs"), "\n");
        // A standalone crate with its own [workspace].
        touch(
            &b.join("alone/Cargo.toml"),
            "[package]\nname = \"alone\"\nversion = \"0.1.0\"\n[workspace]\n",
        );
        touch(&b.join("alone/src/deep/mod.rs"), "\n");

        let root = |p: &str| project_root(Language::Rust, &b.join(p));
        assert_eq!(root("plugins/member/src/lib.rs"), b);
        assert_eq!(
            root("plugins/skipped/src/lib.rs"),
            b.join("plugins/skipped")
        );
        assert_eq!(root("extra/stray/src/lib.rs"), b.join("extra/stray"));
        assert_eq!(root("alone/src/deep/mod.rs"), b.join("alone"));
        // Tcl keeps the VCS rule — the same file in a Tcl buffer would be
        // rooted at the repository, which is what tcl-lsp indexes.
        touch(&b.join("plugins/member/plugin.tcl"), "\n");
        assert_eq!(
            project_root(Language::Tcl, &b.join("plugins/member/plugin.tcl")),
            b.join("plugins/member"),
            "the nearest Cargo.toml is a Tcl project marker too (unchanged rule)"
        );
    }

    #[test]
    fn typescript_c_racket_and_lisp_roots_follow_their_own_project_files() {
        let base = tempfile::tempdir().unwrap();
        let b = base.path();
        touch(&b.join(".git/HEAD"), "x\n");
        // A TS extension with a tsconfig, one without, and a package.json
        // ABOVE the extension that must not capture it.
        touch(&b.join("package.json"), "{}\n");
        touch(&b.join("plugins/typed/plugin.toml"), "name = \"typed\"\n");
        touch(&b.join("plugins/typed/tsconfig.json"), "{}\n");
        touch(&b.join("plugins/typed/src/plugin.ts"), "\n");
        touch(&b.join("plugins/loose/plugin.toml"), "name = \"loose\"\n");
        touch(&b.join("plugins/loose/plugin.ts"), "\n");
        assert_eq!(
            project_root(Language::TypeScript, &b.join("plugins/typed/src/plugin.ts")),
            b.join("plugins/typed")
        );
        assert_eq!(
            project_root(Language::TypeScript, &b.join("plugins/loose/plugin.ts")),
            b.join("plugins/loose"),
            "the extension directory, not the package.json above it"
        );
        // C: the compilation database wins over the build file.
        touch(&b.join("native/plugin.toml"), "name = \"native\"\n");
        touch(&b.join("native/build/compile_commands.json"), "[]\n");
        touch(&b.join("native/build/gen/plugin.c"), "\n");
        touch(&b.join("native/src/plugin.c"), "\n");
        assert_eq!(
            project_root(Language::C, &b.join("native/build/gen/plugin.c")),
            b.join("native/build")
        );
        assert_eq!(
            project_root(Language::C, &b.join("native/src/plugin.c")),
            b.join("native"),
            "no database above src/: the extension's build root"
        );
        // Racket and Lisp: their package files, else the extension.
        touch(&b.join("rkt/plugin.toml"), "name = \"rkt\"\n");
        touch(&b.join("rkt/plugin.rkt"), "\n");
        assert_eq!(
            project_root(Language::Racket, &b.join("rkt/plugin.rkt")),
            b.join("rkt")
        );
        touch(&b.join("cl/plugin.toml"), "name = \"cl\"\n");
        touch(&b.join("cl/sys/tool.asd"), "\n");
        touch(&b.join("cl/sys/plugin.lisp"), "\n");
        assert_eq!(
            project_root(Language::CommonLisp, &b.join("cl/sys/plugin.lisp")),
            b.join("cl/sys")
        );
    }

    #[test]
    fn a_typescript_extension_without_tsconfig_is_told_what_the_server_cannot_know() {
        let base = tempfile::tempdir().unwrap();
        let b = base.path();
        touch(&b.join("loose/plugin.toml"), "name = \"loose\"\n");
        touch(&b.join("loose/plugin.ts"), "\n");
        touch(&b.join("typed/plugin.toml"), "name = \"typed\"\n");
        touch(&b.join("typed/tsconfig.json"), "{}\n");
        touch(&b.join("typed/plugin.ts"), "\n");
        let any = |n: &str| Some(PathBuf::from(n));
        let loose = select_with(&b.join("loose/plugin.ts"), &any, &none_env).unwrap();
        assert!(loose.note.as_deref().unwrap_or("").contains("veles"));
        let typed = select_with(&b.join("typed/plugin.ts"), &any, &none_env).unwrap();
        assert_eq!(typed.note, None);
    }

    #[test]
    fn windows_paths_become_file_uris_and_back_on_any_host() {
        // dev-ext-13: exercised on Linux with the platform as a parameter.
        for (path, uri) in [
            (
                r"C:\Users\Ann\AppData\Roaming\veles\plugins\demo\plugin.tcl",
                "file:///C:/Users/Ann/AppData/Roaming/veles/plugins/demo/plugin.tcl",
            ),
            (
                r"\\?\C:\Users\Ann\plugins\nested dir\x.ts",
                "file:///C:/Users/Ann/plugins/nested%20dir/x.ts",
            ),
            (
                r"\\?\UNC\srv\share\plugins\demo\lib.rs",
                "file://srv/share/plugins/demo/lib.rs",
            ),
            (r"\\srv\share\a.c", "file://srv/share/a.c"),
        ] {
            assert_eq!(file_uri(path, "windows"), uri, "{path}");
        }
        assert_eq!(
            uri_path("file:///C:/Users/Ann/plugins/nested%20dir/x.ts", "windows").unwrap(),
            r"C:\Users\Ann\plugins\nested dir\x.ts"
        );
        // What servers actually send back: a lower-case, encoded drive.
        assert_eq!(
            uri_path("file:///c%3A/Users/Ann/x.rs", "windows").unwrap(),
            r"C:\Users\Ann\x.rs"
        );
        assert_eq!(
            uri_path("file://srv/share/a.c", "windows").unwrap(),
            r"\\srv\share\a.c"
        );
        // Unix: encoded, and decoded back exactly.
        let odd = "/home/ann/.config/veles/plugins/my ext/é#1.tcl";
        let uri = file_uri(odd, "linux");
        assert_eq!(
            uri,
            "file:///home/ann/.config/veles/plugins/my%20ext/%C3%A9%231.tcl"
        );
        assert_eq!(uri_path(&uri, "linux").unwrap(), odd);
        assert_eq!(
            uri_path("file://localhost/tmp/x", "linux").unwrap(),
            "/tmp/x"
        );
        assert_eq!(uri_path("untitled:Untitled-1", "linux"), None);
        // Inside the user plugin root, as the desktop spells a path and as
        // canonicalize spells the root.
        let roaming = r"C:\Users\Ann\AppData\Roaming\veles\plugins";
        assert_eq!(
            relative_to_root(
                r"C:\Users\Ann\AppData\Roaming\veles\plugins\demo\nested\helper.tcl",
                &format!(r"\\?\{roaming}"),
                "windows"
            )
            .as_deref(),
            Some("demo/nested/helper.tcl")
        );
        assert_eq!(
            relative_to_root(
                "c:/users/ann/appdata/roaming/veles/plugins/demo/plugin.tcl",
                roaming,
                "windows"
            )
            .as_deref(),
            Some("demo/plugin.tcl")
        );
        assert_eq!(
            relative_to_root(
                r"C:\Users\Ann\AppData\Roaming\veles\pluginsX\d\p.tcl",
                roaming,
                "windows"
            ),
            None
        );
        assert_eq!(relative_to_root(roaming, roaming, "windows"), None);
        assert_eq!(
            relative_to_root(
                "/home/a/.config/veles/plugins/demo/plugin.tcl",
                "/home/a/.config/veles/plugins",
                "linux"
            )
            .as_deref(),
            Some("demo/plugin.tcl")
        );
        assert_eq!(
            relative_to_root(
                "/home/a/.config/veles/pluginsX/demo/p.tcl",
                "/home/a/.config/veles/plugins",
                "linux"
            ),
            None
        );
        // A verbatim prefix is only a Windows thing.
        assert_eq!(simplify_verbatim(r"\\?\C:\x", "linux"), r"\\?\C:\x");
        assert_eq!(simplify_verbatim(r"\\?\C:\x", "windows"), r"C:\x");
        assert_eq!(simplify_verbatim(r"\\?\UNC\h\s\x", "windows"), r"\\h\s\x");
    }
}
