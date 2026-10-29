//! Plugin system — Tcl on disk, Rust in-tree AND out-of-tree. All real
//! since 2026-09-03 (docs/plugin-plan.md records the design decisions).
//!
//! Tcl plugins (hot, fast iteration — the slopdrop lesson):
//! `~/.config/veles/plugins/<name>/plugin.tcl` (+ `plugin.toml` manifest),
//! merged with the read-only BUNDLED tier a release carries
//! (`bundled_roots`: `$VELES_BUNDLED_PLUGINS`, then
//! `<exe>/../share/veles/plugins`), the user's copy winning by name
//! - loaded into the plugins interpreter at boot; `kind = "rust"`
//!   manifests go to the `.so` loader instead (see below), `"module"`
//!   to the module host, `"process"` to the process host, and `"js"` /
//!   `"racket"` / `"lisp"` to the script host (docs/plugins.md has the
//!   tier table); a kind nothing handles is skipped with a warning
//! - re-sourced without restart by a notify::watcher on the directory,
//!   or on demand with `veles plugins reload` or `veles tcl reload`
//!   (both reach the RUNNING gateway over loopback; the first did not
//!   exist when this comment was written and a line here said so)
//! - managed from the CLI (`veles plugins new|install|remove|list|
//!   rollback|hooks`, and since the plugin rounds `status|reload|unload|
//!   start|stop|logs|rescan|services|overrides|api|install-module`) as
//!   well as the desktop manager. Those three verbs
//!   were desktop-only until 2026-09-04, which left a plugin author
//!   working in a terminal with no way to create a plugin at all.
//! - what they can register, for real: engine hooks (`::veles::hook`),
//!   gateway dispatch filters (`pre_gateway_dispatch` skip/rewrite),
//!   TOOLS (`veles::register_tool` — TclTool adapter, rides
//!   `register_global_tool`, visible to sessions created after the
//!   load) and SLASH COMMANDS (`veles::register_command` — proc-backed
//!   render, wins over a same-named .md file)
//! - what they can drive besides: sessions/history/usage reads, config
//!   reads + writes (config_set, with the restart advisory), HTTP,
//!   gateway outbound, background tasks, `veles::tool_call` (the full
//!   native surface — bridge.rs, declared in plugins/veles_api.tcl,
//!   cross-checked by test)
//! - state persistence: git-backed directory (slopdrop pattern) so every
//!   live `proc` redefinition is versioned with author attribution
//!
//! Rust plugins, Phase A: crates linked into the binary, registered at
//! boot from the composition root before any agent or gateway starts —
//! see rustplugins.rs for the trait, the registry and the hook/tool/
//! gateway-filter integration. Reference example: crates/veles-plugin-example.
//!
//! Rust plugins, Phase B: out-of-tree `cdylib` plugins. Manifest with
//! `kind = "rust"`, `entry = "plugin.so"`; the loader dlopens the
//! library and speaks the C ABI in crates/veles-plugin-abi (the ONLY
//! thing a plugin links — linking veles-core would make it in-tree).
//! Hooks, tools, slash commands, the gateway filter and PROVIDER
//! DRIVERS all cross the boundary, and since ABI 2 the boundary goes
//! both ways: `host_call` reaches the same op table the Tcl commands
//! adapt over, so an out-of-tree plugin drives the engine exactly as a
//! Tcl one does. Version 1 could not — it had three function pointers,
//! all registration — and rustplugins.rs records what that cost.
//! Worked example: crates/veles-plugin-sample-so, loaded end-to-end by
//! the integration test.
//!
//! - what BOTH kinds can drive: sessions/history/usage reads, config
//!   reads + writes, HTTP (with headers), gateway outbound, notes,
//!   background tasks, `tool_call`, `call_llm`, and `provider_emit`
//!   for streaming out of a driver. One table — `bridgeops::OP_NAMES`,
//!   cross-checked against Tcl and the C ABI by test.
//!
//! Sample plugins you can copy: plugins/examples/ —
//!   - tcl-wordcount (tool registration)
//!   - tcl-alias-filter (gateway filter)
//!   - tcl-session-digest (bridge reads + notes)
//!   - tcl-anthropic-driver (a COMPLETE provider driver: the Anthropic
//!     Messages wire, translated both ways). Its twin lives in
//!     crates/veles-plugin-anthropic-so, and tests/driver_parity.rs
//!     asserts the two produce the SAME Completion from one fixture —
//!     which is what makes "both languages, same capability" a checked
//!     property rather than a claim.
//!
//! The cargo gate loads all of them, so the templates cannot silently
//! rot. The author's guide is docs/plugins.md.
//!
//! Manifest (plugin.toml):
//!      name = "reasoning-summary"
//!      kind = "tcl"            # or "rust" (entry = "plugin.so")
//!      provides_hooks = ["post_llm_call"]
//!      entry = "plugin.tcl"

use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub provides_hooks: Vec<String>,
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default)]
    pub description: String,
    /// The extension API version the plugin was written against
    /// (`api = "0.1"`), optional (PLAN-extension-api.md D1). A plugin that
    /// targets a newer API than this veles has is noted once ([`api_note`]).
    #[serde(default)]
    pub api: Option<String>,
}

fn default_kind() -> String {
    "tcl".into()
}
fn default_entry() -> String {
    "plugin.tcl".into()
}

/// What a plugin's `api` says against this build's extension API, when it
/// is worth saying: it targets a NEWER version than this veles has (what
/// it calls may not exist here), or its value is not a version. `None`
/// when it is absent, the same, or older: 0.x and every minor bump after
/// 1.0 only add names, and a major mismatch is refused from 1.0 on, not
/// noted.
pub fn api_note(m: &PluginManifest) -> Option<String> {
    let want = m.api.as_deref()?.trim();
    let parse = |v: &str| -> Option<(u32, u32)> {
        let (a, b) = v.split_once('.')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    };
    let have = veles_types::plugin::EXT_API_VERSION;
    let Some(w) = parse(want) else {
        return Some(format!(
            "plugin {}: api = {want:?} in its plugin.toml is not a <major>.<minor> version",
            m.name
        ));
    };
    let h = parse(have)?;
    (w > h).then(|| {
        format!(
            "plugin {} targets extension API {want}, and this veles has {have}: what it calls \
             may be missing here (docs/extension-api-changes.md says what each version added)",
            m.name
        )
    })
}

/// Log [`api_note`] once per plugin and value for the life of the
/// process: discovery runs on every listing and every watcher pass, and a
/// note repeated each time is noise nobody reads.
fn note_api_once(m: &PluginManifest) {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};
    static NOTED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let Some(note) = api_note(m) else { return };
    let mut seen = NOTED
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if seen.insert(note.clone()) {
        tracing::warn!("{note}");
    }
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
}

/// Every plugin directory in `plugins_dir()` that has a manifest,
/// regardless of whether config would load it.
///
/// Split out from `discover` because a MANAGER needs the ones that are
/// switched off: the desktop's plugin list was built on the filtered
/// call, so a disabled plugin vanished from the UI entirely and there
/// was no row left to click to switch it back on. Filtering is the
/// loader's business, not discovery's.
///
/// # Why this takes no directory
///
/// It used to take a `home` and look in `<home>/plugins`, while the
/// management half of this module (`plugin_new`, `snapshot`, `rollback`)
/// used `plugins_dir()`. The XDG split on 2026-09-04 moved plugins to
/// the CONFIG dir and the two halves silently diverged: five read-side
/// frontends were still passing `veles_home()` (now `data_dir()`), so
/// `veles plugins list`, both `/plugins` commands and the desktop's
/// hook-attribution map all read a directory that does not exist,
/// while the loader happily ran plugins out of the config dir. Nothing
/// caught it because this module had no tests at all.
///
/// The parameter is gone rather than corrected, so the wrong directory
/// is unrepresentable at a call site. Tests that need an explicit tree
/// use `discover_all_in`.
///
/// Sorted by name so the list does not reshuffle with directory order.
///
/// # Two roots
///
/// The user's `plugins_dir()` and the read-only BUNDLED tier
/// ([`bundled_roots`]), merged by manifest name with the user's copy
/// winning. The bundled tier is what lets a fresh install speak a wire
/// the engine has no native client for (the Anthropic Messages driver,
/// 0.1.2) without anyone copying a plugin into their config dir first.
/// Everything downstream of this — `discover`, the loader, every
/// listing — sees one merged list, so a bundled plugin obeys the same
/// `[plugins] enabled/disabled` rules as one the user wrote.
pub fn discover_all() -> Result<Vec<LoadedPlugin>> {
    discover_all_from(&plugins_dir(), &bundled_roots())
}

