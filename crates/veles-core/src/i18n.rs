//! i18n — locale resolution + string lookup (catalogs read at runtime).
//!
//! USER DIRECTIVE (2026-08-22): l10n/i18n/a11y on the plan. This module
//! is the plumbing: one resolver shared by every Rust surface, and a
//! matching msgcat locale on the Tcl/desktop side (lib/i18n.tcl reads
//! the same config key). USER DIRECTIVE (2026-09-21): fill the l10n
//! gaps — wrap every visible string and ship the catalogs.
//!
//! Resolution order (first hit wins):
//!   1. config `tui.locale`
//!   2. VELES_LOCALE env
//!   3. system (`LANG` / `LC_ALL`, taking the part before `.`/`@`)
//!   4. "en" (the source language; catalogs fall back to en per-key too)
//!
//! THE KEY CONVENTION is "the English source string is the key" —
//! identical to the desktop's msgcat catalogs, so one translator can
//! work both sides and an engine string can pass through untouched when
//! nothing is translated. Two shapes travel under it:
//!
//!   * Symbolic keys (`doctor_total`) for strings that were extracted
//!     first; en.yml carries them and `lookup` resolves them.
//!   * English-as-key (`"focused pane: %{pane}"`) for everything added
//!     since — a missing entry resolves to the key itself, which IS the
//!     English text, so a gap degrades to the untranslated string
//!     rather than to a bare identifier. en.yml does NOT need entries
//!     for these; the en catalog is the source strings themselves.
//!
//! Catalogs live in `locales/<locale>.yml` (flat keys under
//! `_version: 1`) and are RUNTIME files: a locale's catalog is read the
//! first time something renders in it (see [`locales_dir`]). Only
//! en.yml is compiled in. Locale ids are canonical: lowercase language,
//! `_` before a kept script subtag (`zh_hans`, `sr_cyrl`), exactly
//! matching the file stems. [`normalize`] maps anything else onto one of
//! those, dropping region codes (`de_DE` -> `de`) so a regional config
//! value cannot silently fall through to English.
//!
//! Usage: `crate::i18n::lookup("key", &[("name", val)])`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// English, the one catalog compiled in: the symbolic keys' text, so a
/// binary with no catalogs on disk still renders every string.
const EN_YML: &str = include_str!("../locales/en.yml");

type Catalog = HashMap<String, String>;

/// Where the translations are read from. They are not compiled in (USER,
/// 2026-09-28: "embedding this in the binaries 47 times seems objectively
/// stupid"): rust-i18n's `i18n!` expanded the 47 catalogs into ~11,400
/// `map.insert` statements in ONE function, compiled into every build of
/// this crate — the library, its test binary, clippy, each debug build.
///
/// First hit wins:
///   1. `VELES_LOCALES_DIR` (the nix wrapper sets it, as it sets
///      `VELES_DESKTOP_DIR`);
///   2. `share/veles/locales` beside the binary's `bin/` — the installed
///      layout, so a bare `$out/bin/veles` finds them too;
///   3. this source tree's `locales/` — what a working-tree build runs
///      against (`target/debug/veles`, the dev gateway, `cargo test`).
///
/// None of them: English only.
fn locales_dir() -> Option<PathBuf> {
    locales_dir_from(
        std::env::var_os("VELES_LOCALES_DIR").map(PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf)),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("locales"),
    )
}

/// [`locales_dir`]'s chain, pure, so a test can point it at a fake
/// layout.
fn locales_dir_from(
    env: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    source_tree: PathBuf,
) -> Option<PathBuf> {
    let installed = exe_dir.and_then(|d| Some(d.parent()?.join("share/veles/locales")));
    [env, installed, Some(source_tree)]
        .into_iter()
        .flatten()
        .find(|d| d.is_dir())
}

/// A catalog file's rows. Nested maps flatten to dot-keys and non-string
/// values (`_version: 1`) are skipped — what rust-i18n did with them.
fn parse_catalog(yaml: &str) -> Result<Catalog, String> {
    fn flatten(prefix: &str, map: &serde_yaml::Mapping, out: &mut Catalog) {
        for (k, v) in map {
            let Some(k) = k.as_str() else { continue };
            let key = if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{prefix}.{k}")
            };
            match v {
                serde_yaml::Value::String(s) => {
                    out.insert(key, s.clone());
                }
                serde_yaml::Value::Mapping(m) => flatten(&key, m, out),
                _ => {}
            }
        }
    }
    let map: serde_yaml::Mapping = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
    let mut out = Catalog::new();
    flatten("", &map, &mut out);
    Ok(out)
}

/// The locales one directory offers, each catalog read on first use.
struct Catalogs {
    dir: Option<PathBuf>,
    en: Catalog,
    /// Canonical ids with a catalog, `en` included; sorted.
    ids: Vec<String>,
    loaded: RwLock<HashMap<String, Arc<Catalog>>>,
}

