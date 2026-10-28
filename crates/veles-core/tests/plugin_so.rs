//! ABI 4 end-to-end: load the REAL out-of-tree sample `.so`
//! (crates/veles-plugin-sample-so, on the SDK) through libloading and
//! exercise every surface — hook verdict, gateway filter rewrite, tool
//! call, slash command, the engine round trip — plus what ABI 4 added
//! (R8): unload with a full purge, a hook registered from a callback,
//! timers, an async tool woken by a timer, a platform that gets the
//! gateway's outbound and publishes inbound, TCP sockets (and the TLS
//! error lane), the refusal of an ABI 3 plugin BEFORE its init runs,
//! and the conformance suite shared with the module tier.
//!
//! The `.so` is a separate workspace member; `cargo test` does not
//! guarantee it is built before this test runs (no dependency edge), so
//! a bare `cargo test` skips with a LOUD message when the artifact is
//! missing. scripts/precommit.sh and the nix `preCheck` both build it
//! first AND set `VELES_REQUIRE_PLUGIN_SO=1`, which turns that skip into
//! a failure — a gate that can silently skip is not a gate.
//!
//! One file, many loads: every test loads the sample under its own
//! name. A `.so` mapped once shares its statics across those copies, so
//! the tests that rely on "the plugin I just loaded is the one my tool
//! talks to" run one at a time (`SERIAL`).
//!
//! Finding the artifact is not `target/{debug,release}`: nixpkgs'
//! buildRustPackage builds into `target/<triple>/release`, so the old
//! two-path guess made this test SKIP in `nix flake check` while its own
//! docstring claimed the daily gate ran it for real. Walk instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use veles_core::rustplugins::{
    load_rust_manifests, load_so, run_gateway_filters, run_hooks, unload_so, FilterDecision,
};

#[path = "support/conformance.rs"]
mod conformance;

/// A cdylib's file name on this platform: `lib<stem>.so` on Linux,
/// `<stem>.dll` on Windows (the `module_load.rs` pattern). The names
/// were hard-coded Unix ones, so on Windows `find_named` found nothing
/// and every test that needs a plugin took its "not built" skip behind
/// an "ok" — the pitfall 81 shape driver_parity.rs had before d3cf989f.
fn cdylib_name(stem: &str) -> String {
    format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// The sample plugin (crates/veles-plugin-sample-so).
fn so_name() -> String {
    cdylib_name("veles_plugin_sample_so")
}

/// The plugin that dlopens fine and is then REFUSED — an ABI 3 build.
/// See crates/veles-plugin-badabi-so.
fn bad_so_name() -> String {
    cdylib_name("veles_plugin_badabi_so")
}

/// The tests that load the sample run one at a time (see the module
/// docs). A tokio mutex: the guard is held across awaits.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Depth-limited walk of `target/` for the cdylib. Covers the flat
/// layout (`target/debug`, `target/release`), the per-target layout nix
/// uses (`target/<triple>/release`), and anything else cargo invents —
/// the artifact's NAME is the contract, not its directory.
fn find_named(dir: &Path, want: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() && p.file_name().is_some_and(|n| n == want) {
            return Some(p);
        }
        if p.is_dir() {
            // `deps/` holds hash-suffixed copies, `build/` holds script
            // output — neither is the artifact we want to load.
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if name != "deps" && name != "build" && name != "incremental" && !name.starts_with('.')
            {
                subdirs.push(p);
            }
        }
    }
    subdirs
        .into_iter()
        .find_map(|d| find_named(&d, want, depth - 1))
}

fn target_dir() -> Option<PathBuf> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Some(crate_dir.parent()?.parent()?.join("target"))
}

fn so_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("VELES_PLUGIN_SO") {
        let p = PathBuf::from(explicit);
        return p.exists().then_some(p);
    }
    find_named(&target_dir()?, &so_name(), 4)
}