/// The variable naming extra bundled Tcl-plugin roots (a search path:
/// `:`-separated, `;` on Windows). The APK sets it to the plugins it
/// unpacks from `runtime.zip`; anything else finds its bundle beside
/// the binary and needs no variable.
pub const BUNDLED_ENV: &str = "VELES_BUNDLED_PLUGINS";

/// Where bundled Tcl plugins are looked for, first match wins per name:
/// `$VELES_BUNDLED_PLUGINS`, then `<exe>/../share/veles/plugins` (the
/// Linux archive and AppImage, the nix package and so the OCI image,
/// and the Windows zip, which all put the binary in `bin/`).
///
/// The same shape as the process tier's and the module tier's roots
/// (`pluginhost::default_roots`, `modulehost::default_roots`), minus a
/// data-dir entry: the USER's root for this tier is `plugins_dir()`,
/// which [`discover_all`] puts in front of these.
///
/// These roots are READ-ONLY to veles. Nothing here or in tclrt writes
/// to them: `snapshot`/`rollback` run git in `plugins_dir()` only,
/// `remove` refuses a bundled plugin by name, and the hot-reload watcher
/// watches `plugins_dir()`. A bundled root is usually a nix store path
/// or a directory inside an unpacked release, and an edit there would
/// be lost on the next upgrade anyway.
pub fn bundled_roots() -> Vec<PathBuf> {
    let mut v = crate::platform::split_search_path(BUNDLED_ENV);
    if let Some(prefix) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().and_then(Path::parent).map(Path::to_path_buf))
    {
        v.push(prefix.join("share").join("veles").join("plugins"));
    }
    v
}

/// `discover_all` against an explicit user root and explicit bundled
/// roots. A plugin in `user` shadows a bundled one with the same
/// manifest name, and an earlier bundled root shadows a later one. A
/// bundled root that is missing or unreadable contributes nothing and
/// hides nothing.
pub fn discover_all_from(user: &Path, bundled: &[PathBuf]) -> Result<Vec<LoadedPlugin>> {
    let mut out = discover_all_in(user)?;
    for root in bundled {
        // A bundled root that IS the user root (a misset variable) must
        // not make the user's plugins count twice.
        if root == user {
            continue;
        }
        let Ok(found) = discover_all_in(root) else {
            continue;
        };
        for p in found {
            if !out.iter().any(|q| q.manifest.name == p.manifest.name) {
                out.push(p);
            }
        }
    }
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    Ok(out)
}

/// Did this plugin come from a bundled root rather than the user's
/// `plugins_dir()`? For the listings, which say where a plugin came
/// from, and for `remove`, which refuses one.
pub fn is_bundled(p: &LoadedPlugin) -> bool {
    let Some(parent) = p.dir.parent() else {
        return false;
    };
    parent != plugins_dir() && bundled_roots().iter().any(|r| r == parent)
}

/// `discover_all` against an explicit plugins ROOT (not a home).
/// For tests and for anything deliberately inspecting a tree that is
/// not the live one.
pub fn discover_all_in(root: &Path) -> Result<Vec<LoadedPlugin>> {
    let root = root.to_path_buf();
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&root)? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("plugin.toml");
        if !manifest_path.exists() {
            continue;
        }
        // A malformed manifest must not hide every OTHER plugin: the
        // read used to `?` straight out of the loop, so one bad
        // plugin.toml emptied the whole list.
        let raw = match std::fs::read_to_string(&manifest_path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("plugin {}: unreadable manifest: {e}", dir.display());
                continue;
            }
        };
        match toml::from_str::<PluginManifest>(&raw) {
            Ok(manifest) => {
                note_api_once(&manifest);
                out.push(LoadedPlugin { manifest, dir })
            }
            Err(e) => tracing::warn!("plugin {}: bad manifest: {e}", dir.display()),
        }
    }
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    Ok(out)
}

/// Is this plugin one the loader should source?
///
/// `disabled` wins over `enabled`, and a non-empty `enabled` is an
/// allowlist. Stated once here so the loader and the UI's state column
/// cannot disagree about what "enabled" means.
pub fn is_enabled(name: &str, enabled: &[String], disabled: &[String]) -> bool {
    if disabled.iter().any(|d| d == name) {
        return false;
    }
    enabled.is_empty() || enabled.iter().any(|e| e == name)
}

/// The plugins the loader will actually source. Takes no directory for
/// the reason spelled out on `discover_all`.
pub fn discover(enabled: &[String], disabled: &[String]) -> Result<Vec<LoadedPlugin>> {
    // Both tiers (see `discover_all`), then the same filter.
    let mut all = discover_all()?;
    all.retain(|p| is_enabled(&p.manifest.name, enabled, disabled));
    Ok(all)
}

/// `discover` against an explicit plugins ROOT (not a home).
pub fn discover_in(
    root: &Path,
    enabled: &[String],
    disabled: &[String],
) -> Result<Vec<LoadedPlugin>> {
    let mut all = discover_all_in(root)?;
    all.retain(|p| is_enabled(&p.manifest.name, enabled, disabled));
    Ok(all)
}

// ── management ──────────────────────────────────────────────────────
//
// `discover` was the whole module: the desktop could list plugins and
// nothing else, so the Plugins overlay was a text dump with an
// enabled-dot it could not toggle. These are the operations behind the
// manager — enable/disable through config, and create/install/remove/
// rollback against the plugins directory, which is a git repo the
// runtime already snapshots on every change (see tclrt's
// `git_snapshot_plugins`). That repo IS the undo for everything here.

use anyhow::Context;
use std::path::Path as StdPath;

pub fn plugins_dir() -> PathBuf {
    crate::config::config_dir().join("plugins")
}

/// A plugin name that is safe as a single directory component.
///
/// Rejected rather than sanitized: a name silently rewritten to
/// something else creates a plugin the user cannot find by the name
/// they typed, and `..` in an accepted name would escape the directory.
pub fn validate_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        anyhow::bail!("a plugin needs a name");
    }
    if n.len() > 64 {
        anyhow::bail!("plugin name is too long (max 64)");
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("plugin name may only contain letters, digits, `-` and `_` (got `{n}`)");
    }
    Ok(n.to_string())
}

/// Run a git command in the plugins repo.
fn git(args: &[&str]) -> Result<std::process::Output> {
    let dir = plugins_dir();
    std::process::Command::new("git")
        // Plugin sources must round-trip byte for byte (a rollback
        // restores what was committed; the build key hashes bytes), so a
        // user-level `core.autocrlf = true` — Windows' default — must not
        // rewrite them on checkout.
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(&dir)
        .env("GIT_AUTHOR_NAME", "veles")
        .env("GIT_AUTHOR_EMAIL", "veles@local")
        .env("GIT_COMMITTER_NAME", "veles")
        .env("GIT_COMMITTER_EMAIL", "veles@local")
        .output()
        .with_context(|| format!("running git in {}", dir.display()))
}