impl Catalogs {
    fn from_dir(dir: Option<PathBuf>) -> Self {
        let mut ids = vec!["en".to_string()];
        if let Some(entries) = dir.as_deref().and_then(|d| std::fs::read_dir(d).ok()) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("yml") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        ids.push(stem.to_string());
                    }
                }
            }
        }
        ids.sort();
        ids.dedup();
        Catalogs {
            dir,
            en: parse_catalog(EN_YML).expect("the compiled-in en.yml parses"),
            ids,
            loaded: RwLock::new(HashMap::new()),
        }
    }

    /// `id`'s catalog, read from disk the first time. A file that will
    /// not read or parse renders that locale in English, once logged —
    /// one bad catalog must not take the others down.
    fn catalog(&self, id: &str) -> Option<Arc<Catalog>> {
        if id == "en" || !self.ids.iter().any(|k| k == id) {
            return None;
        }
        if let Some(c) = self
            .loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            return Some(c.clone());
        }
        let path = self.dir.as_ref()?.join(format!("{id}.yml"));
        let rows = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|y| parse_catalog(&y))
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "i18n: {} is unreadable ({e}) — {id} renders in English",
                    path.display()
                );
                Catalog::new()
            });
        Some(
            self.loaded
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(rows))
                .clone(),
        )
    }

    /// The locale's text, else English's, else `None` (the caller shows
    /// the key).
    fn translate(&self, locale: &str, key: &str) -> Option<String> {
        self.catalog(locale)
            .and_then(|c| c.get(key).cloned())
            .or_else(|| self.en.get(key).cloned())
    }
}

fn catalogs() -> &'static Catalogs {
    static CATALOGS: OnceLock<Catalogs> = OnceLock::new();
    CATALOGS.get_or_init(|| Catalogs::from_dir(locales_dir()))
}

/// One key in one locale, falling back to English — `None` when neither
/// has it.
pub fn translate(locale: &str, key: &str) -> Option<String> {
    catalogs().translate(locale, key)
}

/// The active locale, cached for the process. A Mutex, not a OnceLock:
/// the language picker switches the live process (USER ask 2026-09-20,
/// "a dedicated button to swap"), and boot-time `init` runs more than
/// once in a process (light-command boot, then the gateway boot), so
/// "first write wins" would pin whatever the earliest caller resolved.
static LOCALE: Mutex<Option<String>> = Mutex::new(None);

/// Resolve + install the process locale. Called from boot paths
/// (main.rs, gateway, TUI); later callers just read [`current`].
pub fn init(cfg_locale: Option<&str>) -> String {
    let loc = cfg_locale
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("VELES_LOCALE").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(system_locale)
        .unwrap_or_else(|| "en".to_string());
    set_active(&loc)
}

/// Switch the locale at runtime: normalize, and update the cached value
/// `current()` answers. Returns the canonical id that was installed.
/// This is what the language picker calls; the caller is responsible
/// for persisting `tui.locale` if the choice should survive a restart
/// (the desktop palette does, via `/api/config/set`; a bare CLI
/// invocation does not).
pub fn set_active(raw: &str) -> String {
    let normalized = normalize(raw);
    // A poisoned lock is not worth panicking over in a UI switch: the
    // worst case is `current()` reporting stale while lookups work.
    if let Ok(mut g) = LOCALE.lock() {
        *g = Some(normalized.clone());
    }
    normalized
}

// ── per-request locale ───────────────────────────────────────────────
//
// The PROCESS locale above is right for a single-user process (the TUI,
// the REPL, a CLI command). It is wrong for the gateway, which serves
// IRC, Telegram, A2A and the desktop from ONE process: the first `/locale`
// switched the language for every user of every surface at once, while
// its ACL comment claimed it "changes what this session RENDERS"
// (USER, 2026-09-25: per-surface locale). So a lookup renders in the
// locale of the REQUEST it belongs to, when there is one:
//
//   1. a thread-local scope — set around a SYNC unit of work (the shared
//      command dispatcher sets it from the session's own locale);
//   2. a task-local scope — set around an ASYNC unit of work (a desktop
//      request carries the window's language in `X-Veles-Locale`; a turn
//      runs under its session's locale);
//   3. the process locale.
//
// Thread-local wins over task-local because a sync scope is always
// entered INSIDE the async unit that called it — it is the innermost.
// A sync function cannot await, so a thread-local scope can never leak
// into another task scheduled on the same thread.
//
// Work handed to `tokio::spawn` / `spawn_blocking` does not inherit a
// task-local: a lookup there falls back to the process locale unless the
// spawner re-enters the scope (`scope(scoped(), fut)`), as the desktop's
// turn spawns do.

tokio::task_local! {
    static TASK_LOCALE: String;
}