fn bad_so_path() -> Option<PathBuf> {
    find_named(&target_dir()?, &bad_so_name(), 4)
}

/// True when the gate is demanding real artifacts rather than allowing
/// a skip. `scripts/precommit.sh` and the nix `preCheck` both set it.
fn required() -> bool {
    std::env::var("VELES_REQUIRE_PLUGIN_SO")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

/// The sample, or a loud skip.
macro_rules! sample_or_skip {
    () => {
        match so_path() {
            Some(p) => p,
            None => {
                let so_name = so_name();
                assert!(
                    !required(),
                    "VELES_REQUIRE_PLUGIN_SO is set but {so_name} is nowhere under target/ — \
                     the gate must never skip the ABI test. Build it: \
                     cargo build -p veles-plugin-sample-so"
                );
                eprintln!(
                    "SKIP: {so_name} not built — run \
                     `cargo build -p veles-plugin-sample-so` (scripts/precommit.sh does this)"
                );
                return;
            }
        }
    };
}

/// One scratch home for the whole process, set BEFORE anything can
/// build the engine bridge: `host_call` reaches the bridge's own store
/// (`$VELES_HOME/state.db`), the bridge is built once per process by
/// whichever test first files a note or asks an op, and a test that
/// then sets its own home would be reading a different file.
fn scratch_home() -> &'static Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let d = tempfile::TempDir::new().expect("scratch home");
        std::fs::write(d.path().join("config.toml"), "").expect("config");
        std::env::set_var("VELES_HOME", d.path());
        d
    })
    .path()
}

/// A session-shaped tool dispatch, the way a turn would do it: a
/// registry built after the load, over the process's one scratch store
/// (`conformance::shared_store`; a fresh store per call cost ~14
/// fdatasyncs, which on the nix sandbox's disk-backed TMPDIR outran the
/// timer the cancel case below races).
async fn dispatch(name: &str, args: serde_json::Value) -> String {
    let reg = veles_core::tools::builtin_registry(conformance::shared_store(), None, "so_test");
    reg.dispatch(name, args).await
}