/// Commit the current plugin tree. Mirrors the runtime's own snapshot
/// so a change made through the API is versioned the same way one made
/// by editing a file is — the rollback list must not have holes in it.
pub fn snapshot(message: &str) -> Result<()> {
    let dir = plugins_dir();
    if !dir.exists() {
        return Ok(());
    }
    if !dir.join(".git").exists() {
        let _ = git(&["init", "-q"])?;
    }
    let _ = git(&["add", "-A"])?;
    let status = git(&["status", "--porcelain"])?;
    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Ok(());
    }
    let out = git(&["commit", "-q", "-m", message])?;
    anyhow::ensure!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// One entry of the plugin directory's history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginCommit {
    pub commit: String,
    pub when: String,
    pub subject: String,
}

pub fn history(limit: usize) -> Result<Vec<PluginCommit>> {
    let dir = plugins_dir();
    if !dir.join(".git").exists() {
        return Ok(Vec::new());
    }
    let n = format!("-{}", limit.clamp(1, 200));
    let out = git(&[
        "log",
        &n,
        "--date=iso-strict",
        "--pretty=format:%H%x1f%ad%x1f%s",
    ])?;
    if !out.status.success() {
        anyhow::bail!("git log failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\u{1f}');
            Some(PluginCommit {
                commit: parts.next()?.to_string(),
                when: parts.next()?.to_string(),
                subject: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// Restore the whole plugin tree to a commit. Snapshots first, so the
/// state being rolled back FROM is itself recoverable — otherwise the
/// undo button is a one-way door.
pub fn rollback(commit: &str) -> Result<()> {
    let dir = plugins_dir();
    anyhow::ensure!(
        dir.join(".git").exists(),
        "no plugin git history ({} has no .git)",
        dir.display()
    );
    let c = commit.trim();
    anyhow::ensure!(
        !c.is_empty()
            && c.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_./^~".contains(ch)),
        "`{commit}` is not a commit-ish"
    );
    snapshot("veles: state before rollback")?;
    let out = git(&["checkout", c, "--", "."])?;
    anyhow::ensure!(
        out.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    snapshot(&format!("veles: rolled back to {c}"))?;
    Ok(())
}

/// Create a working, minimal Tcl plugin and return its directory.
///
/// The stub REGISTERS a hook rather than being an empty file: a new
/// plugin that does nothing teaches nothing, and the one thing a plugin
/// author needs to see is the `::veles::hook` call and where its
/// arguments arrive.
pub fn scaffold(name: &str, description: &str) -> Result<PathBuf> {
    scaffold_kind(name, description, "tcl")
}

/// `scaffold` for a plugin of `kind`: `tcl` (the default), `js` — a
/// TypeScript plugin the lang-js host runs — `racket`, a Racket module
/// the lang-racket host runs, or `lisp`, a Common Lisp file the
/// lang-lisp host loads (R9).
pub fn scaffold_kind(name: &str, description: &str, kind: &str) -> Result<PathBuf> {
    match kind {
        "tcl" => scaffold_tcl(name, description),
        "js" => scaffold_js(name, description),
        "racket" => scaffold_racket(name, description),
        "lisp" => scaffold_lisp(name, description),
        other => anyhow::bail!("unknown plugin kind `{other}` (tcl, js, racket, lisp, overlay)"),
    }
}

/// A Common Lisp plugin: a file the lang-lisp host loads into a package
/// of its own that uses COMMON-LISP and VELES, registering one tool and
/// one hook. Handlers run in threads of their own; NIL from a hook is
/// "no opinion".
fn scaffold_lisp(name: &str, description: &str) -> Result<PathBuf> {
    let name = validate_name(name)?;
    let dir = plugins_dir().join(&name);
    anyhow::ensure!(!dir.exists(), "{name} already exists");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let desc = if description.trim().is_empty() {
        "a new veles plugin".to_string()
    } else {
        description.trim().replace('"', "'")
    };
    let manifest = format!(
        "name = \"{name}\"\nkind = \"lisp\"\nentry = \"plugin.lisp\"\ndescription = \"{desc}\"\nprovides_hooks = [\"post_llm_call\"]\n"
    );
    std::fs::write(dir.join("plugin.toml"), manifest)?;
    let ident = name.replace('-', "_");
    let entry = format!(
        ";;;; {name} — {desc}\n\
         ;;;;\n\
         ;;;; A veles plugin in Common Lisp (kind = \"lisp\"): the lang-lisp host\n\
         ;;;; loads this file into a package of its own that uses COMMON-LISP and\n\
         ;;;; VELES, and again on every reload. VELES is the API (docs/plugins.md,\n\
         ;;;; \"Common Lisp\"); a handler runs in a thread of its own and answers a\n\
         ;;;; string or JSON (a hash table: see VELES:OBJ).\n\
         \n\
         (register-tool \"{ident}_hello\" \"say hello from {name}\"\n\
         \x20 (obj \"type\" \"object\" \"properties\" (obj \"who\" (obj \"type\" \"string\")))\n\
         \x20 (lambda (args session-id)\n\
         \x20   (declare (ignore session-id))\n\
         \x20   (format nil \"hello, ~a\" (gethash \"who\" args \"world\"))))\n\
         \n\
         (hook \"post_llm_call\"\n\
         \x20 (lambda (args)\n\
         \x20   ;; Answer NIL for \"no opinion\", or a verdict (obj ...).\n\
         \x20   (declare (ignore args))\n\
         \x20   nil))\n"
    );
    std::fs::write(dir.join("plugin.lisp"), entry)?;
    snapshot(&format!("veles: scaffold plugin {name}"))?;
    Ok(dir)
}

/// A Racket plugin: a `#lang racket/base` module that requires the
/// `veles` collection the lang-racket host provides, registering one
/// tool and one hook. Handlers run as Racket threads under the plugin's
/// own custodian; `(void)` from a hook is "no opinion".
fn scaffold_racket(name: &str, description: &str) -> Result<PathBuf> {
    let name = validate_name(name)?;
    let dir = plugins_dir().join(&name);
    anyhow::ensure!(!dir.exists(), "{name} already exists");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let desc = if description.trim().is_empty() {
        "a new veles plugin".to_string()
    } else {
        description.trim().replace('"', "'")
    };
    let manifest = format!(
        "name = \"{name}\"\nkind = \"racket\"\nentry = \"plugin.rkt\"\ndescription = \"{desc}\"\nprovides_hooks = [\"post_llm_call\"]\n"
    );
    std::fs::write(dir.join("plugin.toml"), manifest)?;
    let ident = name.replace('-', "_");
    let entry = format!(
        "#lang racket/base\n\
         ;; {name} — {desc}\n\
         ;;\n\
         ;; A veles plugin in Racket (kind = \"racket\"): the lang-racket host\n\
         ;; instantiates this module in a namespace of its own, under a\n\
         ;; custodian of its own, and again on every reload. `veles` is the\n\
         ;; API (docs/plugins.md, \"Racket\"); a handler runs as a Racket\n\
         ;; thread and answers a string or a jsexpr.\n\
         (require veles)\n\
         \n\
         (register-tool \"{ident}_hello\" \"say hello from {name}\"\n\
         \x20 (hash 'type \"object\" 'properties (hash 'who (hash 'type \"string\")))\n\
         \x20 (lambda (args session-id)\n\
         \x20   (format \"hello, ~a\" (hash-ref args 'who \"world\"))))\n\
         \n\
         (hook \"post_llm_call\"\n\
         \x20 (lambda (args)\n\
         \x20   ;; Answer (void) for \"no opinion\", or a verdict hash.\n\
         \x20   (void)))\n"
    );
    std::fs::write(dir.join("plugin.rkt"), entry)?;
    snapshot(&format!("veles: scaffold plugin {name}"))?;
    Ok(dir)
}

/// A TypeScript plugin: the manifest names the lang-js host's kind, the
/// entry registers one tool and one hook through the `veles` object.
fn scaffold_js(name: &str, description: &str) -> Result<PathBuf> {
    let name = validate_name(name)?;
    let dir = plugins_dir().join(&name);
    anyhow::ensure!(!dir.exists(), "{name} already exists");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let desc = if description.trim().is_empty() {
        "a new veles plugin".to_string()
    } else {
        description.trim().replace('"', "'")
    };
    let manifest = format!(
        "name = \"{name}\"\nkind = \"js\"\nentry = \"plugin.ts\"\ndescription = \"{desc}\"\nprovides_hooks = [\"post_llm_call\"]\n"
    );
    std::fs::write(dir.join("plugin.toml"), manifest)?;
    let ident = name.replace('-', "_");
    let entry = format!(
        "// {name} — {desc}\n\
         //\n\
         // A veles plugin in TypeScript (kind = \"js\"): the lang-js host\n\
         // evaluates it as an ES module in a context of its own, and in a\n\
         // FRESH context on every reload, so top-level code runs once per\n\
         // load. `veles` is the API (docs/plugins.md, \"JavaScript /\n\
         // TypeScript\"); every call answers a Promise.\n\
         \n\
         await veles.registerTool(\n\
         \x20 {{\n\
         \x20   name: \"{ident}_hello\",\n\
         \x20   description: \"say hello from {name}\",\n\
         \x20   schema: {{ type: \"object\", properties: {{ who: {{ type: \"string\" }} }} }},\n\
         \x20 }},\n\
         \x20 async (args: {{ who?: string }}) => `hello, ${{args.who ?? \"world\"}}`,\n\
         );\n\
         \n\
         veles.hook(\"post_llm_call\", (args: {{ content?: string }}) => {{\n\
         \x20 // Answer nothing for \"no opinion\", or a verdict object.\n\
         \x20 return undefined;\n\
         }});\n"
    );
    std::fs::write(dir.join("plugin.ts"), entry)?;
    snapshot(&format!("veles: scaffold plugin {name}"))?;
    Ok(dir)
}

fn scaffold_tcl(name: &str, description: &str) -> Result<PathBuf> {
    let name = validate_name(name)?;
    let dir = plugins_dir().join(&name);
    anyhow::ensure!(!dir.exists(), "{name} already exists");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let desc = if description.trim().is_empty() {
        "a new veles plugin".to_string()
    } else {
        description.trim().replace('"', "'")
    };
    let manifest = format!(
        "name = \"{name}\"\nkind = \"tcl\"\nentry = \"plugin.tcl\"\ndescription = \"{desc}\"\nprovides_hooks = [\"post_llm_call\"]\n"
    );
    std::fs::write(dir.join("plugin.toml"), manifest)?;

    let proc_name = name.replace('-', "_");
    let entry = format!(
        "# {name} — {desc}\n\
         #\n\
         # Sourced into the gateway's Tcl interpreter at boot and re-sourced on\n\
         # every reload, so it must be IDEMPOTENT: redefining a proc is fine,\n\
         # appending to a global list twice is not.\n\
         #\n\
         # Hook arguments arrive as JSON in ::veles::hook_args. Returning a\n\
         # non-empty string wins the hook (hermes transform semantics);\n\
         # returning nothing lets the next handler, and then the engine,\n\
         # carry on.\n\
         \n\
         proc ::{proc_name}::handle {{}} {{\n\
         \x20   # set args $::veles::hook_args\n\
         \x20   return \"\"\n\
         }}\n\
         \n\
         namespace eval ::{proc_name} {{}}\n\
         ::veles::hook post_llm_call ::{proc_name}::handle\n"
    );
    std::fs::write(dir.join("plugin.tcl"), entry)?;
    snapshot(&format!("veles: scaffold plugin {name}"))?;
    Ok(dir)
}

/// The files of a Rust OVERLAY module's own repository
/// (PLAN-extension-api.md E5): a dylib crate that veles' flake
/// (`lib.<system>.velesWithModules`) builds inside veles' own cargo
/// invocation, so it loads into that build's gateway. Not a plugin
/// under `plugins_dir`: it is a repository of its own, wherever its
/// author keeps it. `examples/overlay-module` is this for `greeter`,
/// byte for byte (the test below).
///
/// The name is lowercase, `[a-z][a-z0-9-]*`: it is the module's name,
/// the crate's (`veles-overlay-<name>`) and the flake's
/// `crates/overlay-<name>`, and a cargo package name with an uppercase
/// letter or `_` would make the three differ.
pub fn overlay_files(name: &str, description: &str) -> Result<Vec<(&'static str, String)>> {
    let name = name.trim();
    anyhow::ensure!(
        name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "an overlay module's name is lowercase letters, digits and `-`, starting with a letter \
         (got `{name}`)"
    );
    let desc = match description.trim() {
        "" => "an overlay module for veles".to_string(),
        d => d
            .replace(['"', '\\'], "'")
            .replace(['\n', '\r'], " ")
            .trim_end_matches('.')
            .to_string(),
    };
    let ident = name.replace('-', "_");
    let fill = |t: &str| {
        t.replace("@NAME@", name)
            .replace("@IDENT@", &ident)
            .replace("@DESC@", &desc)
    };
    Ok(vec![
        ("Cargo.toml", fill(OVERLAY_CARGO)),
        ("src/lib.rs", fill(OVERLAY_LIB)),
        ("flake.nix", fill(OVERLAY_FLAKE)),
        ("README.md", fill(OVERLAY_README)),
    ])
}

/// Write [`overlay_files`] into `<parent>/<name>` and return it.
pub fn scaffold_overlay(name: &str, description: &str, parent: &Path) -> Result<PathBuf> {
    let files = overlay_files(name, description)?;
    let dir = parent.join(name.trim());
    anyhow::ensure!(!dir.exists(), "{} already exists", dir.display());
    for (rel, text) in files {
        let path = dir.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(dir)
}

const OVERLAY_CARGO: &str = r#"# @NAME@: a veles OVERLAY module, a Rust module in a repository of its
# own. veles' flake (`lib.<system>.velesWithModules`) builds it INSIDE
# veles' own cargo invocation, so it loads into the gateway that build
# makes: a module loads only into the build it came from.
#
# Not part of veles' stable extension API: a module calls veles' Rust
# internals, so a veles update can break this build. When an override
# point, a script, a `.so` or a process plugin can do the job, prefer it
# (veles' docs/extending.md).
[package]
name = "veles-overlay-@NAME@"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
# A module is a Rust dylib on libveles_runtime.so, never a cdylib.
crate-type = ["dylib"]

[dependencies]
# veles' runtime, from the workspace the flake builds this crate in.
veles-runtime = { workspace = true }
# Only crates veles' Cargo.lock already has: the build is offline.
async-trait = "0.1"
serde_json = "1"
"#;

const OVERLAY_LIB: &str = r#"//! @NAME@: @DESC@.
//!
//! A veles overlay module (veles' docs/extending.md, "Rust overlay
//! modules"). The module tier's contract (docs/plugins.md, "Modules"):
//! declare the module with `veles_core::export!`, register what it
//! provides on every start (a reload purges it and starts again), and
//! never spawn a thread or a runtime of its own.

use std::sync::Arc;

use veles_runtime::veles_core;
use veles_runtime::veles_types::plugin::Manifest;

fn manifest() -> Manifest {
    Manifest {
        name: "@NAME@".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: veles_runtime::veles_types::plugin::PROTOCOL,
        description: "@DESC@".into(),
        // The rest at its default: one instance, no lease, no platform,
        // no services, no override points.
        ..Default::default()
    }
}

#[derive(Default)]
pub struct Module;

impl veles_core::rustplugins::VelesPlugin for Module {
    fn name(&self) -> &str {
        "@NAME@"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn tools(&self) -> Vec<Arc<dyn veles_core::tools::Tool>> {
        vec![Arc::new(Hello) as Arc<dyn veles_core::tools::Tool>]
    }
}

struct Hello;

#[async_trait::async_trait]
impl veles_core::tools::Tool for Hello {
    fn name(&self) -> &str {
        "@IDENT@_hello"
    }
    fn description(&self) -> &str {
        "say hello from @NAME@"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"who": {"type": "string"}}})
    }
    async fn call(&self, args: serde_json::Value) -> String {
        let who = args["who"].as_str().unwrap_or("world");
        format!("hello, {who}, from @NAME@")
    }
}

veles_core::export!(Module, manifest());
"#;

const OVERLAY_FLAKE: &str = r#"# @NAME@: a veles overlay module in a repository of its own. This flake
# builds a whole veles package with the module in it:
#
#   nix build     # result/bin/veles, result/lib/*.so (this module too)
#
# `inputs.veles` is the veles you build against; flake.lock pins its rev.
# `nix flake update veles` moves to a newer veles, and the next build
# rebuilds this module against it with no step of yours. It fails only
# if veles' Rust internals changed under the module.
{
  description = "@DESC@";

  # The veles you build against: veles' repository, or your fork
  # (github:<you>/veles), or a checkout (git+file:///path/to/veles).
  inputs.veles.url = "github:77787777778777/veles";

  outputs =
    { veles, ... }:
    {
      packages.x86_64-linux.default = veles.lib.x86_64-linux.velesWithModules {
        modules = [
          {
            name = "@NAME@";
            src = ./.;
          }
        ];
      };
    };
}
"#;

const OVERLAY_README: &str = r#"# @NAME@

@DESC@.

A veles overlay module: a Rust module that lives in a repository of its
own and is built inside veles' own build, so it loads into the gateway
that build makes. veles' docs/extending.md, "Rust overlay modules", has
the whole story.

- `Cargo.toml`: a `dylib` crate. It names `veles-runtime = { workspace = true }`
  and only crates veles' `Cargo.lock` already has.
- `src/lib.rs`: the module, with one tool, `@IDENT@_hello`.
- `flake.nix`: builds a whole veles package with this module in it,
  against veles' repository. Point `inputs.veles.url` at your fork or
  checkout if you run one.

## Build and install

    nix build

Everything in `result/` comes from one build, and only that build's
gateway loads this module. Run `result/bin/veles`, and install every
module you use from `result/lib/`, the stock ones too: the gateway
refuses a module from any other build ("core changed").

    dir=${XDG_DATA_HOME:-$HOME/.local/share}/veles/modules/@NAME@
    so=result/lib/libveles_overlay_@IDENT@.so
    sha=$(sha256sum $so | cut -d' ' -f1)
    install -d -m 0700 $dir
    install -m 0600 $so $dir/$sha.so
    ln -sfn $sha.so $dir/current
    printf 'name = "@NAME@"\nkind = "module"\nentry = "module.so"\n' > $dir/plugin.toml
    result/bin/veles plugins rescan

## Updating veles

    nix flake update veles
    nix build

Install the modules again and restart the gateway on the new
`result/bin/veles`: the new gateway refuses the old build's modules.

## Not stable

This tier is not part of veles' stable extension API. It calls veles'
Rust internals, so a veles update can break its build. When an override
point, a script, a `.so` or a process plugin can do the job, prefer it.
"#;

/// Clone a plugin repo into the plugins directory.
pub fn install(url: &str, as_name: Option<&str>) -> Result<PathBuf> {
    install_via(url, as_name, &std::env::temp_dir())
}

/// Removes the staging clone however `install` leaves — the early
/// `remove_dir_all` calls covered the explicit refusals, but every `?`
/// (a manifest that does not parse, a bad `as_name`, a failed copy)
/// used to leave `veles-plugin-install-<stamp>` in $TMPDIR for good.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// [`install`], staging the clone under `staging` (tests pass their own
/// directory so they can see what is left behind).
fn install_via(url: &str, as_name: Option<&str>, staging: &std::path::Path) -> Result<PathBuf> {
    let root = plugins_dir();
    std::fs::create_dir_all(&root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = staging.join(format!("veles-plugin-install-{stamp}"));
    let _ = std::fs::remove_dir_all(&tmp);
    let _staged = RemoveOnDrop(tmp.clone());
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "core.autocrlf=false",
            "clone",
            "--quiet",
            "--depth",
            "1",
            url,
            &tmp.to_string_lossy(),
        ])
        .status()
        .context("cannot run git")?;
    if !status.success() {
        anyhow::bail!("git clone failed for {url}");
    }

    // A plugin is a directory with a manifest. Validate BEFORE moving it
    // into place: half-installing something the loader will skip leaves
    // a directory nobody can explain.
    let manifest_path = tmp.join("plugin.toml");
    if !manifest_path.exists() {
        anyhow::bail!(
            "no plugin.toml at the root of {url} — that is what makes a directory a plugin"
        );
    }
    let manifest: PluginManifest = toml::from_str(&std::fs::read_to_string(&manifest_path)?)
        .with_context(|| format!("plugin.toml in {url} is not a valid manifest"))?;
    let name = validate_name(as_name.unwrap_or(&manifest.name))?;
    let dest = root.join(&name);
    if dest.exists() {
        anyhow::bail!("{name} is already installed — remove it first");
    }
    // The clone's own .git would nest a repo inside the plugins repo.
    let _ = std::fs::remove_dir_all(tmp.join(".git"));
    copy_tree(&tmp, &dest)?;
    snapshot(&format!("veles: install plugin {name} from {url}"))?;
    Ok(dest)
}

/// Delete a plugin directory. The git snapshot taken first is the undo.
pub fn remove(name: &str) -> Result<PathBuf> {
    let name = validate_name(name)?;
    let dir = plugins_dir().join(&name);
    if !dir.is_dir() {
        // Bundled with veles, not the user's: its directory belongs to
        // the install (often a nix store path), and deleting it would
        // either fail or come back with the next upgrade. Say how to
        // switch it off instead.
        if let Some(b) = discover_all()
            .unwrap_or_default()
            .into_iter()
            .find(|p| p.manifest.name == name && is_bundled(p))
        {
            anyhow::bail!(
                "{name} is bundled with veles ({}) and is not removed; switch it off with \
                 `[plugins] disabled = [\"{name}\"]` in config.toml, or put your own copy in {} \
                 to replace it",
                b.dir.display(),
                plugins_dir().join(&name).display()
            );
        }
        anyhow::bail!("no such plugin: {name}");
    }
    snapshot(&format!("veles: state before removing {name}"))?;
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    snapshot(&format!("veles: remove plugin {name}"))?;
    Ok(dir)
}

fn copy_tree(from: &StdPath, to: &StdPath) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// What a script plugin declares about services in its plugin.toml
/// (`crate::services`): read beside [`PluginManifest`], so the manifest
/// type stays what every tier's loader already shares.
///
/// ```toml
/// provides_services = ["weather.now"]       # it registers these
/// uses_services     = ["appview.query"]     # it needs these
/// optional_services = ["triggers.forward"]  # it works without these
/// ```
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ServiceDeclarations {
    #[serde(default)]
    pub provides_services: Vec<String>,
    #[serde(default)]
    pub uses_services: Vec<String>,
    #[serde(default)]
    pub optional_services: Vec<String>,
}

impl ServiceDeclarations {
    /// Read them from a plugin.toml's text; nothing declared on a parse
    /// error (the loader reports the manifest's own errors).
    pub fn parse(toml_text: &str) -> Self {
        toml::from_str(toml_text).unwrap_or_default()
    }

    /// Record them for the dependency view under the plugin's name.
    pub fn declare(&self, plugin: &str) {
        let provides = self
            .provides_services
            .iter()
            .map(|n| crate::services::ServiceDecl {
                name: n.clone(),
                ..Default::default()
            })
            .collect();
        let uses = self
            .uses_services
            .iter()
            .map(|n| (n, false))
            .chain(self.optional_services.iter().map(|n| (n, true)))
            .map(|(n, optional)| crate::services::UseDecl {
                name: n.clone(),
                optional,
                description: String::new(),
            })
            .collect();
        crate::services::declare(plugin, provides, uses);
    }
}

/// Declare a loaded plugin's services (`plugin.toml` beside its entry).
pub fn declare_services(p: &LoadedPlugin) {
    let text = std::fs::read_to_string(p.dir.join("plugin.toml")).unwrap_or_default();
    ServiceDeclarations::parse(&text).declare(&p.manifest.name);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a manifest (and an entry file) for one plugin under `root`.
    fn plant(root: &Path, name: &str, manifest: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("plugin dir");
        std::fs::write(dir.join("plugin.toml"), manifest).expect("manifest");
        std::fs::write(dir.join("plugin.tcl"), "# nothing\n").expect("entry");
    }

    fn named(found: &[LoadedPlugin]) -> Vec<String> {
        found.iter().map(|p| p.manifest.name.clone()).collect()
    }

    /// The overlay scaffold for `greeter` IS examples/overlay-module, byte
    /// for byte, and nothing else is there: the flake builds that example
    /// (`packages.veles-overlay-example`), so what `plugins new --kind
    /// overlay` writes is what was built. Change one, change the other.
    #[test]
    fn the_overlay_scaffold_is_the_example() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let example = repo.join("examples").join("overlay-module");
        let files = overlay_files("greeter", "").expect("scaffold");
        for (rel, text) in &files {
            let on_disk = std::fs::read_to_string(example.join(rel))
                .unwrap_or_else(|e| panic!("examples/overlay-module/{rel}: {e}"));
            assert!(
                on_disk == *text,
                "examples/overlay-module/{rel} differs from `overlay_files(\"greeter\", \"\")`: \
                 regenerate it with `veles plugins new greeter --kind overlay` (or change the \
                 template in plugins.rs)"
            );
        }
        let mut listed: Vec<String> = Vec::new();
        let mut stack = vec![example.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).expect("read example") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p
                        .strip_prefix(&example)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    listed.push(rel);
                }
            }
        }
        listed.sort();
        let mut expected: Vec<String> = files.iter().map(|(r, _)| r.to_string()).collect();
        expected.sort();
        assert_eq!(
            listed, expected,
            "examples/overlay-module holds exactly the scaffold"
        );
    }

    /// The name is the module's, the crate's and the flake's directory:
    /// lowercase, digits and `-`, starting with a letter. A scaffold never
    /// writes over a directory that exists.
    #[test]
    fn the_overlay_scaffold_checks_its_name_and_never_overwrites() {
        for bad in ["", "Greeter", "my_mod", "1st", "-x", "a b", "a/b"] {
            assert!(overlay_files(bad, "").is_err(), "`{bad}` must be refused");
        }
        let files = overlay_files("my-mod", "says \"hi\"\nloudly.").expect("scaffold");
        let lib = &files.iter().find(|(r, _)| *r == "src/lib.rs").unwrap().1;
        assert!(lib.contains("fn name(&self) -> &str {\n        \"my_mod_hello\""));
        assert!(lib.contains("description: \"says 'hi' loudly\".into()"));
        let cargo = &files.iter().find(|(r, _)| *r == "Cargo.toml").unwrap().1;
        assert!(cargo.contains("name = \"veles-overlay-my-mod\""));
        let readme = &files.iter().find(|(r, _)| *r == "README.md").unwrap().1;
        assert!(readme.contains("so=result/lib/libveles_overlay_my_mod.so"));

        let parent = tempfile::tempdir().expect("tempdir");
        let dir = scaffold_overlay("my-mod", "", parent.path()).expect("first");
        assert!(dir.join("src/lib.rs").is_file() && dir.join("flake.nix").is_file());
        std::fs::write(dir.join("Cargo.toml"), "mine").unwrap();
        assert!(scaffold_overlay("my-mod", "", parent.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(),
            "mine",
            "the second scaffold wrote nothing"
        );
    }

    /// `api` in plugin.toml is read, and a plugin written for a newer
    /// extension API than this build's is noted; the same, an older one
    /// and none at all are not; a value that is not a version is.
    #[test]
    fn a_plugin_targeting_a_newer_api_is_noted() {
        let root = tempfile::tempdir().expect("tempdir");
        plant(root.path(), "newer", "name = \"newer\"\napi = \"99.0\"\n");
        plant(
            root.path(),
            "same",
            &format!(
                "name = \"same\"\napi = \"{}\"\n",
                veles_types::plugin::EXT_API_VERSION
            ),
        );
        plant(root.path(), "older", "name = \"older\"\napi = \"0.0\"\n");
        plant(root.path(), "silent", "name = \"silent\"\n");
        plant(
            root.path(),
            "garbled",
            "name = \"garbled\"\napi = \"soon\"\n",
        );
        let found = discover_all_in(root.path()).expect("discover");
        let note = |n: &str| {
            api_note(
                &found
                    .iter()
                    .find(|p| p.manifest.name == n)
                    .expect(n)
                    .manifest,
            )
        };
        let newer = note("newer").expect("a newer API is noted");
        assert!(newer.contains("targets extension API 99.0"), "{newer}");
        assert_eq!(note("same"), None);
        assert_eq!(note("older"), None);
        assert_eq!(note("silent"), None);
        assert!(note("garbled")
            .expect("a non-version is noted")
            .contains("not a <major>.<minor>"));
    }

    /// THE regression test for the XDG split.
    ///
    /// Plugins live in the CONFIG dir. Discovery must read that and only
    /// that. Until 2026-09-04 `discover`/`discover_all` took a `home`
    /// and five frontends handed them `veles_home()` (= `data_dir()`),
    /// so `veles plugins list`, `/plugins` on two surfaces and the
    /// desktop's hook-attribution map all read a directory that did not
    /// exist while the loader ran plugins out of the config dir.
    ///
    /// Note what this test can and cannot prove. It pins `plugins_dir()`
    /// and both `discover*` functions to the config side; it CANNOT pin
    /// the frontends, because the parameter they used to get wrong no
    /// longer exists — re-adding it is a compile error, which is a
    /// stronger guarantee than any assertion here.
    #[test]
    fn discovery_reads_the_config_dir_and_never_the_data_dir() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let config_root = crate::config::config_dir();
        let data_root = crate::config::data_dir();
        assert_ne!(
            config_root, data_root,
            "the fixture must hold the two halves apart or this test proves nothing"
        );

        // The plugin is in the config dir, where the loader looks.
        plant(
            &config_root.join("plugins"),
            "in-config",
            "name = \"in-config\"\n",
        );
        // A decoy in the data dir: the layout that used to be searched.
        plant(
            &data_root.join("plugins"),
            "in-data",
            "name = \"in-data\"\n",
        );

        assert_eq!(plugins_dir(), config_root.join("plugins"));
        assert_eq!(named(&discover_all().expect("discover_all")), ["in-config"]);
        assert_eq!(
            named(&discover(&[], &[]).expect("discover")),
            ["in-config"],
            "the data-dir decoy must be invisible"
        );
    }

    /// The bundled tier (0.1.2): a second root merged by manifest name,
    /// the user's copy shadowing the bundled one, the first bundled root
    /// shadowing a later one, and a missing bundled root hiding nothing.
    #[test]
    fn the_bundled_tier_merges_and_the_users_copy_wins() {
        let user = tempfile::tempdir().expect("user root");
        let first = tempfile::tempdir().expect("bundled root 1");
        let second = tempfile::tempdir().expect("bundled root 2");
        plant(user.path(), "mine", "name = \"mine\"\n");
        plant(user.path(), "shared", "name = \"shared\"\n");
        plant(first.path(), "shared", "name = \"shared\"\n");
        plant(first.path(), "driver", "name = \"driver\"\n");
        plant(second.path(), "driver", "name = \"driver\"\n");
        plant(second.path(), "late", "name = \"late\"\n");
        let missing = user.path().join("no-such-root");

        let found = discover_all_from(
            user.path(),
            &[
                missing,
                first.path().to_path_buf(),
                second.path().to_path_buf(),
            ],
        )
        .expect("discover");
        assert_eq!(named(&found), ["driver", "late", "mine", "shared"]);
        let dir_of = |n: &str| {
            found
                .iter()
                .find(|p| p.manifest.name == n)
                .map(|p| p.dir.clone())
                .expect("present")
        };
        assert_eq!(
            dir_of("shared"),
            user.path().join("shared"),
            "the user's copy wins"
        );
        assert_eq!(
            dir_of("driver"),
            first.path().join("driver"),
            "first bundled root wins"
        );
        assert_eq!(dir_of("late"), second.path().join("late"));

        // The user root named again as a bundled root counts once.
        let twice = discover_all_from(user.path(), &[user.path().to_path_buf()]).expect("again");
        assert_eq!(named(&twice), ["mine", "shared"]);
    }

    /// The live path: `$VELES_BUNDLED_PLUGINS` reaches `discover_all`,
    /// so a bundled plugin is listed, loaded and filtered exactly like
    /// a user one — `[plugins] disabled` switches it off — and is marked
    /// as bundled until a user copy of the same name replaces it.
    #[test]
    fn a_bundled_plugin_is_discovered_filtered_and_shadowed() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let bundled = tempfile::tempdir().expect("bundled root");
        plant(bundled.path(), "shipped", "name = \"shipped\"\n");
        std::env::set_var(BUNDLED_ENV, bundled.path());
        assert!(bundled_roots().contains(&bundled.path().to_path_buf()));

        let found = discover_all().expect("discover_all");
        assert_eq!(named(&found), ["shipped"]);
        assert!(is_bundled(&found[0]), "{:?}", found[0].dir);
        assert_eq!(named(&discover(&[], &[]).expect("discover")), ["shipped"]);
        assert!(
            discover(&[], &["shipped".to_string()])
                .expect("discover")
                .is_empty(),
            "[plugins] disabled must reach a bundled plugin"
        );
        assert!(
            discover(&["other".to_string()], &[])
                .expect("discover")
                .is_empty(),
            "an allowlist must reach a bundled plugin"
        );

        // A user copy shadows it, and is the user's.
        plant(&plugins_dir(), "shipped", "name = \"shipped\"\n");
        let found = discover_all().expect("discover_all");
        assert_eq!(named(&found), ["shipped"]);
        assert_eq!(found[0].dir, plugins_dir().join("shipped"));
        assert!(!is_bundled(&found[0]));
    }

    /// `remove` touches the user's root only. A bundled plugin is
    /// refused by name with the way to switch it off, and its files stay.
    #[test]
    fn a_bundled_plugin_is_refused_by_remove_and_left_in_place() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let bundled = tempfile::tempdir().expect("bundled root");
        plant(bundled.path(), "shipped", "name = \"shipped\"\n");
        std::env::set_var(BUNDLED_ENV, bundled.path());

        let err = remove("shipped").expect_err("a bundled plugin is not removable");
        let msg = err.to_string();
        assert!(msg.contains("bundled"), "{msg}");
        assert!(msg.contains("[plugins] disabled"), "{msg}");
        assert!(bundled.path().join("shipped/plugin.toml").exists());
        // A name that is nowhere is still the plain answer.
        let err = remove("nowhere").expect_err("absent");
        assert!(err.to_string().contains("no such plugin"), "{err}");
    }

    /// `kind` and `entry` are what the loader branches on: a manifest
    /// that omits them must mean "tcl" and "plugin.tcl", not empty
    /// strings that make the loader skip the plugin as an unknown kind.
    #[test]
    fn a_minimal_manifest_defaults_to_a_tcl_plugin() {
        let root = tempfile::tempdir().expect("tempdir");
        plant(root.path(), "bare", "name = \"bare\"\n");
        let found = discover_all_in(root.path()).expect("discover");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.kind, "tcl");
        assert_eq!(found[0].manifest.entry, "plugin.tcl");
        assert!(found[0].manifest.provides_hooks.is_empty());
        assert_eq!(found[0].manifest.description, "");
    }

    /// One unparseable manifest must not empty the whole list — the
    /// read used to `?` straight out of the loop. Also pins the sort,
    /// since a list that reshuffles with directory order makes a
    /// manager's rows move under the cursor.
    #[test]
    fn a_bad_manifest_hides_only_itself_and_the_rest_stay_sorted() {
        let root = tempfile::tempdir().expect("tempdir");
        plant(root.path(), "zebra", "name = \"zebra\"\n");
        plant(root.path(), "broken", "name = = = not toml\n");
        plant(root.path(), "alpha", "name = \"alpha\"\n");
        // A directory with no manifest at all is not a plugin.
        std::fs::create_dir_all(root.path().join("just-a-dir")).expect("dir");
        assert_eq!(
            named(&discover_all_in(root.path()).expect("discover")),
            ["alpha", "zebra"]
        );
    }

    /// `disabled` beats `enabled`, and a non-empty `enabled` is an
    /// allowlist. `discover_all` ignores both — a manager needs the
    /// switched-off rows or there is nothing left to click.
    #[test]
    fn filtering_is_the_loaders_business_not_discoverys() {
        let root = tempfile::tempdir().expect("tempdir");
        for n in ["a", "b", "c"] {
            plant(root.path(), n, &format!("name = \"{n}\"\n"));
        }
        let all = discover_all_in(root.path()).expect("all");
        assert_eq!(named(&all), ["a", "b", "c"]);

        let off = vec!["b".to_string()];
        assert_eq!(
            named(&discover_in(root.path(), &[], &off).expect("disabled")),
            ["a", "c"]
        );
        let only = vec!["c".to_string()];
        assert_eq!(
            named(&discover_in(root.path(), &only, &[]).expect("allowlist")),
            ["c"]
        );
        // Named in both: disabled wins.
        assert!(
            named(&discover_in(root.path(), &only, &only).expect("both")).is_empty(),
            "`disabled` must beat `enabled`"
        );
    }

    /// A name becomes a single directory component, so it is rejected
    /// rather than sanitized: `..` would escape the plugins directory,
    /// and a name silently rewritten creates a plugin the user cannot
    /// find by the name they typed.
    #[test]
    fn a_plugin_name_is_rejected_not_sanitized() {
        assert_eq!(
            validate_name("  good-name_1 ").expect("trimmed"),
            "good-name_1"
        );
        for bad in [
            "",
            "   ",
            "..",
            "a/b",
            "a..b/../c",
            "with space",
            "quote\"d",
            "x".repeat(65).as_str(),
        ] {
            assert!(
                validate_name(bad).is_err(),
                "`{bad}` must be refused as a directory component"
            );
        }
    }

    /// The scaffold is the template a new plugin author reads first, so
    /// it has to be a plugin the loader actually picks up — a stub that
    /// discovery skips teaches the wrong thing twice.
    #[test]
    fn a_scaffolded_plugin_is_discoverable_and_removable() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let dir = scaffold("fresh-one", "does a thing").expect("scaffold");
        assert!(dir.join("plugin.tcl").exists());

        let found = discover_all().expect("discover");
        assert_eq!(named(&found), ["fresh-one"]);
        assert_eq!(found[0].manifest.kind, "tcl");
        assert_eq!(found[0].manifest.entry, "plugin.tcl");
        assert_eq!(found[0].manifest.provides_hooks, ["post_llm_call"]);

        // Twice is an error, not a silent overwrite of somebody's work.
        assert!(scaffold("fresh-one", "").is_err());

        remove("fresh-one").expect("remove");
        assert!(discover_all().expect("discover").is_empty());
        assert!(remove("fresh-one").is_err(), "removing twice must fail");
    }

    /// `--kind racket` (R9): a `#lang racket/base` entry that requires
    /// the `veles` collection and registers one tool and one hook, under
    /// the lang-racket host's kind.
    #[test]
    fn a_racket_plugin_scaffolds_with_the_racket_hosts_kind() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let dir = scaffold_kind("rk-one", "a racket thing", "racket").expect("scaffold");
        assert!(dir.join("plugin.rkt").exists());
        let found = discover_all().expect("discover");
        assert_eq!(named(&found), ["rk-one"]);
        assert_eq!(found[0].manifest.kind, "racket");
        assert_eq!(found[0].manifest.entry, "plugin.rkt");
        let src = std::fs::read_to_string(dir.join("plugin.rkt")).expect("read");
        assert!(src.starts_with("#lang racket/base"), "{src}");
        assert!(
            src.contains("(require veles)")
                && src.contains("(register-tool")
                && src.contains("rk_one_hello")
                && src.contains("(hook \"post_llm_call\""),
            "{src}"
        );
    }

    /// `--kind lisp` (R9): a Common Lisp file registering one tool and one
    /// hook through VELES, under the lang-lisp host's kind; an unknown
    /// kind is refused by name.
    #[test]
    fn a_lisp_plugin_scaffolds_with_the_lisp_hosts_kind() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let dir = scaffold_kind("cl-one", "a lisp thing", "lisp").expect("scaffold");
        assert!(dir.join("plugin.lisp").exists());
        let found = discover_all().expect("discover");
        assert_eq!(named(&found), ["cl-one"]);
        assert_eq!(found[0].manifest.kind, "lisp");
        assert_eq!(found[0].manifest.entry, "plugin.lisp");
        let src = std::fs::read_to_string(dir.join("plugin.lisp")).expect("read");
        assert!(
            src.contains("(register-tool")
                && src.contains("cl_one_hello")
                && src.contains("(hook \"post_llm_call\""),
            "{src}"
        );
        let err = scaffold_kind("py-one", "", "python")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown plugin kind `python`"), "{err}");
    }

    /// `install` clones a git repo. Local paths are valid clone sources,
    /// so the whole path is testable without a network: build a repo,
    /// install it, and check the .git did NOT come along — a nested
    /// repo inside the plugins repo makes the snapshot history useless.
    #[test]
    fn install_takes_the_manifest_and_leaves_the_git_behind() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let src = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            src.path().join("plugin.toml"),
            "name = \"from-git\"\nkind = \"tcl\"\nprovides_hooks = [\"pre_llm_call\"]\n",
        )
        .expect("manifest");
        std::fs::write(src.path().join("plugin.tcl"), "# hi\n").expect("entry");
        for args in [
            vec!["init", "-q"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .args(&args)
                .current_dir(src.path())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed while building the fixture repo");
        }

        let dest = install(&src.path().to_string_lossy(), None).expect("install");
        assert_eq!(dest, plugins_dir().join("from-git"));
        assert!(dest.join("plugin.tcl").exists());
        assert!(
            !dest.join(".git").exists(),
            "the clone's .git must not nest inside the plugins repo"
        );
        assert_eq!(named(&discover_all().expect("discover")), ["from-git"]);

        // Same name twice is refused rather than clobbered.
        assert!(install(&src.path().to_string_lossy(), None).is_err());
        // `as_name` renames, and is validated like any other name.
        let renamed = install(&src.path().to_string_lossy(), Some("other-name")).expect("rename");
        assert_eq!(renamed, plugins_dir().join("other-name"));
        assert!(install(&src.path().to_string_lossy(), Some("../escape")).is_err());

        // Every refusal path leaves the staging area empty — including
        // the ones that fail through `?` after the clone exists (a bad
        // `as_name` here), which used to litter $TMPDIR.
        let staging = tempfile::tempdir().expect("staging");
        for as_name in [Some("../escape"), Some("other-name"), None] {
            assert!(install_via(&src.path().to_string_lossy(), as_name, staging.path()).is_err());
        }
        let left: Vec<_> = std::fs::read_dir(staging.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            left.is_empty(),
            "a refused install left its clone: {left:?}"
        );
    }

    /// A directory with no plugin.toml is not a plugin, and `install`
    /// says so before moving anything into place — a half-install
    /// leaves a directory nobody can explain.
    #[test]
    fn install_refuses_a_repo_that_is_not_a_plugin() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        let src = tempfile::tempdir().expect("tempdir");
        std::fs::write(src.path().join("README.md"), "not a plugin\n").expect("readme");
        for args in [
            vec!["init", "-q"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            let _ = std::process::Command::new("git")
                .args(&args)
                .current_dir(src.path())
                .status();
        }
        let err = install(&src.path().to_string_lossy(), None)
            .expect_err("a repo with no manifest is not installable");
        assert!(
            err.to_string().contains("plugin.toml"),
            "the error must name what is missing: {err}"
        );
        assert!(!plugins_dir().join("src").exists());
    }

    /// `rollback` commits the state it is rolling back FROM, so the
    /// undo is not a one-way door — which is what `veles plugins
    /// rollback` was until it stopped hand-rolling its own git checkout
    /// (frontends/cli.rs).
    ///
    /// The case that matters is UNCOMMITTED work: a clean tree is
    /// already a commit and needs no rescue (`snapshot` correctly
    /// no-ops on it, which an earlier version of this test mistook for
    /// the bug). Live plugin edits are exactly the uncommitted kind —
    /// the runtime snapshots on reload, not on keystroke — so this
    /// models an unsaved edit being rolled over.
    #[test]
    fn rollback_rescues_uncommitted_work_before_it_checks_out() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        scaffold("versioned", "v1").expect("scaffold");
        let first = history(10).expect("history");
        assert!(!first.is_empty(), "the scaffold must have been committed");
        let base = first[0].commit.clone();
        let entry = plugins_dir().join("versioned").join("plugin.tcl");
        let v1 = std::fs::read_to_string(&entry).expect("read");

        // An edit nobody committed — what a live plugin looks like
        // between reloads.
        std::fs::write(&entry, "# unsaved work\n").expect("write");

        rollback(&base).expect("rollback");
        assert_eq!(
            std::fs::read_to_string(&entry).expect("read"),
            v1,
            "the rollback must restore the committed content"
        );

        // The unsaved edit survived, as a commit made before the
        // checkout clobbered the file.
        let after = history(20).expect("history");
        let rescue = after
            .iter()
            .find(|c| c.subject.contains("before rollback"))
            .unwrap_or_else(|| {
                panic!(
                    "rollback must commit the working tree first; history: {:?}",
                    after.iter().map(|c| &c.subject).collect::<Vec<_>>()
                )
            });
        let shown = std::process::Command::new("git")
            .args(["show", &format!("{}:versioned/plugin.tcl", rescue.commit)])
            .current_dir(plugins_dir())
            .output()
            .expect("git show");
        assert_eq!(
            String::from_utf8_lossy(&shown.stdout),
            "# unsaved work\n",
            "the rescued commit must hold the edit that was about to be lost"
        );

        assert!(rollback("not a commit-ish!").is_err());
    }
}