thread_local! {
    static THREAD_LOCALE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The innermost request scope, if any.
pub fn scoped() -> Option<String> {
    if let Some(l) = THREAD_LOCALE.with(|c| c.borrow().clone()) {
        return Some(l);
    }
    TASK_LOCALE.try_with(|l| l.clone()).ok()
}

/// The locale a lookup renders in right now: the request's, else the
/// process's.
pub fn effective() -> String {
    scoped().unwrap_or_else(current)
}

/// Run `fut` with `loc` as its request locale (normalized). `None` runs
/// it unscoped, so callers can pass an optional value straight through.
pub async fn scope<F: std::future::Future>(loc: Option<String>, fut: F) -> F::Output {
    match loc.filter(|l| !l.trim().is_empty()) {
        Some(l) => TASK_LOCALE.scope(normalize(&l), fut).await,
        None => fut.await,
    }
}

/// A thread-local request scope for SYNC work, restored on drop. `None`
/// leaves the current scope alone.
#[must_use = "the scope ends when the guard drops"]
pub struct ScopeGuard(Option<Option<String>>);

impl ScopeGuard {
    pub fn enter(loc: Option<String>) -> Self {
        match loc.filter(|l| !l.trim().is_empty()) {
            Some(l) => {
                let prev = THREAD_LOCALE.with(|c| c.replace(Some(normalize(&l))));
                ScopeGuard(Some(prev))
            }
            None => ScopeGuard(None),
        }
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.0.take() {
            THREAD_LOCALE.with(|c| *c.borrow_mut() = prev);
        }
    }
}

/// The active PROCESS locale ("en" before init — English is the source).
/// A lookup renders in [`effective`], which a request scope can override.
pub fn current() -> String {
    LOCALE
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "en".to_string())
}

/// Canonical id for an arbitrary locale spelling, exposed for callers
/// that hold a config value and want to compare it with
/// [`current`] (the `/locale swap` path). Mirrors `normalize` so the
/// desktop and the engine agree on what a stored value means.
pub fn normalize_id(raw: &str) -> String {
    normalize(raw)
}

/// Every locale with a catalog in [`locales_dir`], plus English;
/// canonical ids, sorted (the picker's list; also what [`normalize`]
/// treats as "known").
pub fn available() -> Vec<String> {
    catalogs().ids.clone()
}

/// Canonical form of one candidate id: lowercase, `-` folded to `_`,
/// codeset/modifier stripped. The script subtag keeps its spelling
/// lowercased — file stems are compared case-insensitively.
fn canon(raw: &str) -> String {
    raw.trim()
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .replace('-', "_")
        .to_lowercase()
}

/// Map any locale spelling onto a canonical id this build actually has,
/// falling back to the bare language, then to the input's canonical
/// form (which then resolves to English per key).
///
/// The live bug this exists for: `de_DE` in the config made msgcat
/// answer German while the engine (then rust-i18n) answered English,
/// because its fallback chain split on `-` only — so the desktop was
/// translated and the CLI/TUI were not, from one stored value.
fn normalize(raw: &str) -> String {
    let c = canon(raw);
    if c.is_empty() {
        return "en".into();
    }
    // "C"/"POSIX" mean "no preference", not a language. (system_locale
    // already filters these from the env; a config value or a
    // `C.UTF-8` LANG reaches here unfiltered and would otherwise pin a
    // bogus locale id that no catalog can exist for.)
    if c == "c" || c == "posix" {
        return "en".into();
    }
    let known = available();
    let find = |want: &str| known.iter().find(|k| k.eq_ignore_ascii_case(want)).cloned();
    if let Some(hit) = find(&c) {
        return hit;
    }
    // Two languages carry a SCRIPT split that a bare language code
    // cannot express, and the region implies the script: Simplified
    // (zh_CN/SG/MY) vs Traditional (zh_TW/HK/MO), Cyrillic vs Latin
    // Serbian (sr_RS default Cyrillic; @latin/Latn is the exception).
    // Guessing these from the region is what makes `zh_TW` in a config
    // actually show Traditional.
    let parts: Vec<&str> = c.split('_').collect();
    let lang = parts[0];
    if lang == "zh" {
        let traditional = parts
            .iter()
            .any(|p| matches!(*p, "tw" | "hk" | "mo" | "hant"));
        let want = if traditional { "zh_hant" } else { "zh_hans" };
        return find(want).unwrap_or_else(|| want.to_string());
    }
    if lang == "sr" {
        let latin = parts.iter().any(|p| matches!(*p, "latn" | "latin"));
        let want = if latin { "sr_latn" } else { "sr_cyrl" };
        return find(want).unwrap_or_else(|| want.to_string());
    }
    // Drop the subtag (`de_de` -> `de`, `pt_br` -> `pt`).
    if parts.len() > 1 {
        if let Some(hit) = find(lang) {
            return hit;
        }
        return lang.to_string();
    }
    c
}

fn system_locale() -> Option<String> {
    for var in ["LC_ALL", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() && v != "C" && v != "POSIX" {
                return Some(v);
            }
        }
    }
    None
}

/// Translate with optional args. Falls back to the key itself when no
/// catalog entry exists (so extraction can be gradual without breaking
/// surfaces).
#[macro_export]
macro_rules! i18n {
    ($key:expr) => {
        $crate::i18n::lookup($key, &[])
    };
    ($key:expr, $($name:ident = $val:expr),* $(,)?) => {
        $crate::i18n::lookup($key, &[$((stringify!($name), $val.to_string())),+])
    };
}