/// Unloads its plugin when dropped, so a test that panics midway leaves
/// nothing loaded for the next one to trip over (2026-10-13: one red
/// cancel left `sample-so-services` mapped, and the unload test then
/// found ITS `soask` "survived"). A second unload after the test's own
/// is an error, ignored.
struct UnloadOnDrop(&'static str);
impl Drop for UnloadOnDrop {
    fn drop(&mut self) {
        let _ = unload_so(self.0);
    }
}

fn owned_tools(owner: &str) -> Vec<String> {
    veles_core::tools::global_tools_with_owners()
        .into_iter()
        .filter(|(_, o)| *o == Some(owner))
        .map(|(n, _)| n)
        .collect()
}

/// `"… handle N"` → N.
fn handle_in(s: &str) -> u64 {
    s.rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no handle in: {s}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_so_sample_loads_and_drives_every_surface() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();

    // Direct load, the way the boot path does it.
    load_so(&so, "sample-so", "test").expect("the sample .so loads");

    // HOOK: verdict only on the marker; no opinion otherwise.
    let v = run_hooks("post_llm_call", r#"{"content":"so-magic"}"#);
    assert_eq!(v.as_deref(), Some(r#"{"decision":"saw-it-from-so"}"#));
    assert!(run_hooks("post_llm_call", r#"{"content":"plain"}"#).is_none());

    // GATEWAY FILTER: rewrite the magic text, pass everything else.
    let d = run_gateway_filters("irc", "chat1", "!soping");
    assert!(
        matches!(d, Some(FilterDecision::Rewrite(ref s)) if s == "pong from the .so"),
        "filter verdict: {d:?}"
    );
    assert!(run_gateway_filters("irc", "chat1", "hello").is_none());

    // TOOL: rides register_global_tool under the plugin's owner,
    // visible through a session registry built AFTER the load.
    assert!(
        owned_tools("so:sample-so").iter().any(|n| n == "so_echo"),
        "the tool carries the plugin's owner: {:?}",
        veles_core::tools::global_tools_with_owners()
    );
    let out = dispatch("so_echo", serde_json::json!({"text":"across the boundary"})).await;
    assert!(out.contains("so_echo"), "the .so tool answered: {out}");
    assert!(out.contains("across the boundary"), "{out}");

    // MANIFEST DISCOVERY: kind = "rust" + entry = plugin.so, loaded by
    // the same function the boot path calls. A different name so it
    // coexists with the direct load above.
    let home = tempfile::TempDir::new().expect("tempdir");
    let pdir = home.path().join("plugins").join("sample-so-manifest");
    std::fs::create_dir_all(&pdir).expect("plugin dir");
    std::fs::write(
        pdir.join("plugin.toml"),
        "name = \"sample-so-manifest\"\nkind = \"rust\"\nentry = \"plugin.so\"\n",
    )
    .expect("manifest");
    std::fs::copy(&so, pdir.join("plugin.so")).expect("copy .so into place");
    let discovered =
        veles_core::plugins::discover_in(&home.path().join("plugins"), &[], &[]).expect("discover");
    assert_eq!(discovered.len(), 1, "the rust manifest is discovered");
    load_rust_manifests(&discovered);
    // Its hook fires too — the registry holds BOTH plugins now.
    let v = run_hooks("post_llm_call", r#"{"content":"so-magic"}"#);
    assert_eq!(v.as_deref(), Some(r#"{"decision":"saw-it-from-so"}"#));

    // Re-loading the same manifest REPLACES, never stacks (a reload must
    // not double every handler).
    load_rust_manifests(&discovered);
    let count = veles_core::rustplugins::plugins()
        .iter()
        .filter(|p| p.name() == "sample-so-manifest")
        .count();
    assert_eq!(count, 1, "re-load replaces, does not stack");
    for n in ["sample-so", "sample-so-manifest"] {
        unload_so(n).expect("unload");
    }
}

/// The plugin calls the ENGINE: the sample's `so_ask` tool asks for
/// `sessions` and then hands the answer back through `json_get`, so the
/// round trip is host → plugin → host → plugin with the engine's own
/// JSON walker in the middle. No provider needed, so it runs anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_so_sample_calls_back_into_the_engine() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();
    load_so(&so, "sample-so-hostcall", "test").expect("the sample .so loads");
    // A session in the BRIDGE's store (the scratch home's state.db),
    // so the answer is a value and not just an absence.
    let store = Arc::new(std::sync::Mutex::new(
        veles_core::store::Store::open(&scratch_home().join("state.db")).expect("store"),
    ));
    store
        .lock()
        .unwrap()
        .create_session("seen-from-the-so")
        .expect("session");
    let reg = veles_core::tools::builtin_registry(store, None, "hostcall_test");
    // `dispatch` is async, and the tool's `call` hops to spawn_blocking
    // before touching the boundary — which is what makes the plugin's
    // blocking `host_call` legal.
    let out = reg.dispatch("so_ask", serde_json::json!({})).await;
    assert!(
        out.contains("first_session=seen-from-the-so"),
        "the .so must have read the session list THROUGH the engine, got: {out}"
    );
    unload_so("sample-so-hostcall").expect("unload");
}

/// A slash command from a `.so` is indistinguishable from a Tcl one to
/// every frontend that renders a command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_so_slash_command_resolves_and_renders_like_any_other() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();
    load_so(&so, "sample-so-command", "test").expect("the sample .so loads");
    let resolved = veles_core::commands::find_any("soask").expect("/soask is registered");
    let cmd = match resolved {
        veles_core::commands::ResolvedCommand::Plugin(c) => c,
        veles_core::commands::ResolvedCommand::Disk(_) => {
            panic!("a plugin command must win over anything on disk")
        }
    };
    assert_eq!(cmd.origin(), "native", "a .so command has no proc name");
    // The ASYNC surface (gateway, desktop API): a native body goes to
    // spawn_blocking, because it is sync by contract and may block on
    // an engine call.
    let rendered = veles_core::commands::render_plugin_async(&cmd, "the weather")
        .await
        .expect("render");
    assert_eq!(rendered, "The .so command was asked about: the weather");
    // And the SYNC surface (REPL, TUI, chat), which calls it directly.
    let rendered =
        veles_core::commands::render_plugin_blocking(&cmd, "again").expect("blocking render");
    assert_eq!(rendered, "The .so command was asked about: again");
    unload_so("sample-so-command").expect("unload");
}

/// An ABI 3 plugin is REFUSED from its version export, before any of
/// its code runs: the message names the file, the ABI it found, the
/// ABI the host loads and the rebuild; its init never ran (the marker
/// file it would write is absent); nothing is registered.
#[test]
fn an_abi_3_plugin_is_refused_before_its_init_runs() {
    let bad_so_name = bad_so_name();
    let Some(bad) = bad_so_path() else {
        assert!(
            !required(),
            "VELES_REQUIRE_PLUGIN_SO is set but {bad_so_name} is nowhere under target/ — \
             build it: cargo build -p veles-plugin-badabi-so"
        );
        eprintln!("SKIP: {bad_so_name} not built");
        return;
    };
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mark = tmp.path().join("badabi-init-ran");
    std::env::set_var("VELES_BADABI_MARK", &mark);
    let err = match load_so(&bad, "refuser", "test") {
        Ok(_) => panic!("an ABI 3 plugin must not load"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    for part in [
        "ABI 3 plugin",
        "loads ABI 4 only",
        "rebuild against veles-plugin-abi 4",
        bad_so_name.as_str(),
    ] {
        assert!(
            msg.contains(part),
            "the refusal must say `{part}`, got: {msg}"
        );
    }
    assert!(
        !mark.exists(),
        "the plugin's init RAN — the host read a v3 init through the v4 table"
    );
    // By NAME, not by count: the registry is process-global and the
    // tests in this file share it.
    assert!(
        !veles_core::rustplugins::plugins()
            .iter()
            .any(|p| p.name() == "refuser"),
        "a refused plugin must register nothing and appear nowhere"
    );
}

/// The other two rejection paths: a file dlopen cannot load at all, and
/// a real shared library that is not a veles plugin (no version export:
/// refused as "not a plugin, or an older one").
#[test]
fn a_library_that_is_not_a_plugin_is_rejected_by_name() {
    // Not a shared object at all.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let junk = tmp.path().join("not-a-library.so");
    std::fs::write(&junk, b"#!/bin/sh\necho hello\n").expect("write");
    let err = match load_so(&junk, "junk", "test") {
        Ok(_) => panic!("text is not a library"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("dlopen"), "the step must be named: {msg}");
    assert!(msg.contains("not-a-library.so"), "{msg}");
    // A real library with none of our symbols: a proc-macro dylib out
    // of `target/`, which any build of this workspace produces.
    let Some(target) = target_dir() else { return };
    let Some(stranger) = ["debug", "release"]
        .iter()
        .flat_map(|profile| {
            std::fs::read_dir(target.join(profile).join("deps"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .collect::<Vec<_>>()
        })
        .find(|p| {
            p.extension().is_some_and(|e| e == "so")
                && !p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains("veles_plugin")
        })
    else {
        eprintln!("SKIP: no unrelated .so under target/*/deps to test the symbol check with");
        return;
    };
    let err = match load_so(&stranger, "stranger", "test") {
        Ok(_) => panic!("a library without our symbols is not a plugin"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("veles_plugin_abi_version"),
        "the error must name the export that is missing, got: {msg}"
    );
}

/// A reload must not double the plugin's tools: four loads of one
/// manifest leave ONE `so_echo` under the plugin's owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reloading_a_plugin_does_not_double_its_tools() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();
    let home = tempfile::TempDir::new().expect("tempdir");
    let pdir = home.path().join("plugins").join("reload-me");
    std::fs::create_dir_all(&pdir).expect("plugin dir");
    std::fs::write(
        pdir.join("plugin.toml"),
        "name = \"reload-me\"\nkind = \"rust\"\nentry = \"plugin.so\"\n",
    )
    .expect("manifest");
    std::fs::copy(&so, pdir.join("plugin.so")).expect("copy");
    let discovered =
        veles_core::plugins::discover_in(&home.path().join("plugins"), &[], &[]).expect("discover");
    // Four loads, as four reloads would be.
    for _ in 0..4 {
        load_rust_manifests(&discovered);
    }
    let names = owned_tools("so:reload-me");
    for tool in ["so_echo", "so_ask"] {
        assert_eq!(
            names.iter().filter(|n| n.as_str() == tool).count(),
            1,
            "`{tool}` must appear once after four loads, got: {names:?}"
        );
    }
    unload_so("reload-me").expect("unload");
    assert!(owned_tools("so:reload-me").is_empty());
}

/// Unload is the M3 teardown for the tier: every registration the
/// plugin made goes (tools by owner, the command, the hook), the
/// listing says `unloaded`, the watcher's reload leaves it out, and
/// `start` brings it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unload_purges_every_registration_and_start_brings_it_back() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();
    let home = tempfile::TempDir::new().expect("tempdir");
    let pdir = home.path().join("plugins").join("unload-me");
    std::fs::create_dir_all(&pdir).expect("plugin dir");
    std::fs::write(
        pdir.join("plugin.toml"),
        "name = \"unload-me\"\nkind = \"rust\"\nentry = \"plugin.so\"\n",
    )
    .expect("manifest");
    std::fs::copy(&so, pdir.join("plugin.so")).expect("copy");
    let discovered =
        veles_core::plugins::discover_in(&home.path().join("plugins"), &[], &[]).expect("discover");
    load_rust_manifests(&discovered);
    let _unload = UnloadOnDrop("unload-me");
    assert!(!owned_tools("so:unload-me").is_empty());
    assert!(veles_core::commands::plugin_command("soask").is_some());

    let line = veles_core::rt::spawn_blocking(|| unload_so("unload-me"))
        .await
        .expect("join")
        .expect("unload");
    assert!(line.contains("unloaded"), "{line}");
    assert!(
        line.contains("registration(s) purged"),
        "the line counts what went: {line}"
    );
    assert!(
        owned_tools("so:unload-me").is_empty(),
        "tools survived the unload"
    );
    assert!(
        !veles_core::rustplugins::plugins()
            .iter()
            .any(|p| p.name() == "unload-me"),
        "the plugin survived the unload"
    );
    assert!(
        veles_core::commands::plugin_command("soask").is_none(),
        "the command survived the unload"
    );
    assert!(veles_core::rustplugins::is_so_unloaded("unload-me"));

    // The watcher's reload (every load_rust_manifests) leaves it out.
    load_rust_manifests(&discovered);
    assert!(
        owned_tools("so:unload-me").is_empty(),
        "a watcher reload resurrected an unloaded plugin"
    );
    // A direct load (what `start` does) brings it back.
    load_so(&pdir.join("plugin.so"), "unload-me", "so").expect("load again");
    assert!(!veles_core::rustplugins::is_so_unloaded("unload-me"));
    assert!(owned_tools("so:unload-me").iter().any(|n| n == "so_echo"));
    unload_so("unload-me").expect("final unload");
}

/// What ABI 4 added, driven through the sample's tools: a hook
/// registered from a callback is the plugin's and `unregister` drops
/// it; a timer fires once and a cancelled one never; an async tool's
/// future is woken by a host timer; a platform gets the gateway's
/// outbound as JSON and publishes inbound; a socket round-trips bytes
/// over a local listener and the TLS lane reports its handshake
/// failure; unload releases the lot. One test, in order: these share
/// the sample's statics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_abi4_services_round_trip_and_unload_releases_them() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();

    // A gateway bus for the platform, held here so inbound is readable.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let bus = veles_core::bus::GatewayBus {
        inbound: in_tx,
        outbound: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        last_chat: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        state_path: tmp.path().join("gateway-state.json"),
    };
    veles_core::bus::set_global_bus(bus.clone());

    load_so(&so, "sample-so-services", "test").expect("the sample .so loads");
    let _unload = UnloadOnDrop("sample-so-services");
    let owner = "so:sample-so-services";

    // LATE HOOK: registered from a tool call, attributed by the id.
    let out = dispatch("so_late_hook", serde_json::json!({})).await;
    let h = handle_in(&out);
    assert!(h > 0, "{out}");
    assert_eq!(
        run_hooks("pre_tool_call", r#"{"content":"so-late-magic"}"#).as_deref(),
        Some(r#"{"decision":"late-hook-saw-it"}"#),
        "the late hook fires"
    );
    let out = dispatch("so_unregister", serde_json::json!({"handle": h})).await;
    assert!(out.contains("dropped"), "{out}");
    assert!(
        run_hooks("pre_tool_call", r#"{"content":"so-late-magic"}"#).is_none(),
        "unregister drops the late hook"
    );

    // TIMERS: one fires, a cancelled one never.
    let out = dispatch("so_timer", serde_json::json!({"ms": 30})).await;
    assert!(handle_in(&out) > 0, "{out}");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let log = dispatch("so_timer_log", serde_json::json!({})).await;
    assert!(log.contains("fired after 30 ms"), "timer log: {log}");
    // Long enough that the two dispatches before the cancel cannot
    // outlive it (at 150 ms a disk-backed TMPDIR did: the timer had
    // fired and the cancel found no handle), short enough that a cancel
    // the host ignored would show up in the log before the next read.
    let out = dispatch("so_timer", serde_json::json!({"ms": 500})).await;
    assert!(handle_in(&out) > 0, "{out}");
    let out = dispatch("so_timer_cancel", serde_json::json!({})).await;
    assert!(out.contains("cancelled"), "{out}");
    let out = dispatch("so_timer_cancel", serde_json::json!({})).await;
    assert!(
        out.contains("NoSuchHandle"),
        "a second cancel finds no timer: {out}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let log = dispatch("so_timer_log", serde_json::json!({})).await;
    assert_eq!(log.lines().count(), 1, "the cancelled timer fired: {log}");

    // ASYNC TOOL: its future is pending until the host timer wakes it.
    let out = dispatch("so_wait", serde_json::json!({"ms": 40})).await;
    assert_eq!(out, "so_wait: woke after 40 ms");
    // A future that PANICS when polled: the SDK's poll wrapper answers
    // an error that says so — never a success — and the host lives.
    let out = dispatch("so_wait", serde_json::json!({"ms": 1, "panic": true})).await;
    assert!(
        out.contains("error")
            && out.contains("panicked")
            && out.contains("boom from the .so's future"),
        "{out}"
    );
    let out = dispatch("so_wait", serde_json::json!({"ms": 10})).await;
    assert_eq!(out, "so_wait: woke after 10 ms");

    // PANIC: caught by the SDK, answered as an error, the host lives.
    let out = dispatch("so_panic", serde_json::json!({})).await;
    assert!(
        out.contains("error") && out.contains("panicked") && out.contains("boom from the .so"),
        "{out}"
    );
    let out = dispatch("so_echo", serde_json::json!({"text":"after the panic"})).await;
    assert!(out.contains("after the panic"), "{out}");

    // PLATFORM: the route is claimed; an outbound reaches `deliver` as
    // JSON; an inbound the plugin publishes arrives on the bus.
    assert!(bus.has_adapter("soecho"), "the platform's route is claimed");
    assert!(
        !veles_core::bus::is_public_feed("soecho"),
        "registered with public_feed = false"
    );
    let sent = bus.send_outbound(
        "soecho",
        veles_types::bus::OutboundMessage {
            chat_id: "room-1".into(),
            reply_to: None,
            text: "hello plugin".into(),
            reasoning: None,
            provider: "test".into(),
            model: "test".into(),
            kind: Default::default(),
            ask: None,
            meta: Default::default(),
        },
    );
    assert!(sent, "send_outbound found the route");
    let mut delivered = String::new();
    for _ in 0..50 {
        delivered = dispatch("so_delivered", serde_json::json!({})).await;
        if delivered.contains("hello plugin") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        delivered.contains("\"chat_id\":\"room-1\"") && delivered.contains("hello plugin"),
        "the outbound reached deliver as JSON: {delivered}"
    );
    let out = dispatch(
        "so_inbound",
        serde_json::json!({"text":"from the plugin","chat_id":"room-1"}),
    )
    .await;
    assert!(out.contains("published"), "{out}");
    let inbound = tokio::time::timeout(std::time::Duration::from_secs(2), in_rx.recv())
        .await
        .expect("an inbound within 2 s")
        .expect("the bus is open");
    assert_eq!(inbound.platform, "soecho");
    assert_eq!(inbound.text, "from the plugin");
    assert_eq!(inbound.chat_id, "room-1");

    // SOCKET: a local echo listener; `send` goes out once connected and
    // comes back as data; close delivers `closed`.
    // (the waits below are 10 s, not 2: under `nix flake check` on a busy
    // laptop the echo took longer than 2 s once, 2026-10-20 — "the echo
    // came back: connected" — while the same test passes in 3.5 s on the
    // host; the budget is a ceiling, the loop returns as soon as it sees
    // the line)
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 64];
            if let Ok(n) = s.read(&mut buf) {
                let _ = s.write_all(&buf[..n]);
            }
            // Keep it open until the client closes.
            let mut sink = [0u8; 64];
            while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
        }
    });
    let out = dispatch(
        "so_tcp",
        serde_json::json!({"host":"127.0.0.1","port":port,"send":"ping"}),
    )
    .await;
    assert!(handle_in(&out) > 0, "{out}");
    let mut log = String::new();
    for _ in 0..500 {
        log = dispatch("so_tcp_log", serde_json::json!({})).await;
        if log.contains("data: ping") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(log.contains("connected"), "socket log: {log}");
    assert!(log.contains("data: ping"), "the echo came back: {log}");
    let out = dispatch("so_tcp_close", serde_json::json!({})).await;
    assert!(out.contains("closing"), "{out}");
    for _ in 0..500 {
        log = dispatch("so_tcp_log", serde_json::json!({})).await;
        if log.contains("closed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(log.contains("closed"), "close delivers `closed`: {log}");
    assert_eq!(
        log.matches("connected").count(),
        1,
        "events arrive once each, in order: {log}"
    );

    // TLS lane: a plain listener cannot complete a handshake, and the
    // plugin hears that as an error — not a hang, not a crash.
    let plain = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let tls_port = plain.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok((mut s, _)) = plain.accept() {
            let _ = s.write_all(b"this is not TLS\r\n");
        }
    });
    let out = dispatch(
        "so_tcp",
        serde_json::json!({"host":"127.0.0.1","port":tls_port,"tls":true}),
    )
    .await;
    assert!(handle_in(&out) > 0, "{out}");
    for _ in 0..150 {
        log = dispatch("so_tcp_log", serde_json::json!({})).await;
        if log.contains("error: tls handshake") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        log.contains("error: tls handshake"),
        "the TLS lane reports: {log}"
    );

    // LISTING: the tier knows the plugin.
    assert!(
        veles_core::rustplugins::so_rows()
            .iter()
            .any(|(n, s, _)| n == "sample-so-services" && *s == "loaded"),
        "{:?}",
        veles_core::rustplugins::so_rows()
    );

    // UNLOAD: a pending timer, a LIVE socket and the platform are
    // outstanding; the purge releases them (counted) and aborts their
    // tasks, so nothing holds the library and the code is unmapped;
    // the route is gone and nothing is registered under the owner.
    dispatch("so_timer", serde_json::json!({"ms": 5000})).await;
    let parked = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let parked_port = parked.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((_s, _)) = parked.accept() {
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    });
    let out = dispatch(
        "so_tcp",
        serde_json::json!({"host":"127.0.0.1","port":parked_port}),
    )
    .await;
    assert!(handle_in(&out) > 0, "{out}");
    for _ in 0..500 {
        log = dispatch("so_tcp_log", serde_json::json!({})).await;
        if log.matches("connected").count() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        log.matches("connected").count(),
        2,
        "the parked socket connected: {log}"
    );
    // A second parked socket, TLS against the same silent listener: its
    // task sits in the handshake, where no command channel is polled,
    // so only the purge's ABORT ends it — and it holds the library.
    let tls_errors = log.matches("error: tls").count();
    let out = dispatch(
        "so_tcp",
        serde_json::json!({"host":"127.0.0.1","port":parked_port,"tls":true}),
    )
    .await;
    assert!(handle_in(&out) > 0, "{out}");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    log = dispatch("so_tcp_log", serde_json::json!({})).await;
    assert_eq!(
        log.matches("error: tls").count(),
        tls_errors,
        "the TLS-parked socket must still be in its handshake: {log}"
    );
    assert_eq!(log.matches("connected").count(), 2, "{log}");
    assert!(veles_core::soservices::count_owner(owner) > 0);
    let line = veles_core::rt::spawn_blocking(|| unload_so("sample-so-services"))
        .await
        .expect("join")
        .expect("unload");
    assert!(line.contains("unloaded"), "{line}");
    assert!(
        line.contains("host resource(s) released"),
        "the line counts the services: {line}"
    );
    assert!(
        line.contains("code unmapped"),
        "a purged socket's task must let go of the library: {line}"
    );
    assert_eq!(veles_core::soservices::count_owner(owner), 0);
    assert!(
        !bus.has_adapter("soecho"),
        "the platform's route is released"
    );
    assert!(owned_tools(owner).is_empty());
    let timer_lines = log.lines().count();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        log.lines().count(),
        timer_lines,
        "nothing reaches an unloaded plugin"
    );
}

/// The conformance suite, on the native tier.
struct SoTier {
    so: PathBuf,
}

impl conformance::Tier for SoTier {
    fn fixture(&self) -> conformance::Fixture {
        conformance::Fixture {
            owner: "so:sample-so-conformance",
            tool: "so_echo",
            tool_marker: "so_echo:",
            panic_tool: Some("so_panic"),
            command: "soask",
            command_marker: "The .so command was asked about",
            hook: "post_llm_call",
            hook_magic: "so-magic",
            hook_verdict: r#"{"decision":"saw-it-from-so"}"#,
            point_tag: Some("so"),
        }
    }
    fn load(&mut self) -> conformance::BoxFut<'_, ()> {
        Box::pin(async move {
            load_so(&self.so, "sample-so-conformance", "test").expect("load");
        })
    }
    fn unload(&mut self) -> conformance::BoxFut<'_, String> {
        Box::pin(async move {
            veles_core::rt::spawn_blocking(|| unload_so("sample-so-conformance"))
                .await
                .expect("join")
                .expect("unload")
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_native_tier_passes_the_conformance_suite() {
    let so = sample_or_skip!();
    let _s = SERIAL.lock().await;
    let _home = scratch_home();
    let mut tier = SoTier { so };
    let passed = conformance::run_all(&mut tier).await;
    assert_eq!(passed.len(), 5, "{passed:?}");
}