/// Direct lookup used by the macro above.
///
/// TWO KEY SHAPES travel through here (module doc): symbolic ids and
/// English source strings. Both miss the same way on purpose — the key
/// itself is returned — but they mean different things when they do:
/// a symbolic id coming back is a GAP (the catalog lost a key), while
/// an English string coming back IS the English text. The key is a
/// lookup string, exactly like msgcat's on the desktop.
pub fn lookup(key: &str, args: &[(&str, String)]) -> String {
    // The REQUEST's locale, not the process's — see "per-request
    // locale" above. A miss is the key itself, never "<locale>.<key>"
    // (rust-i18n's `_rust_i18n_translate` did that, which would have
    // rendered every English-as-key string as "en.running tasks:").
    let mut s = translate(&effective(), key).unwrap_or_else(|| key.to_string());
    if s == key && args.is_empty() {
        // No catalog entry anywhere — surface the key itself.
        //
        // For an English-source key that is the correct text; for a
        // symbolic key it is a loud gap. A `{}` here would be worse than
        // either: the existing loud-missing policy is what turns a
        // missing translation into something a reader can act on.
        return s;
    }
    for (k, v) in args {
        s = s.replace(&format!("%{{{k}}}"), v);
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locale is PROCESS-GLOBAL state (the `LOCALE` latch), and cargo
    /// runs test fns on parallel threads. Two
    /// tests that both switch locale will interleave: this surfaced as
    /// `set_active_is_visible_to_current` failing on `assert_eq!(current(),
    /// "de")` while every one of its own statements was correct — another
    /// test had switched it back in between. Serialize the mutating
    /// tests on this lock rather than trusting the scheduler.
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with the locale lock held, restoring the previous locale.
    fn with_locale<T>(f: impl FnOnce() -> T) -> T {
        // A poisoned lock means another test panicked mid-switch; the
        // locale is global, so recovering is the honest choice — the
        // alternative is a cascade of unrelated failures.
        let _guard = LOCALE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = current();
        let out = f();
        set_active(&previous);
        out
    }

    fn catalog(loc: &str, key: &str) -> String {
        translate(loc, key).unwrap_or_else(|| key.to_string())
    }

    /// The catalogs are RUNTIME files (USER, 2026-09-28): a catalog
    /// written after this binary was built is found and read, a broken
    /// one renders English instead of taking the rest down, and with no
    /// directory at all the compiled-in English still renders the
    /// symbolic keys. A compile-time embed can pass none of these: it
    /// never sees a file the test writes.
    #[test]
    fn catalogs_are_read_at_runtime_and_english_is_built_in() {
        let dir = crate::testenv::kept_dir();
        std::fs::write(
            dir.join("xx.yml"),
            "_version: 1\ntasks_header: \"XX-tasks:\"\n\"focused pane: %{pane}\": \"XX %{pane}\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("yy.yml"), "tasks_header: [unclosed\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a catalog").unwrap();

        let c = Catalogs::from_dir(Some(dir.clone()));
        assert_eq!(c.ids, ["en", "xx", "yy"], "one id per .yml, en always");
        assert_eq!(
            c.translate("xx", "tasks_header").as_deref(),
            Some("XX-tasks:")
        );
        assert_eq!(
            c.translate("xx", "focused pane: %{pane}").as_deref(),
            Some("XX %{pane}")
        );
        assert_eq!(
            c.translate("xx", "tasks_none").as_deref(),
            Some("(no running tasks)"),
            "a key the locale lacks falls back to English"
        );
        assert_eq!(
            c.translate("yy", "tasks_header").as_deref(),
            Some("running tasks:"),
            "a catalog that does not parse renders English"
        );
        assert_eq!(c.translate("xx", "nobody has this"), None);

        let bare = Catalogs::from_dir(None);
        assert_eq!(bare.ids, ["en"]);
        assert_eq!(
            bare.translate("de", "tasks_header").as_deref(),
            Some("running tasks:"),
            "no catalogs on disk: English is compiled in"
        );
    }

    /// Where the catalogs are looked for: the env var, then the installed
    /// layout beside the binary, then the source tree — each only if it
    /// exists.
    #[test]
    fn the_catalog_directory_follows_env_then_install_then_source() {
        let root = crate::testenv::kept_dir();
        let env = root.join("env-locales");
        let out = root.join("out");
        let installed = out.join("share/veles/locales");
        let tree = root.join("tree/locales");
        for d in [&env, &installed, &tree] {
            std::fs::create_dir_all(d).unwrap();
        }
        let bin = Some(out.join("bin"));
        assert_eq!(
            locales_dir_from(Some(env.clone()), bin.clone(), tree.clone()),
            Some(env)
        );
        assert_eq!(
            locales_dir_from(None, bin.clone(), tree.clone()),
            Some(installed),
            "a bare $out/bin/veles finds $out/share/veles/locales"
        );
        assert_eq!(
            locales_dir_from(
                Some(root.join("gone")),
                Some(root.join("x/bin")),
                tree.clone()
            ),
            Some(tree.clone()),
            "a stale env var or a binary with no install layout falls through"
        );
        assert_eq!(locales_dir_from(None, None, root.join("no-tree")), None);
    }

    /// THE property of the per-request locale: two requests in flight at
    /// once, on a multi-thread runtime, each render THEIR language — and
    /// neither moves the process locale. The first `/locale` switched the
    /// process, i.e. every user of the gateway at once.
    #[test]
    fn two_requests_render_two_languages_at_once() {
        with_locale(|| {
            set_active("en");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .build()
                .unwrap();
            let (de, ru) = rt.block_on(async {
                let de = crate::rt::spawn(scope(Some("de".into()), async {
                    tokio::task::yield_now().await;
                    lookup("tasks_header", &[])
                }));
                let ru = crate::rt::spawn(scope(Some("ru_RU.UTF-8".into()), async {
                    tokio::task::yield_now().await;
                    lookup("tasks_header", &[])
                }));
                (de.await.unwrap(), ru.await.unwrap())
            });
            assert_eq!(de, catalog("de", "tasks_header"));
            assert_eq!(ru, catalog("ru", "tasks_header"));
            assert_ne!(de, ru);
            assert_eq!(current(), "en", "a request scope never moves the process");
            assert_eq!(lookup("tasks_header", &[]), catalog("en", "tasks_header"));
        });
    }

    /// An English-as-key string with no catalog entry is still the key
    /// itself under a scope, never "de.<key>" (what rust-i18n's
    /// `_rust_i18n_translate` answered before the catalogs moved to
    /// runtime files).
    #[test]
    fn a_scoped_miss_is_the_english_key() {
        with_locale(|| {
            let _g = ScopeGuard::enter(Some("de".into()));
            assert_eq!(
                lookup("nobody translated this %{x}", &[("x", "yet".into())]),
                "nobody translated this yet"
            );
        });
    }

    /// A sync scope is the innermost one, and it is restored on drop —
    /// including back to an outer sync scope.
    #[test]
    fn a_sync_scope_nests_and_restores() {
        with_locale(|| {
            set_active("en");
            assert_eq!(scoped(), None);
            {
                let _outer = ScopeGuard::enter(Some("de".into()));
                assert_eq!(effective(), "de");
                {
                    let _inner = ScopeGuard::enter(Some("ru".into()));
                    assert_eq!(effective(), "ru");
                    let _noop = ScopeGuard::enter(None);
                    assert_eq!(effective(), "ru", "None leaves the scope alone");
                }
                assert_eq!(effective(), "de");
            }
            assert_eq!(scoped(), None);
            assert_eq!(effective(), "en");
        });
    }

    /// The registry the normalizer consults must be REAL — it is read
    /// from the compiled catalogs, not a hand-kept list.
    ///
    /// The assertions name `de` and `ru` only. An earlier version also
    /// required `zh_hans`, which made the test a claim about TRANSLATION
    /// PROGRESS rather than about the mechanism: it failed the moment a
    /// locale was absent from the build for a reason that has nothing to
    /// do with the registry (a catalog mid-translation, a build that
    /// picked up fewer files). `de` and `ru` are the two the l10n proof
    /// ships with, so they are the honest floor; the rest is the
    /// catalogue-set test in `i18ntest.tcl`, which reads the tree.
    #[test]
    fn available_locales_are_the_shipped_catalogs() {
        let a = available();
        assert!(a.contains(&"en".to_string()), "en: {a:?}");
        assert!(a.contains(&"de".to_string()), "de: {a:?}");
        assert!(a.iter().any(|l| l == "ru"), "ru missing from {a:?}");
        // The registry is DERIVED, not hardcoded: whatever it reports
        // must be a sorted, de-duplicated set of id-shaped names, and
        // every one must normalize to itself (else `find` can never hit
        // it and the catalog is unreachable from config).
        let mut sorted = a.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(a, sorted, "available() must be sorted and unique: {a:?}");
        for id in &a {
            assert_eq!(
                &normalize(id),
                id,
                "{id} is shipped but does not normalize to itself"
            );
        }
    }

    #[test]
    fn canon_folds_case_and_separators() {
        assert_eq!(canon("de_DE.UTF-8"), "de_de");
        assert_eq!(canon("zh-Hans"), "zh_hans");
        assert_eq!(canon("sr_Cyrl_RS@latin"), "sr_cyrl_rs");
        assert_eq!(canon("  en  "), "en");
    }

    /// The live divergence this normalizer exists for: `de_DE` used to
    /// answer German in msgcat and ENGLISH in rust-i18n, because the
    /// fallback chain splits on `-` only. One stored value must resolve
    /// to one language on both surfaces.
    #[test]
    fn a_regional_locale_resolves_to_its_language() {
        assert_eq!(normalize("de_DE"), "de");
        assert_eq!(normalize("de-DE"), "de");
        assert_eq!(normalize("ru_RU"), "ru");
        assert_eq!(normalize("zh_CN"), "zh_hans");
        assert_eq!(normalize("zh_TW"), "zh_hant");
        assert_eq!(normalize("pt_BR"), "pt");
    }

    /// A script subtag is kept, because `zh_Hans` and `zh_Hant` are
    /// genuinely different catalogs while `zh_CN`/`zh_SG` are not.
    #[test]
    fn a_script_subtag_survives_normalization() {
        assert_eq!(normalize("zh_Hans"), "zh_hans");
        assert_eq!(normalize("zh-Hant"), "zh_hant");
        assert_eq!(normalize("sr_Latn"), "sr_latn");
        assert_eq!(normalize("sr_RS"), "sr_cyrl"); // bare sr → Cyrillic default
    }

    #[test]
    fn an_unknown_locale_falls_back_without_panicking() {
        assert_eq!(normalize(""), "en");
        assert_eq!(normalize("qq_ZZ"), "qq"); // keeps going; en per key
                                              // "C"/"POSIX" mean no preference, not a language named C.
        assert_eq!(normalize("C"), "en");
        assert_eq!(normalize("POSIX"), "en");
        assert_eq!(normalize("C.UTF-8"), "en");
    }

    /// `set_active` + `current` are a pair: a switch must be visible to
    /// `current()` immediately. (The old OnceLock made this impossible —
    /// "first write wins" pinned whatever boot resolved.)
    #[test]
    fn set_active_is_visible_to_current() {
        with_locale(|| {
            let got = set_active("de_DE");
            assert_eq!(got, "de");
            assert_eq!(current(), "de");
            let got = set_active("ru");
            assert_eq!(got, "ru");
            assert_eq!(current(), "ru");
        });
    }

    #[test]
    fn lookup_falls_back_to_key_when_missing() {
        // No catalog entry for this nonsense key — must not panic and
        // must surface the key itself (loud-missing policy).
        assert_eq!(lookup("no_such_key_xyz", &[]), "no_such_key_xyz");
    }

    /// English-as-key is the convention for everything extracted after
    /// the symbolic batch: an English source string that has no entry
    /// in the active locale must come back as ITSELF (the English text),
    /// and must come back TRANSLATED when the catalog has it.
    ///
    /// The translated half is driven by a key read OUT OF de.yml rather
    /// than a hardcoded one. Naming a specific key made this test a
    /// claim about translation PROGRESS: it asserted `focused pane:
    /// %{pane}` was translated, so it went red the moment that string
    /// entered the tree ahead of its German, which says nothing about
    /// whether the mechanism works. Reading a real entry tests the same
    /// thing and cannot drift out of date.
    #[test]
    fn an_english_key_translates_and_otherwise_returns_itself() {
        with_locale(|| {
            // A key with no catalog entry anywhere: fallback-to-key is
            // the English text, and no locale may turn it into a panic.
            set_active("en");
            assert_eq!(
                lookup("focused pane: %{pane}", &[("pane", "dev".into())]),
                "focused pane: dev"
            );
            set_active("qq");
            assert_eq!(
                lookup("focused pane: %{pane}", &[("pane", "dev".into())]),
                "focused pane: dev",
                "unknown locale must degrade to English"
            );

            // And the other direction, against a real de.yml entry.
            set_active("de");
            let yml = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales/de.yml"),
            )
            .expect("de.yml");
            let key = yml
                .lines()
                .filter(|l| !l.starts_with('#') && !l.starts_with("_version"))
                .find_map(|l| {
                    let (k, v) = l.split_once(": \"")?;
                    let k = k.trim().trim_matches('"');
                    // A plain prose key with no placeholder: the value is
                    // compared verbatim, so anything that has to be
                    // formatted would need the arguments too.
                    (!k.is_empty() && !k.contains('%') && !v.is_empty()).then(|| k.to_string())
                })
                .expect("de.yml carries at least one plain prose entry");
            let got = lookup(&key, &[]);
            assert_ne!(
                got, key,
                "de.yml carries `{key}` but lookup returned the English key"
            );
        });
    }

    /// Every row of every engine catalog says what its ENGLISH SOURCE
    /// says, placeholder for placeholder, with the source's edge spaces.
    ///
    /// The 2026-09-21 round reported "every locale complete" by counting
    /// KEYS. The values were never compared with the English they
    /// translate, and ~125 engine rows in 42 locales turned out to be
    /// translations of the key's NAME: `tasks_cancelled` ("cancelled
    /// task %{id}") became "Aufgaben abgebrochen" — the id gone — and in
    /// ja/th the failover prompt stopped naming the provider that failed
    /// (live: `VELES_LOCALE=ja veles memory doctor` printed "ブロック"
    /// with no numbers). The source of a symbolic key is en.yml's text,
    /// never the key itself — comparing against the name is exactly how
    /// those rows passed. `scripts/i18n-catalog-check.tcl` is the same
    /// check for both surfaces; this is the engine half inside
    /// `cargo test`, so the flake gates it.
    #[test]
    fn every_catalog_row_keeps_its_sources_placeholders() {
        fn unyaml(s: &str) -> String {
            s.replace("\\\"", "\"").replace("\\\\", "\\")
        }
        fn rows(yaml: &str) -> Vec<(String, String)> {
            let mut out = Vec::new();
            for l in yaml.lines() {
                if l.starts_with('#') || l.starts_with("_version") {
                    continue;
                }
                let Some((k, v)) = l.split_once(": \"") else {
                    continue;
                };
                let k = k.trim();
                let k = k
                    .strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .unwrap_or(k);
                let Some(v) = v.strip_suffix('"') else {
                    continue;
                };
                out.push((unyaml(k), unyaml(v)));
            }
            out
        }
        fn specs(s: &str) -> Vec<String> {
            let re = regex::Regex::new(r"%\{[A-Za-z0-9_]+\}|%(?:\d+\$)?[sd]|%%").unwrap();
            let mut v: Vec<String> = re.find_iter(s).map(|m| m.as_str().to_string()).collect();
            v.sort();
            v
        }
        fn edges(s: &str) -> (usize, usize) {
            (
                s.len() - s.trim_start_matches(' ').len(),
                s.len() - s.trim_end_matches(' ').len(),
            )
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let en: std::collections::HashMap<String, String> =
            rows(&std::fs::read_to_string(dir.join("en.yml")).unwrap())
                .into_iter()
                .collect();
        assert!(en.contains_key("tasks_cancelled"), "en.yml must parse");
        let mut bad = Vec::new();
        let mut checked = 0usize;
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let loc = p.file_stem().unwrap().to_string_lossy().to_string();
            if loc == "en" || p.extension().and_then(|x| x.to_str()) != Some("yml") {
                continue;
            }
            for (k, v) in rows(&std::fs::read_to_string(&p).unwrap()) {
                let src = en.get(&k).cloned().unwrap_or_else(|| k.clone());
                checked += 1;
                if specs(&src) != specs(&v) {
                    bad.push(format!("{loc}: placeholders {k:?} → {v:?}"));
                } else if src.trim() != "" && edges(&src) != edges(&v) {
                    bad.push(format!("{loc}: edge spaces {k:?} → {v:?}"));
                }
            }
        }
        assert!(
            checked > 5000,
            "the scan must actually read the catalogs ({checked} rows)"
        );
        assert!(
            bad.is_empty(),
            "{} bad rows, first: {:#?}",
            bad.len(),
            &bad[..bad.len().min(8)]
        );
    }

    /// Every key in en.yml must be looked up somewhere, and no shipped
    /// catalog may invent a key that no surface uses.
    ///
    /// 12 of the first 18 shipped keys were dead: all the failover_*,
    /// choice_* and agents_* strings were written and translated into
    /// German, and nothing ever looked them up — the surfaces they were
    /// written for stayed hardcoded English. Nothing caught it because
    /// an unused YAML entry is invisible to the compiler.
    ///
    /// THE SUBSET RULE CHANGED, 2026-09-21. It used to read "every other
    /// catalog is a subset of en.yml's keys", which held while the
    /// engine keyed catalogs ONLY by symbolic id. The English-as-key
    /// convention then landed: a string extracted from either surface is
    /// keyed by its ENGLISH TEXT, and en.yml — which keeps the symbolic
    /// half only, because for the other half the fallback already IS the
    /// English text — stopped being a superset of anything. Under the
    /// old rule de.yml reported ~900 "keys en.yml does not have", every
    /// one of them legitimate.
    ///
    /// The rule true of BOTH conventions: a key in any catalog must be
    /// one the tree actually uses. The reference set is therefore the
    /// tree's key set, and en.yml is checked in the other direction —
    /// its symbolic keys must be live too, which is the dead-key check
    /// this test was written for in the first place.
    #[test]
    fn catalog_keys_and_code_do_not_drift() {
        use std::collections::BTreeSet;

        /// Top-level `key: "..."` rows, splitting on the colon BEFORE
        /// THE QUOTE rather than the first colon. English-as-key rows
        /// are whole sentences and plenty contain a colon
        /// (`state       unreachable but unauthorized (401/403) — check
        /// the token`), so a first-colon split fabricated keys that
        /// matched nothing on either side.
        fn keys_of(yaml: &str) -> BTreeSet<String> {
            yaml.lines()
                .filter_map(|l| {
                    if l.starts_with(char::is_whitespace) || l.starts_with('#') {
                        return None;
                    }
                    let (k, v) = l.split_once(": \"")?;
                    // Two key spellings exist: a QUOTED key
                    // (`"  (none)": "..."`) and a bare one
                    // (`agents_none: "..."`). Splitting on `: "` leaves
                    // the CLOSING quote attached to a quoted key, so the
                    // trailing `"` has to come off too — without this,
                    // all 906 ru keys compared as `  (none)"` and
                    // matched nothing on either side.
                    let k = k.trim();
                    let k = match k.strip_prefix('"') {
                        Some(inner) => inner.strip_suffix('"').unwrap_or(inner),
                        None => k,
                    };
                    if k.is_empty() || k == "_version" || v.is_empty() {
                        return None;
                    }
                    // Unescape the two escapes the generator writes, so a
                    // YAML key compares equal to the SAME key as the
                    // desktop catalog spells it. Without this, seven keys
                    // containing a quote or a backslash compared as their
                    // escaped form (`...e.g. \"fast\".`) and looked
                    // unreferenced — the escaping is a YAML detail, not
                    // part of the key.
                    let k = k
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\")
                        .replace("\\n", "\n")
                        .replace("\\t", "\t");
                    Some(k)
                })
                .collect()
        }

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .to_path_buf();

        // The engine's own keys: string literals passed to lookup!/i18n!.
        // Escapes are decoded the same way the generator does.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut code = String::new();
        fn walk(d: &std::path::Path, out: &mut String) {
            let Ok(entries) = std::fs::read_dir(d) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                    out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        walk(&src, &mut code);
        // The surfaces are their own crate since 2026-09-28 (veles-frontends);
        // they are engine sources too.
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../veles-frontends/src"),
            &mut code,
        );

        // The desktop's key set, read from the GENERATED en.msg. That
        // file comes from the shared extractor, so using it here means
        // one walker, not two that can disagree.
        let mut desk_keys = BTreeSet::new();
        if let Ok(body) = std::fs::read_to_string(root.join("veles-desktop/msgs/en.msg")) {
            for l in body.lines() {
                let Some(rest) = l.strip_prefix("::msgcat::mcset en ") else {
                    continue;
                };
                // The generator writes `{key} {value}` — the key is a
                // brace GROUP, not a bare word, because English-as-key
                // keys contain spaces. Splitting on a space instead
                // produced the first WORD of every key (`  (fallback:`
                // for `  (fallback: %1$s:%2$s)`), which is why 711 keys
                // still looked unreferenced. Take the brace group, with
                // the closing brace being the one that balances it.
                let bytes = rest.as_bytes();
                if bytes.first() != Some(&b'{') {
                    // A key with no spaces is written bare:
                    // `::msgcat::mcset en Send Отправить`.
                    if let Some((k, _)) = rest.split_once(' ') {
                        desk_keys.insert(k.to_string());
                    }
                    continue;
                }
                let mut depth = 0usize;
                let mut close = None;
                let mut i = 0usize;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(i);
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if let Some(c) = close {
                    desk_keys.insert(rest[1..c].to_string());
                }
            }
        }

        let en = std::fs::read_to_string(dir.join("en.yml")).expect("en.yml");
        let en_keys = keys_of(&en);
        assert!(!en_keys.is_empty(), "parsed no keys out of en.yml");

        //  a catalog key must be used by SOME surface ────────────────
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("locales dir") {
            let p = entry.expect("dir entry").path();
            if p.extension().and_then(|x| x.to_str()) != Some("yml") {
                continue;
            }
            if p.file_stem().and_then(|x| x.to_str()) == Some("en") {
                continue;
            }
            checked += 1;
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let body = std::fs::read_to_string(&p).expect("catalog");
            let orphan: Vec<_> = keys_of(&body)
                .into_iter()
                .filter(|k| !code.contains(&format!("\"{k}\"")) && !desk_keys.contains(k))
                .collect();
            assert!(
                orphan.is_empty(),
                "{name} has {} key(s) no surface uses, first few: {:?}",
                orphan.len(),
                &orphan[..orphan.len().min(6)]
            );
        }
        assert!(
            checked >= 2,
            "expected a shipped locale set, checked {checked}"
        );

        //  every locale CARRIES the engine-reachable keys ─────────────
        //
        // THE OTHER DIRECTION, and its absence was a real shipped bug: 29
        // of 46 locales were missing 24 symbolic keys (failover_*,
        // doctor_*, tasks_*, err_store_locked…) because a translator's
        // part file came back with the identifier copied through as the
        // value and the yml generator drops a value-equals-key row. The
        // loop above only catches keys nothing USES; nothing caught a
        // locale that was short of keys the engine LOOKS UP.
        //
        // Why the desktop's completeness contract missed it too: en.msg
        // carries those keys with the IDENTIFIER as the value, so a locale
        // "has" them and the .msg side reports complete. The two checks
        // looked at two different files and neither covered the seam.
        //
        // The reference set is en.yml, which is the engine's own identity
        // catalog. A locale may legitimately omit an ENGLISH-AS-KEY row
        // that would be identical to its key (the generator drops those by
        // design, and the fallback IS the English text) — so only keys
        // whose en.yml value DIFFERS from the key itself are required.
        // Those are the symbolic ones, where a miss shows the user
        // something they cannot read.
        let required: Vec<&String> = en_keys
            .iter()
            .filter(|k| {
                let v = en
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{k}: ")))
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .unwrap_or_default();
                v != **k
            })
            .collect();
        assert!(
            required.len() > 20,
            "expected the symbolic key set, found {} — did en.yml change shape?",
            required.len()
        );
        for entry in std::fs::read_dir(&dir).expect("locales dir") {
            let p = entry.expect("dir entry").path();
            if p.extension().and_then(|x| x.to_str()) != Some("yml") {
                continue;
            }
            let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("");
            if stem == "en" {
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let have = keys_of(&std::fs::read_to_string(&p).expect("catalog"));
            let missing: Vec<&&String> = required.iter().filter(|k| !have.contains(**k)).collect();
            assert!(
                missing.is_empty(),
                "{name} is missing {} engine-reachable key(s) — the user sees \
                 English where these render. First few: {:?}",
                missing.len(),
                &missing[..missing.len().min(6)]
            );
        }

        //  en.yml's symbolic keys must be live ──────────────────────
        // A symbolic key reaches lookup() through a VARIABLE
        // (`lookup(none, &[])` where `none` came from a match arm), so
        // the literal test alone would call all of those dead. The
        // tuple shape `("tasks_header", "tasks_none")` is how they
        // appear, and it is what the second and third probes cover.
        let dead: Vec<_> = en_keys
            .iter()
            .filter(|k| {
                !code.contains(&format!("\"{k}\""))
                    && !code.contains(&format!("({k},"))
                    && !code.contains(&format!("({k}, "))
            })
            .cloned()
            .collect();
        assert!(
            dead.is_empty(),
            "catalog keys nothing looks up (wire them or delete them): {dead:?}"
        );
    }
}
