//! `veles module-host-test <case>` — the R3 module-host tests, run in
//! THIS binary (PLAN-plugins-installer.md R3).
//!
//! A `cargo test` binary links veles-core STATICALLY, so it has no
//! runtime to give a module (`module::check`'s `HostWithoutRuntime`
//! refusal). The module host's tests therefore run here: `veles` links
//! `libveles_runtime.so`, and `module::check` passes. Each case prints
//! `OK <case>` and returns; a failure panics (the exit code says so).
//!
//! cases: load-unload, stray-spawn, spin-keeps-mapped, quit-cut-short,
//! standby-quit, stale-call, fault-restart, key-mismatch, rescan, irc-module, slopdrop-module, telegram-module,
//! signal-pin;
//! irc-live is manual
//! (a real IRC server, see its doc).

use anyhow::Context;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use veles_core::bus::GatewayBus;
use veles_core::modulehost::{self, ModuleHost};
use veles_core::pluginhost::{Duty, LeaseGate, Ungated};
use veles_runtime::veles_core;
use veles_runtime::veles_types;

/// The echo module's built `.so`, beside this binary in deps/.
/// The conformance suite shared with the native tier (R8): one file,
/// included by path from both rigs.
#[path = "../../veles-core/tests/support/conformance.rs"]
mod conformance;

fn echo_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_echo.so")
}

/// The JS host module's built `.so` (R9: crates/veles-module-lang-js).
fn lang_js_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_lang_js.so")
}

/// The Racket host module's built `.so` (R9: crates/veles-module-lang-racket).
fn lang_racket_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_lang_racket.so")
}

/// The Common Lisp host module's built `.so` (R9: crates/veles-module-lang-lisp).
fn lang_lisp_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_lang_lisp.so")
}

/// The IRC module's built `.so` (R4: crates/veles-module-irc), same
/// build, same deps dir.
fn irc_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_irc.so")
}

/// The slopdrop module's built `.so` (R5: crates/veles-module-slopdrop).
fn slopdrop_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_slopdrop.so")
}

/// The /atproto module's built `.so` (crates/veles-module-atproto-cmd).
fn atproto_cmd_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_atproto_cmd.so")
}

/// The Telegram module's built `.so` (R6: crates/veles-module-telegram).
fn telegram_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| Path::new("target/debug"))
        .join("deps/libveles_module_telegram.so")
}

/// A bus over a scratch home's state path (every rig builds one).
fn test_bus(home: &Path) -> GatewayBus {
    let (tx, _inbound) = mpsc::unbounded_channel();
    GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.join("gateway-state.json"),
    }
}

fn fast() -> modulehost::HostOptions {
    modulehost::HostOptions {
        start_timeout: Duration::from_secs(10),
        stop_deadline: Duration::from_secs(3),
        shutdown_timeout: Duration::from_secs(3),
        drain_timeout: Duration::from_secs(3),
        backoff_base: Duration::from_millis(100),
        backoff_max: Duration::from_millis(400),
        stable_after: Duration::from_secs(5),
    }
}

/// A scratch home, a module root with the echo module installed
/// (content-addressed), and a host over a test bus. Returns the host,
/// the tempdir (keep it alive), the module dir and the installed path.
async fn rig(config: &str) -> (ModuleHost, tempfile::TempDir, PathBuf, PathBuf) {
    rig_gated(config, Arc::new(Ungated), "running").await
}

/// A cluster whose arbiter never grants: an instance with a lease stands
/// by for it for good, as Telegram does on a node the VPS out-holds.
struct NeverGranted;

#[async_trait::async_trait]
impl LeaseGate for NeverGranted {
    async fn gated(&self, _key: &str, _what: &str, _duty: Duty) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

/// [`rig`] behind `gate`, waiting for the echo instance to reach `state`.
async fn rig_gated(
    config: &str,
    gate: Arc<dyn LeaseGate>,
    state: &str,
) -> (ModuleHost, tempfile::TempDir, PathBuf, PathBuf) {
    let home = tempfile::tempdir().expect("scratch home");
    // XDG roots under the scratch home so the host's state dirs land there.
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), config).unwrap();

    let root = home.path().join("modules");
    let dir = root.join("echo-module");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &echo_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"echo-module\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = ModuleHost::new(bus, "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            gate,
        )
        .await;
    assert_eq!(
        started,
        vec!["echo-module".to_string()],
        "started {started:?}"
    );
    wait_for(&host, "echo-module", state, Duration::from_secs(10)).await;
    (host, home, dir, so)
}

async fn wait_for(host: &ModuleHost, label: &str, state: &str, within: Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
        let now = host.status().into_iter().find(|s| {
            if s.instance.is_empty() {
                s.plugin == label
            } else {
                format!("{}:{}", s.plugin, s.instance) == label
            }
        });
        if let Some(s) = &now {
            if s.state == state {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label} never reached {state}: {now:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// load-unload: the module loads, registers, reloads, stops, and its
/// file is unmapped after teardown.
async fn load_unload() {
    let (host, _home, _dir, so) = rig("").await;
    let so = so.canonicalize().unwrap();

    assert!(
        veles_core::commands::plugin_command("echo-module").is_some(),
        "the module's command is registered"
    );
    assert_eq!(
        veles_core::layers::installed(),
        vec![("prompt", Some("module:echo-module"))],
        "the module's layer is installed under its owner (R7)"
    );

    let out = host.reload("echo-module").await.expect("reload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;

    let out = host.stop("echo-module").await.expect("stop");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        veles_core::commands::plugin_command("echo-module").is_none(),
        "the command survived the purge"
    );
    assert!(
        veles_core::layers::installed().is_empty(),
        "the layer survived the purge: {:?}",
        veles_core::layers::installed()
    );
    for _ in 0..100 {
        if !ModuleHost::so_mapped(&so) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!ModuleHost::so_mapped(&so), "still mapped after teardown");

    // `unload` (R7): the instance is started again, then unloaded — the
    // same teardown, and the row is gone until a `rescan` finds the
    // install again (which also proves the command returns with it).
    host.start_again("echo-module").await.expect("start");
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;
    assert!(veles_core::commands::plugin_command("echo-module").is_some());
    let out = host.unload("echo-module").await.expect("unload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        !host.status().iter().any(|s| s.plugin == "echo-module"),
        "an unloaded module leaves the listing: {:?}",
        host.status()
    );
    assert!(
        veles_core::commands::plugin_command("echo-module").is_none(),
        "its command went with it"
    );
    assert!(
        host.unload("echo-module").await.is_err(),
        "not a target any more"
    );
    let found = host.rescan().await;
    assert_eq!(
        found,
        vec!["echo-module".to_string()],
        "rescan finds the install again"
    );
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;
    assert!(veles_core::commands::plugin_command("echo-module").is_some());
    // Leave nothing running: this rig's host is not the process-global
    // one `run` shuts down, and an instance runtime still alive at exit
    // is dropped inside the main runtime (a panic, then a segfault).
    host.unload("echo-module").await.expect("final unload");
    assert!(host.status().is_empty(), "{:?}", host.status());
}

/// stray-spawn: teardown completes within the deadline even though the
/// module parked a forever task in start (the runtime's shutdown_timeout
/// drops it; the spike proved the guard fires).
/// The module tier under the shared conformance suite (R8): the echo
/// module as the fixture, `reload` as its load, `unload` + `rescan` as
/// the round trip. No panicking tool: a module's tool runs on the
/// caller's task (see the echo module).
struct ModuleTier {
    host: ModuleHost,
}

impl conformance::Tier for ModuleTier {
    fn fixture(&self) -> conformance::Fixture {
        conformance::Fixture {
            owner: "module:echo-module",
            tool: "echo_tool",
            tool_marker: "echo_tool:",
            panic_tool: None,
            command: "echo-module",
            command_marker: "echo-module",
            hook: "post_llm_call",
            hook_magic: "echo-magic",
            hook_verdict: r#"{"decision":"saw-it-from-echo"}"#,
            point_tag: Some("module"),
        }
    }
    fn load(&mut self) -> conformance::BoxFut<'_, ()> {
        Box::pin(async move {
            let listed = self.host.status().iter().any(|s| s.plugin == "echo-module");
            if listed {
                self.host.reload("echo-module").await.expect("reload");
            } else {
                let found = self.host.rescan().await;
                assert_eq!(found, vec!["echo-module".to_string()], "rescan: {found:?}");
            }
            wait_for(
                &self.host,
                "echo-module",
                "running",
                Duration::from_secs(10),
            )
            .await;
        })
    }
    fn unload(&mut self) -> conformance::BoxFut<'_, String> {
        Box::pin(async move {
            self.host
                .unload("echo-module")
                .await
                .map(|lines| lines.join("\n"))
                .unwrap_or_else(|e| e)
        })
    }
}

async fn conformance_case() {
    let (host, _home, _dir, _so) = rig("").await;
    let mut tier = ModuleTier { host };
    let passed = conformance::run_all(&mut tier).await;
    println!(
        "conformance (module tier): {} scenario(s) passed: {passed:?}",
        passed.len()
    );
    assert_eq!(passed.len(), 4, "{passed:?}");
    assert!(tier.host.status().is_empty(), "{:?}", tier.host.status());
}

/// The script tier under the conformance suite (R9): the lang-js
/// module as the host, a TypeScript fixture plugin under the scratch
/// home's plugins dir as the fixture. `load` is the seam's `start`
/// verb (a loaded script is reloaded), `unload` its `unload`; the
/// fixture's registrations carry `js:js-fixture`.
struct JsTier {
    host: ModuleHost,
}

const JS_FIXTURE: &str = r#"// The conformance fixture, in TypeScript: one tool, one command,
// one hook, each answering with its marker.
interface Args { text?: string }
enum Marker { Tool = "js_echo_tool:", Command = "js-fixture" }
await veles.registerTool(
  { name: "js_echo_tool", description: "echo with a marker", schema: { type: "object", properties: { text: { type: "string" } } } },
  async (args: Args) => `${Marker.Tool} ${JSON.stringify(args)}`,
);
await veles.registerCommand("js-fixture", "the js conformance command", (text: string) => `${Marker.Command} ${text}`);
veles.hook("post_llm_call", (args: { content?: string }) =>
  args.content === "js-magic" ? { decision: "saw-it-from-js" } : undefined,
);
// The suite's override point: what the layer below answers, with "js" after it.
await veles.registerService(
  { name: "conformance.point", description: "the suite's point, wrapped" },
  async (params: object, cx: { next: (p: object) => Promise<{ via: string[] }> }) => {
    const v = await cx.next(params);
    v.via.push("js");
    return v;
  },
);
"#;

impl conformance::Tier for JsTier {
    fn fixture(&self) -> conformance::Fixture {
        conformance::Fixture {
            owner: "js:js-fixture",
            tool: "js_echo_tool",
            tool_marker: "js_echo_tool:",
            panic_tool: None,
            command: "js-fixture",
            command_marker: "js-fixture",
            hook: "post_llm_call",
            hook_magic: "js-magic",
            hook_verdict: r#"{"decision":"saw-it-from-js"}"#,
            point_tag: Some("js"),
        }
    }
    fn load(&mut self) -> conformance::BoxFut<'_, ()> {
        Box::pin(async move {
            let fut = veles_core::scripthost::request("js-fixture", "start")
                .expect("js-fixture is an enabled js plugin");
            let line = fut.await.expect("the fixture loads");
            assert!(line.contains("js:js-fixture: loaded"), "{line}");
            assert!(line.contains("TypeScript stripped"), "{line}");
        })
    }
    fn unload(&mut self) -> conformance::BoxFut<'_, String> {
        Box::pin(async move {
            let fut = veles_core::scripthost::request("js-fixture", "unload")
                .expect("js-fixture is an enabled js plugin");
            fut.await.unwrap_or_else(|e| e)
        })
    }
}

/// The Racket script tier under the conformance suite (R9): the
/// lang-racket module as the host, a Racket fixture in the scratch
/// home's plugins dir. Same verbs as the JS tier; the fixture's
/// registrations carry `racket:racket-fixture`.
struct RacketTier {
    host: ModuleHost,
}

const RACKET_FIXTURE: &str = r#"#lang racket/base
;; The conformance fixture, in Racket: one tool, one command, one hook,
;; each answering with its marker.
(require veles json)
(register-tool "racket_echo_tool" "echo with a marker"
  (hash 'type "object" 'properties (hash 'text (hash 'type "string")))
  (lambda (args session-id) (format "racket_echo_tool: ~a" (jsexpr->string args))))
(register-command "racket-fixture" "the racket conformance command"
  (lambda (text) (format "racket-fixture ~a" text)))
(hook "post_llm_call"
  (lambda (args)
    (if (equal? (hash-ref args 'content #f) "racket-magic")
        (hash 'decision "saw-it-from-racket")
        (void))))
;; The suite's override point: what the layer below answers, with "racket" after it.
(register-service "conformance.point" "the suite's point, wrapped"
  (lambda (params caller session-id)
    (define v (service-next params))
    (hash-set v 'via (append (hash-ref v 'via) (list "racket")))))
"#;

impl conformance::Tier for RacketTier {
    fn fixture(&self) -> conformance::Fixture {
        conformance::Fixture {
            owner: "racket:racket-fixture",
            tool: "racket_echo_tool",
            tool_marker: "racket_echo_tool:",
            panic_tool: None,
            command: "racket-fixture",
            command_marker: "racket-fixture",
            hook: "post_llm_call",
            hook_magic: "racket-magic",
            hook_verdict: r#"{"decision":"saw-it-from-racket"}"#,
            point_tag: Some("racket"),
        }
    }
    fn load(&mut self) -> conformance::BoxFut<'_, ()> {
        Box::pin(async move {
            let fut = veles_core::scripthost::request("racket-fixture", "start")
                .expect("racket-fixture is an enabled racket plugin");
            let line = fut.await.expect("the fixture loads");
            assert!(line.contains("racket:racket-fixture: loaded"), "{line}");
            assert!(line.contains("4 callback(s)"), "{line}");
        })
    }
    fn unload(&mut self) -> conformance::BoxFut<'_, String> {
        Box::pin(async move {
            let fut = veles_core::scripthost::request("racket-fixture", "unload")
                .expect("racket-fixture is an enabled racket plugin");
            fut.await.unwrap_or_else(|e| e)
        })
    }
}

/// The module host pins the gateway's signal handlers before the first
/// module's code runs. lang-racket's boot saves the dispositions it finds
/// and puts them back; a handler that went in during it was put back to
/// SIG_DFL, and the VPS gateway ignored its stop (2026-10-10). This
/// process has no SIGTERM handler before the start, or the check below
/// would prove nothing.
async fn signal_pin() {
    let before = veles_core::lifecycle::caught_signals();
    assert!(
        !before.contains(&libc::SIGTERM),
        "this case must start with SIGTERM uncaught, or it proves nothing: {before:?}"
    );
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();
    let root = home.path().join("modules");
    std::fs::create_dir_all(&root).unwrap();
    let host = ModuleHost::new(test_bus(home.path()), "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert!(started.is_empty(), "nothing installed: {started:?}");
    let caught = veles_core::lifecycle::caught_signals();
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
        assert!(
            caught.contains(&sig),
            "signal {sig} is not caught after the module host started: {caught:?}"
        );
    }
}

/// conformance-racket: the Racket host loads as a module (booting Racket
/// CS in this process), the seam loads the fixture from the plugins
/// dir, the three scenarios pass, and unloading the module takes the
/// host entry and the script's registrations away — while Racket stays
/// booted and the module is reported kept mapped, by design.
async fn conformance_racket_case() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();
    let plugin_dir = veles_core::plugins::plugins_dir().join("racket-fixture");
    assert!(
        plugin_dir.starts_with(home.path()),
        "{}",
        plugin_dir.display()
    );
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "name = \"racket-fixture\"\nkind = \"racket\"\nentry = \"plugin.rkt\"\ndescription = \"the racket conformance fixture\"\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("plugin.rkt"), RACKET_FIXTURE).unwrap();
    let rows = veles_core::scripthost::rows();
    assert_eq!(
        rows.iter()
            .map(|r| (r.name.as_str(), r.state))
            .collect::<Vec<_>>(),
        vec![("racket-fixture", "no-host")],
        "before the host: {rows:?}"
    );
    let root = home.path().join("modules");
    let dir = root.join("lang-racket");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &lang_racket_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lang-racket\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();
    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    // Racket's own boot deadline is 30 s. The generic `fast()` fixture
    // allows only 10 s, which cancelled a legitimate 11 s cold boot in
    // the Nix full-suite run and then asserted against the RETRY's
    // mapping rather than the original engine-owning instance. Give the
    // real boot its documented bound plus the fixture load margin; do
    // not weaken the kept-mapped assertion or the production timeout.
    let host = ModuleHost::new(
        bus,
        "node-under-test",
        modulehost::HostOptions {
            start_timeout: Duration::from_secs(120),
            ..fast()
        },
    );
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(
        started,
        vec!["lang-racket".to_string()],
        "started {started:?}"
    );
    wait_for(&host, "lang-racket", "running", Duration::from_secs(40)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let rows = veles_core::scripthost::rows();
        if rows
            .iter()
            .any(|r| r.name == "racket-fixture" && r.state == "loaded")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture never loaded: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        veles_core::tools::global_tools_with_owners()
            .iter()
            .any(|(n, o)| n == "racket_echo_tool" && *o == Some("racket:racket-fixture")),
        "{:?}",
        veles_core::tools::global_tools_with_owners()
    );
    // With Racket up, a stop still reaches the gateway: the boot put back
    // the handlers the host pinned before it (SigCgt, the kernel's list).
    let caught = veles_core::lifecycle::caught_signals();
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
        assert!(
            caught.contains(&sig),
            "signal {sig} is not caught with Racket up: {caught:?}"
        );
    }
    let mut tier = RacketTier { host };
    let passed = conformance::run_all(&mut tier).await;
    println!(
        "conformance (racket tier): {} scenario(s) passed: {passed:?}",
        passed.len()
    );
    assert_eq!(passed.len(), 4, "{passed:?}");
    conformance::Tier::load(&mut tier).await;
    assert!(veles_core::scripthost::hosted_kinds().contains(&"racket".to_string()));
    let out = tier.host.unload("lang-racket").await.expect("unload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    // Racket's engine thread keeps the module mapped: said so, by design.
    assert!(
        out.iter().any(|l| l.contains("kept mapped")),
        "the Racket host must be reported kept mapped: {out:?}"
    );
    assert!(!veles_core::scripthost::hosted_kinds().contains(&"racket".to_string()));
    assert!(
        !veles_core::tools::global_tools_with_owners()
            .iter()
            .any(|(_, o)| *o == Some("racket:racket-fixture")),
        "{:?}",
        veles_core::tools::global_tools_with_owners()
    );
    assert_eq!(veles_core::scripthost::rows()[0].state, "no-host");
    assert!(
        veles_core::scripthost::engine_booted("racket"),
        "the boot mark stays with the process"
    );
    let _ = so;
}

/// The Common Lisp script tier under the conformance suite (R9): the
/// lang-lisp module as the host, a Lisp fixture in the scratch home's
/// plugins dir. Same verbs as the JS tier; the fixture's registrations
/// carry `lisp:lisp-fixture`.
struct LispTier {
    host: ModuleHost,
}

const LISP_FIXTURE: &str = r#";;;; The conformance fixture, in Common Lisp: one tool, one command, one
;;;; hook, each answering with its marker.
(register-tool "lisp_echo_tool" "echo with a marker"
  (obj "type" "object" "properties" (obj "text" (obj "type" "string")))
  (lambda (args session-id)
    (declare (ignore session-id))
    (format nil "lisp_echo_tool: ~a" (json-string args))))
(register-command "lisp-fixture" "the lisp conformance command"
  (lambda (text) (format nil "lisp-fixture ~a" text)))
(hook "post_llm_call"
  (lambda (args)
    (when (equal (gethash "content" args) "lisp-magic")
      (obj "decision" "saw-it-from-lisp"))))
;; The suite's override point: what the layer below answers, with "lisp" after it.
(register-service "conformance.point" "the suite's point, wrapped"
  (lambda (params caller session-id)
    (declare (ignore caller session-id))
    (let ((v (service-next params)))
      (setf (gethash "via" v) (concatenate 'vector (gethash "via" v) (vector "lisp")))
      v)))
"#;

impl conformance::Tier for LispTier {
    fn fixture(&self) -> conformance::Fixture {
        conformance::Fixture {
            owner: "lisp:lisp-fixture",
            tool: "lisp_echo_tool",
            tool_marker: "lisp_echo_tool:",
            panic_tool: None,
            command: "lisp-fixture",
            command_marker: "lisp-fixture",
            hook: "post_llm_call",
            hook_magic: "lisp-magic",
            hook_verdict: r#"{"decision":"saw-it-from-lisp"}"#,
            point_tag: Some("lisp"),
        }
    }
    fn load(&mut self) -> conformance::BoxFut<'_, ()> {
        Box::pin(async move {
            let fut = veles_core::scripthost::request("lisp-fixture", "start")
                .expect("lisp-fixture is an enabled lisp plugin");
            let line = fut.await.expect("the fixture loads");
            assert!(line.contains("lisp:lisp-fixture: loaded"), "{line}");
            assert!(line.contains("4 callback(s)"), "{line}");
        })
    }
    fn unload(&mut self) -> conformance::BoxFut<'_, String> {
        Box::pin(async move {
            let fut = veles_core::scripthost::request("lisp-fixture", "unload")
                .expect("lisp-fixture is an enabled lisp plugin");
            fut.await.unwrap_or_else(|e| e)
        })
    }
}

/// conformance-lisp: the Common Lisp host loads as a module (booting ECL
/// in this process), the seam loads the fixture from the plugins dir,
/// the three scenarios pass, and unloading the module takes the host
/// entry and the script's registrations away — while ECL stays booted
/// and the module is reported kept mapped, by design.
async fn conformance_lisp_case() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();
    let plugin_dir = veles_core::plugins::plugins_dir().join("lisp-fixture");
    assert!(
        plugin_dir.starts_with(home.path()),
        "{}",
        plugin_dir.display()
    );
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "name = \"lisp-fixture\"\nkind = \"lisp\"\nentry = \"plugin.lisp\"\ndescription = \"the lisp conformance fixture\"\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("plugin.lisp"), LISP_FIXTURE).unwrap();
    let rows = veles_core::scripthost::rows();
    assert_eq!(
        rows.iter()
            .map(|r| (r.name.as_str(), r.state))
            .collect::<Vec<_>>(),
        vec![("lisp-fixture", "no-host")],
        "before the host: {rows:?}"
    );
    let root = home.path().join("modules");
    let dir = root.join("lang-lisp");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &lang_lisp_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lang-lisp\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();
    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = ModuleHost::new(bus, "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(
        started,
        vec!["lang-lisp".to_string()],
        "started {started:?}"
    );
    wait_for(&host, "lang-lisp", "running", Duration::from_secs(40)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let rows = veles_core::scripthost::rows();
        if rows
            .iter()
            .any(|r| r.name == "lisp-fixture" && r.state == "loaded")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture never loaded: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        veles_core::tools::global_tools_with_owners()
            .iter()
            .any(|(n, o)| n == "lisp_echo_tool" && *o == Some("lisp:lisp-fixture")),
        "{:?}",
        veles_core::tools::global_tools_with_owners()
    );
    // With ECL up, a stop still reaches the gateway: the boot put back
    // the handlers the host pinned before it (SigCgt, the kernel's list).
    let caught = veles_core::lifecycle::caught_signals();
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
        assert!(
            caught.contains(&sig),
            "signal {sig} is not caught with ECL up: {caught:?}"
        );
    }
    let mut tier = LispTier { host };
    let passed = conformance::run_all(&mut tier).await;
    println!(
        "conformance (lisp tier): {} scenario(s) passed: {passed:?}",
        passed.len()
    );
    assert_eq!(passed.len(), 4, "{passed:?}");
    conformance::Tier::load(&mut tier).await;
    assert!(veles_core::scripthost::hosted_kinds().contains(&"lisp".to_string()));
    let out = tier.host.unload("lang-lisp").await.expect("unload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    // ECL's engine thread keeps the module mapped: said so, by design.
    assert!(
        out.iter().any(|l| l.contains("kept mapped")),
        "the Lisp host must be reported kept mapped: {out:?}"
    );
    assert!(!veles_core::scripthost::hosted_kinds().contains(&"lisp".to_string()));
    assert!(
        !veles_core::tools::global_tools_with_owners()
            .iter()
            .any(|(_, o)| *o == Some("lisp:lisp-fixture")),
        "{:?}",
        veles_core::tools::global_tools_with_owners()
    );
    assert_eq!(veles_core::scripthost::rows()[0].state, "no-host");
    assert!(
        veles_core::scripthost::engine_booted("lisp"),
        "the boot mark stays with the process"
    );
    let _ = so;
}

/// conformance-js: the JS host loads as a module, the seam lists and
/// loads the fixture script from the plugins dir, the three scenarios
/// pass, and unloading the module takes the host and its scripts'
/// registrations away.
async fn conformance_js_case() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();
    // The fixture plugin, where `plugins::discover` looks.
    let plugin_dir = veles_core::plugins::plugins_dir().join("js-fixture");
    assert!(
        plugin_dir.starts_with(home.path()),
        "the plugins dir must be under the scratch home: {}",
        plugin_dir.display()
    );
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "name = \"js-fixture\"\nkind = \"js\"\nentry = \"plugin.ts\"\ndescription = \"the js conformance fixture\"\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("plugin.ts"), JS_FIXTURE).unwrap();
    let rows = veles_core::scripthost::rows();
    assert_eq!(
        rows.iter()
            .map(|r| (r.name.as_str(), r.state))
            .collect::<Vec<_>>(),
        vec![("js-fixture", "no-host")],
        "before the host: {rows:?}"
    );
    // The host module.
    let root = home.path().join("modules");
    let dir = root.join("lang-js");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &lang_js_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lang-js\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();
    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = ModuleHost::new(bus, "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(started, vec!["lang-js".to_string()], "started {started:?}");
    wait_for(&host, "lang-js", "running", Duration::from_secs(20)).await;
    // The host's start loaded the fixture itself.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let rows = veles_core::scripthost::rows();
        if rows
            .iter()
            .any(|r| r.name == "js-fixture" && r.state == "loaded")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture never loaded: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        veles_core::tools::global_tools_with_owners()
            .iter()
            .any(|(n, o)| n == "js_echo_tool" && *o == Some("js:js-fixture")),
        "{:?}",
        veles_core::tools::global_tools_with_owners()
    );
    let mut tier = JsTier { host };
    let passed = conformance::run_all(&mut tier).await;
    println!(
        "conformance (js tier): {} scenario(s) passed: {passed:?}",
        passed.len()
    );
    assert_eq!(passed.len(), 4, "{passed:?}");
    // A loaded script's tool, then the module's unload: the host entry
    // and the script's registrations go with it.
    conformance::Tier::load(&mut tier).await;
    assert!(veles_core::scripthost::hosted_kinds().contains(&"js".to_string()));
    let out = tier.host.unload("lang-js").await.expect("unload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(!veles_core::scripthost::hosted_kinds().contains(&"js".to_string()));
    assert!(
        !veles_core::tools::global_tools_with_owners()
            .iter()
            .any(|(_, o)| *o == Some("js:js-fixture")),
        "{:?}",
        veles_core::tools::global_tools_with_owners()
    );
    assert_eq!(veles_core::scripthost::rows()[0].state, "no-host");
    assert!(tier.host.status().is_empty(), "{:?}", tier.host.status());
    let _ = so;
}

/// failover-example (PLAN-extension-api.md E2): the shipped example
/// override of `failover.decide` (examples/plugins/failover-backoff),
/// copied into a scratch plugins dir and loaded by the real JS host,
/// answers the point the way its comments say; the host's unload gives
/// the decision back to core's "policy".
async fn failover_example_case() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();
    // The example as a user installs it: the directory, copied.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let example = repo.join("examples").join("plugins/failover-backoff");
    let plugin_dir = veles_core::plugins::plugins_dir().join("failover-backoff");
    assert!(
        plugin_dir.starts_with(home.path()),
        "{}",
        plugin_dir.display()
    );
    std::fs::create_dir_all(&plugin_dir).unwrap();
    for f in ["plugin.toml", "plugin.ts"] {
        std::fs::copy(example.join(f), plugin_dir.join(f))
            .unwrap_or_else(|e| panic!("copy {f} from {}: {e}", example.display()));
    }
    let root = home.path().join("modules");
    let dir = root.join("lang-js");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &lang_js_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lang-js\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();
    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = ModuleHost::new(bus, "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(started, vec!["lang-js".to_string()], "started {started:?}");
    wait_for(&host, "lang-js", "running", Duration::from_secs(20)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !veles_core::scripthost::rows()
        .iter()
        .any(|r| r.name == "failover-backoff" && r.state == "loaded")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the example never loaded: {:?}",
            veles_core::scripthost::rows()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(veles_core::services::overridden(
        veles_core::failover::POINT
    ));

    let input = |kind: &str, attempt: u32, left: &[&str]| {
        serde_json::json!({
            "session_id": "s",
            "interactive": false,
            "attempt": attempt,
            "hops": 0,
            "route": {"provider": "primary", "model": "m"},
            "error": {"kind": kind, "status": null, "message": "x"},
            "core_would_fall_back": true,
            "remaining_routes": left
                .iter()
                .map(|p| serde_json::json!({"provider": p, "model": "m2"}))
                .collect::<Vec<_>>(),
            "config": {"mode": "stop", "source": "implicit", "max_hops": null, "script": null},
        })
    };
    let ask = |v: serde_json::Value| async move {
        veles_core::services::ask(veles_core::failover::POINT, v, None)
            .await
            .expect("the point answers")
    };
    let v = ask(input("rate_limited", 0, &["second"])).await;
    assert_eq!(
        (v["action"].as_str(), v["seconds"].as_u64()),
        (Some("wait"), Some(30)),
        "{v}"
    );
    let v = ask(input("rate_limited", 1, &["second"])).await;
    assert_eq!(
        (v["action"].as_str(), v["seconds"].as_u64()),
        (Some("wait"), Some(60)),
        "{v}"
    );
    let v = ask(input("rate_limited", 2, &["second"])).await;
    assert_eq!(
        (
            v["action"].as_str(),
            v["provider"].as_str(),
            v["model"].as_str()
        ),
        (Some("goto"), Some("second"), Some("m2")),
        "{v}"
    );
    let v = ask(input("rate_limited", 2, &[])).await;
    assert_eq!(v["action"], "stop", "{v}");
    let v = ask(input("server", 0, &["second"])).await;
    assert_eq!(v["action"], "advance", "{v}");
    let v = ask(input("client", 0, &["second"])).await;
    assert_eq!(
        v["action"], "policy",
        "the rest goes to `next`: core's answer: {v}"
    );

    let out = host.unload("lang-js").await.expect("unload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(!veles_core::services::overridden(
        veles_core::failover::POINT
    ));
    let v = ask(input("rate_limited", 0, &["second"])).await;
    assert_eq!(v["action"], "policy", "core decides again: {v}");
    assert!(host.status().is_empty(), "{:?}", host.status());
    let _ = so;
}

async fn stray_spawn() {
    let (host, _home, _dir, _so) = rig("").await;
    let out = host.stop("echo-module").await.expect("stop");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    let s = host
        .status()
        .into_iter()
        .find(|s| s.plugin == "echo-module")
        .expect("status row");
    assert_eq!(s.state, "stopped", "{s:?}");
}

/// spin-keeps-mapped: a module task that never yields keeps its worker
/// thread past the runtime's shutdown, still running module code. The
/// host must say so and keep the library mapped (the quiescence check's
/// thread leg read 0 by construction until 2026-10-09: it counted the
/// runtime it had already handed to the shutdown thread).
async fn spin_keeps_mapped() {
    std::env::set_var("VELES_ECHO_SPIN", "1");
    let (host, _home, _dir, so) = rig("").await;
    std::env::remove_var("VELES_ECHO_SPIN");
    let so = so.canonicalize().unwrap();
    let out = host.stop("echo-module").await.expect("stop");
    assert!(
        out.iter()
            .any(|l| l.contains("code kept mapped") && l.contains("thread(s) still running")),
        "{out:?}"
    );
    assert!(
        ModuleHost::so_mapped(&so),
        "unmapped under a running thread"
    );
}

/// quit-cut-short: the gateway's stop waits a bounded time for each
/// instance's Quit, then drops its runtime, and with it a teardown still
/// waiting on the instance runtime's shutdown. That drop must not unmap
/// the library. On 2026-10-12 it dlclosed lang-lisp and ECL under the
/// live engine thread, and the gateway died with SIGSEGV on its way out.
/// Here the echo module keeps a blocking thread like that engine's, the
/// Quit gets 1 s against the host's 3 s shutdown, and the runtime that
/// ran the supervisor is dropped with the teardown pending, as the
/// gateway's main drops its own.
async fn quit_cut_short() {
    let (so, home) = tokio::task::spawn_blocking(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (host, home, so) = rt.block_on(async {
            std::env::set_var("VELES_ECHO_BLOCKING", "1");
            let (host, home, _dir, so) = rig("").await;
            std::env::remove_var("VELES_ECHO_BLOCKING");
            host.shutdown_all(Duration::from_secs(1)).await;
            (host, home, so)
        });
        rt.shutdown_timeout(Duration::from_secs(1));
        drop(host);
        (so.canonicalize().unwrap(), home)
    })
    .await
    .expect("the cut-short runtime");
    // Long enough for the blocking thread to wake into the module's code
    // many times over, which faulted while the library was unmapped.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        ModuleHost::so_mapped(&so),
        "a dropped teardown unmapped the library under a running thread"
    );
    drop(home);
}

/// standby-quit: the gateway's stop does not wait on an instance that
/// stands by for its lease. Nothing supervises it until the lease is
/// granted, so nothing answered its Quit, and every stop waited out the
/// whole bound. On the laptop that was Telegram, standing by for the
/// VPS's lease; it hid while the bound equalled the pinned engines' own
/// 10 s (2026-10-12).
async fn standby_quit() {
    std::env::set_var("VELES_ECHO_LEASE", "1");
    let (host, _home, _dir, _so) = rig_gated("", Arc::new(NeverGranted), "standby").await;
    std::env::remove_var("VELES_ECHO_LEASE");
    let began = std::time::Instant::now();
    host.shutdown_all(Duration::from_secs(5)).await;
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "the stop waited {:?} on an instance standing by",
        began.elapsed()
    );
}

/// stale-call: a reload of a stopped module says so, and a start brings
/// it back; a stopped module's gate is closed to new work. The module
/// is STOPPED at the end (not left running): a running instance's
/// supervisor holds its runtime, and the process exit would tear the
/// core runtime down with the module's tasks still queued — the segfault
/// the whole teardown ordering exists to prevent.
async fn stale_call() {
    let (host, _home, _dir, _so) = rig("").await;
    host.stop("echo-module").await.expect("stop");
    let out = host.reload("echo-module").await.expect("reload");
    assert!(
        out.iter().any(|l| l.contains("stopped")),
        "a stopped module's reload says so: {out:?}"
    );
    host.start_again("echo-module").await.expect("start");
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;
    host.stop("echo-module").await.expect("stop again");
}

/// fault-restart (2026-10-10): a module panic restarts the instance
/// ONCE, and a panic that comes after an instance has ended is not held
/// against the next one. The panics are real, through the M5 hook, on a
/// thread tagged with the instance's owner and located here, outside
/// core. Before the fix the fault registry was never drained: every new
/// instance was ended at its first poll, with a doubling backoff, until
/// the gateway restarted (the VPS telegram module, after a reload).
async fn fault_restart() {
    veles_core::lifecycle::install_fatal_panic_hook();
    let (host, _home, _dir, _so) = rig("").await;
    let owner = "module:echo-module";
    let status = |host: &ModuleHost| {
        host.status()
            .into_iter()
            .find(|s| s.plugin == "echo-module")
            .expect("listed")
    };
    // Six polls of the supervisor (it polls the registry every 500 ms).
    let holds = |host: &ModuleHost, restarts: u32| {
        let host = host.clone();
        async move {
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let st = status(&host);
                assert_eq!(st.state, "running", "{st:?}");
                assert_eq!(st.restarts, restarts, "restarted again: {st:?}");
            }
        }
    };

    // 1. A panic while the instance runs: ended, one restart, and the
    //    new instance stays up.
    panic_as(owner);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while status(&host).restarts == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the panic never ended the instance: {:?}",
            status(&host)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;
    holds(&host, 1).await;
    assert!(
        !veles_core::module::faulted_instances().contains_key(owner),
        "the fault the supervisor acted on is still recorded"
    );

    // 2. A panic after the instance ended (the old instance's threads,
    //    late): the next instance runs, says so, and is not restarted.
    host.stop("echo-module").await.expect("stop");
    panic_as(owner);
    host.start_again("echo-module").await.expect("start");
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;
    holds(&host, 1).await;
    let st = status(&host);
    assert!(
        st.last_error
            .as_deref()
            .is_some_and(|e| e.contains("previous instance panicked")),
        "the late panic is not shown: {st:?}"
    );
    assert!(!veles_core::module::faulted_instances().contains_key(owner));
    // Stopped here, as every case does: a module still running when the
    // harness's runtime drops crashes the process (SIGSEGV), before and
    // after this fix (PENDING).
    host.stop("echo-module").await.expect("stop");
}

/// Panic on a fresh thread tagged `owner`: the M5 hook marks the fault
/// and the thread unwinds; the process lives.
fn panic_as(owner: &'static str) {
    let t = std::thread::spawn(move || {
        veles_core::lifecycle::set_thread_owner(owner);
        panic!("fault-restart: a deliberate panic on a module thread");
    });
    assert!(t.join().is_err(), "the thread did not panic");
    assert!(
        veles_core::module::faulted_instances().contains_key(owner),
        "the hook did not mark {owner}"
    );
}

/// key-mismatch: a module built against another core is refused from
/// the file, before any of its code runs.
async fn key_mismatch() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();

    let root = home.path().join("modules");
    let dir = root.join("echo-module");
    std::fs::create_dir_all(&dir).unwrap();
    let key = veles_core::module::BUILD_KEY.as_bytes();
    let mut bytes = std::fs::read(echo_so()).unwrap();
    let mut other = key.to_vec();
    other[0] = if other[0] == b'0' { b'1' } else { b'0' };
    let mut i = 0;
    let mut hits = 0;
    while i + key.len() <= bytes.len() {
        if &bytes[i..i + key.len()] == key {
            bytes[i..i + key.len()].copy_from_slice(&other);
            hits += 1;
            i += key.len();
        } else {
            i += 1;
        }
    }
    assert!(hits > 0, "nothing to patch");
    std::fs::write(dir.join("module.so"), bytes).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"echo-module\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = ModuleHost::new(bus, "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert!(started.is_empty(), "a mismatched module must not start");
    let refused = host.status().into_iter().find(|s| s.state == "refused");
    let r = refused.expect("listed as refused");
    let why = r.last_error.unwrap_or_default();
    assert!(
        why.contains("core changed"),
        "the refusal names the key mismatch: {why}"
    );
}

/// rescan: a module installed after the host started is picked up
/// without a restart.
async fn rescan() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    let cfg_dir = home.path().join("config/veles");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();

    let root = home.path().join("modules");
    std::fs::create_dir_all(&root).unwrap();

    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = ModuleHost::new(bus, "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert!(started.is_empty(), "nothing installed yet");

    let dir = root.join("echo-module");
    std::fs::create_dir_all(&dir).unwrap();
    modulehost::install_file(&dir, &echo_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"echo-module\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();
    let started = host.rescan().await;
    assert_eq!(started, vec!["echo-module".to_string()], "{started:?}");
    wait_for(&host, "echo-module", "running", Duration::from_secs(10)).await;

    let again = host.rescan().await;
    assert!(again.is_empty(), "already supervised: {again:?}");
    // Same exit posture as stale-call: leave nothing running.
    host.stop("echo-module").await.expect("stop");
}

/// irc-module (R4): the REAL IRC dylib against a stub server, through
/// the module host — the whole phase's proof in one case. The module
/// connects and registers (001), claims its bus route, registers the
/// `irc` tool; a reload keeps the tool (the old `Once` is gone) and its
/// QUIT says "reload"; a stop's QUIT says "stop", it unregisters the
/// handle, and the library stays mapped for its tracing callsites. Both
/// stops return on their own, inside their deadline. What this pins
/// that the echo cases cannot: a REAL platform's async work runs on the
/// instance runtime and is dropped by its `shutdown_timeout`. The stub
/// is plain TCP; `irc-live` is the TLS run against a real server.
async fn irc_module() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    std::env::set_var("VELES_HOME", home.path());

    // The stub IRC server: answer the capability probe and the
    // registration, then hold the connection open (the client stays
    // registered until we close or it QUITs). The connection count and
    // the QUIT it receives are the assertions' evidence.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let quits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let conns = conns.clone();
        let quits = quits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let quits = quits.clone();
                tokio::spawn(async move {
                    let (rd, mut wr) = sock.into_split();
                    let mut lines = BufReader::new(rd).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        eprintln!("STUB SAW: {line}");
                        if line.starts_with("CAP LS") {
                            let _ = wr.write_all(b":stub CAP * LS :\r\n").await;
                        } else if line.starts_with("CAP REQ") {
                            let want = line
                                .trim_start_matches("CAP REQ")
                                .trim_start()
                                .trim_start_matches(':')
                                .trim();
                            let _ = wr
                                .write_all(format!(":stub CAP * ACK :{want}\r\n").as_bytes())
                                .await;
                        } else if line.starts_with("CAP END") || line.starts_with("USER ") {
                            let _ = wr.write_all(b":stub 001 velestest :Welcome\r\n").await;
                        } else if line.starts_with("QUIT") {
                            quits.lock().unwrap().push(line.clone());
                            break;
                        }
                    }
                });
            }
        });
    }

    // The module's config: one network at the stub. `instances_of`
    // reads `platforms.irc.networks` from the raw config JSON, so the
    // TOML carries the array; the instance is named `stubnet`.
    // VELES_HOME forces the single-directory layout: config.toml sits
    // at its ROOT, not under config/veles.
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[platforms.irc]\nenabled = true\n\
             [[platforms.irc.networks]]\nname = \"stubnet\"\nserver = \"127.0.0.1\"\n\
             port = {port}\nuse_tls = false\nnickname = \"velestest\"\nusername = \"velestest\"\n"
        ),
    )
    .unwrap();

    let root = home.path().join("modules");
    let dir = root.join("irc");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &irc_so()).expect("install");
    let described = modulehost::describe(&so).expect("describe the real IRC dylib");
    assert_eq!(described.knobs, veles_irc::config_surface());
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"irc\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    let bus = test_bus(home.path());
    let host = ModuleHost::new(bus.clone(), "node-under-test", fast());
    // Same race discipline as `rig`: a `reloading` answer means the
    // supervisor was mid-teardown from a previous ctl — retry once the
    // new instance is up.
    async fn stop_when_idle(host: &ModuleHost, target: &str) -> Vec<String> {
        for _ in 0..20 {
            let out = host.stop(target).await.expect("stop");
            if !out.iter().any(|l| l.contains("reloading")) {
                return out;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        host.stop(target).await.expect("stop")
    }
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(
        started,
        vec!["irc:stubnet".to_string()],
        "started {started:?}"
    );
    wait_for(&host, "irc:stubnet", "running", Duration::from_secs(10)).await;

    // Connected and registered (the stub answered 001).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while conns.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "never connected");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        bus.outbound_channel("irc", Some("stubnet")).is_some(),
        "the module claimed its irc/stubnet route"
    );
    assert!(
        veles_core::tools::global_tool_names()
            .iter()
            .any(|n| n == "irc"),
        "the irc tool is registered"
    );
    assert!(
        veles_core::services::is_point("irc.addressed"),
        "the module declared its point (PLAN-extension-api.md E4)"
    );

    // A reload: the tool survives (the old `Once` would have lost it),
    // and the instance is running again on the new load. Its stop came
    // back on its own (the QUIT flush ended at the stub's close), and the
    // stub saw the reason.
    let out = host.reload("irc:stubnet").await.expect("reload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        !out.iter().any(|l| l.contains("did not return")),
        "the module's stop ran into its deadline: {out:?}"
    );
    assert_eq!(
        quits.lock().unwrap().first().map(String::as_str),
        Some("QUIT reload"),
        "the reload's QUIT carries its reason"
    );
    wait_for(&host, "irc:stubnet", "running", Duration::from_secs(10)).await;
    assert!(
        veles_core::tools::global_tool_names()
            .iter()
            .any(|n| n == "irc"),
        "the irc tool did not come back after the reload (the Once bug)"
    );
    // The old load stays mapped by its tracing callsites, which is no
    // fault: `plugins status` must not show it as the instance's last
    // error (it did after every reload: "last error: 11 tracing
    // callsite(s) …" on a healthy instance).
    let st = host
        .status()
        .into_iter()
        .find(|s| s.plugin == "irc" && s.instance == "stubnet")
        .expect("irc:stubnet listed");
    assert_eq!(
        st.last_error, None,
        "a callsite-pinned reload reported as an error"
    );

    // A stop: QUIT went out, the handle is gone, and the library stays
    // MAPPED: veles-irc logs through its own `tracing` macros, and
    // tracing-core never forgets a callsite (R1 hazard 1).
    let so = so.canonicalize().unwrap();
    let out = stop_when_idle(&host, "irc:stubnet").await;
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        !out.iter().any(|l| l.contains("did not return")),
        "the module's stop ran into its deadline: {out:?}"
    );
    // The branch this asserts exists only when the module's `info!`
    // sites REGISTERED with this process's tracing-core: a level that
    // filters them earlier (RUST_LOG=off) never registers them, and a
    // clean unload is then right. The flake's sandbox has failed here
    // since db62aa83 while every local run passes (2026-10-13): the
    // message says what the sandbox saw, so the next red run names the
    // cause instead of the symptom.
    let kept_by_callsites = out
        .iter()
        .any(|l| l.contains("code kept mapped") && l.contains("tracing callsite"));
    if !kept_by_callsites {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let irc_lines: Vec<&str> = maps
            .lines()
            .filter(|l| {
                l.contains("veles_module_irc") || l.ends_with(so.to_string_lossy().as_ref())
            })
            .collect();
        panic!(
            "the module's own callsites must keep it mapped: {out:?}\n\
             level filter: {}, callsites recorded in this process: {}, module path: {}, \
             maps lines naming it: {irc_lines:?}",
            tracing::level_filters::LevelFilter::current(),
            veles_core::module::callsites_recorded(),
            so.display()
        );
    }
    assert!(
        !veles_core::tools::global_tool_names()
            .iter()
            .any(|n| n == "irc"),
        "the tool survived the teardown purge"
    );
    assert!(
        !veles_core::services::is_point("irc.addressed"),
        "the point survived the teardown purge"
    );
    // The instance's presence is gone from the tap's send registry:
    // a stopped network must not be sendable (a reloaded one
    // re-registers). The tap lives in SHARED core, so this reads the
    // module's registration — pinned against the mutant whose `stop`
    // leaves the registration behind. (The module's own HANDLES map is
    // a separate copy in its mapping, not visible to this harness.)
    assert!(
        !veles_core::irc_tap::sendable().iter().any(|p| p == "bot"),
        "the bot presence survived stop"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while quits.lock().unwrap().len() < 2 {
        assert!(std::time::Instant::now() < deadline, "no QUIT on the wire");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        quits.lock().unwrap().get(1).map(String::as_str),
        Some("QUIT stop"),
        "the stop's QUIT carries its reason"
    );
    assert!(
        ModuleHost::so_mapped(&so),
        "a kept-mapped module was unmapped"
    );
}

/// irc-live — manual, never in the gate: the IRC module against a REAL
/// server, which the stub cannot stand in for (TLS, and a server that
/// answers QUIT with ERROR and closes). It reloads, then stops the
/// instance, with the gateway's own timings, and prints how long each
/// teardown took and every instance thread that outlived it, with the
/// CPU it is burning. Against the local ergo:
///
///   VELES_TEST_IRC=127.0.0.1:6697 VELES_TEST_IRC_CA=/etc/ergo/server.crt \
///     veles module-host-test irc-live
///
/// `VELES_TEST_IRC_TLS=0` for a plain port; `VELES_TEST_IRC_NICK` and
/// `VELES_TEST_IRC_CHANNEL` default to `velesquit` and `#veles-quit`.
async fn irc_live() {
    let addr = std::env::var("VELES_TEST_IRC").expect("VELES_TEST_IRC=host:port");
    let (server, port) = addr.rsplit_once(':').expect("VELES_TEST_IRC=host:port");
    let tls = std::env::var("VELES_TEST_IRC_TLS").map_or(true, |v| v != "0");
    let ca = std::env::var("VELES_TEST_IRC_CA")
        .map(|p| format!("tls_ca_cert = \"{p}\"\n"))
        .unwrap_or_default();
    let nick = std::env::var("VELES_TEST_IRC_NICK").unwrap_or_else(|_| "velesquit".into());
    let chan = std::env::var("VELES_TEST_IRC_CHANNEL").unwrap_or_else(|_| "#veles-quit".into());

    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    std::env::set_var("VELES_HOME", home.path());
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[platforms.irc]\nenabled = true\n\
             [[platforms.irc.networks]]\nname = \"live\"\nserver = \"{server}\"\n\
             port = {port}\nuse_tls = {tls}\n{ca}nickname = \"{nick}\"\n\
             username = \"{nick}\"\nchannels = \"{chan}\"\n"
        ),
    )
    .unwrap();
    let root = home.path().join("modules");
    let dir = root.join("irc");
    std::fs::create_dir_all(&dir).unwrap();
    modulehost::install_file(&dir, &irc_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"irc\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    let host = ModuleHost::new(
        test_bus(home.path()),
        "node-under-test",
        modulehost::HostOptions::default(),
    );
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(started, vec!["irc:live".to_string()], "started {started:?}");
    // Long enough to register and join on a local server.
    let settle = Duration::from_secs(5);
    wait_for(&host, "irc:live", "running", Duration::from_secs(10)).await;
    tokio::time::sleep(settle).await;

    let t0 = std::time::Instant::now();
    let out = host.reload("irc:live").await.expect("reload");
    println!("LIVE reload took {:?}: {out:?}", t0.elapsed());
    println!(
        "LIVE threads after reload: {:?}",
        busy_threads("module:irc:")
    );
    wait_for(&host, "irc:live", "running", Duration::from_secs(10)).await;
    tokio::time::sleep(settle).await;

    let t0 = std::time::Instant::now();
    let out = host.stop("irc:live").await.expect("stop");
    println!("LIVE stop took {:?}: {out:?}", t0.elapsed());
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("LIVE threads after stop: {:?}", busy_threads("module:irc:"));
}

/// This process's threads whose name starts with `prefix`, each with
/// the CPU it used over the next 500 ms (ms of user+system time). A
/// thread an instance's teardown should have ended, still burning CPU,
/// is a task spinning on module code.
fn busy_threads(prefix: &str) -> Vec<(String, u64)> {
    fn cpu_ticks(tid: &str) -> u64 {
        let stat =
            std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).unwrap_or_default();
        // Fields after the `(comm)`: state is 3rd, utime 14th, stime 15th.
        let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
        let f: Vec<&str> = rest.split_whitespace().collect();
        let n = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        n(11) + n(12)
    }
    let tids: Vec<String> = std::fs::read_dir("/proc/self/task")
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|tid| {
                    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
                        .is_ok_and(|c| c.starts_with(prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    let before: Vec<u64> = tids.iter().map(|t| cpu_ticks(t)).collect();
    std::thread::sleep(Duration::from_millis(500));
    // A clock tick is 10 ms (USER_HZ 100).
    tids.iter()
        .zip(before)
        .map(|(t, b)| (t.clone(), cpu_ticks(t).saturating_sub(b) * 10))
        .collect()
}

/// atproto-cmd-module: the REAL /atproto module registers its direct
/// command on start and the host's purge removes it on stop; with the
/// atproto plugin's repo services stubbed, `/atproto delete` runs its
/// verify-first flow through the module's own code.
async fn atproto_cmd_module() {
    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    std::env::set_var("VELES_HOME", home.path());
    std::fs::write(home.path().join("config.toml"), "").unwrap();

    let root = home.path().join("modules");
    let dir = root.join("atproto-cmd");
    std::fs::create_dir_all(&dir).unwrap();
    modulehost::install_file(&dir, &atproto_cmd_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"atproto-cmd\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    assert!(
        veles_core::commands::direct_command("atproto").is_none(),
        "no /atproto before the module"
    );

    let host = ModuleHost::new(test_bus(home.path()), "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(
        started,
        vec!["atproto-cmd".to_string()],
        "started {started:?}"
    );
    wait_for(&host, "atproto-cmd", "running", Duration::from_secs(10)).await;

    let cmd = veles_core::commands::direct_command("atproto").expect("/atproto registered");

    // The repo services the command calls, stubbed (the atproto plugin
    // is not in this process; the command finds services by name,
    // never by provider).
    let deleted = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let d = deleted.clone();
    let decl = |name: &str| veles_core::services::ServiceDecl {
        name: name.into(),
        description: String::new(),
        cap: Some(veles_core::services::SERVICE_CAP_ANY.into()),
    };
    veles_core::services::register(
        decl(veles_types::plugin::method::REPO_WHOAMI),
        veles_core::services::handler(|_p, _cx| async {
            Ok(serde_json::json!({"did": "did:plc:bot", "handle": "bot.example"}))
        }),
    )
    .expect("whoami stub");
    veles_core::services::register(
        decl(veles_types::plugin::method::REPO_GET_RECORD),
        veles_core::services::handler(|_p, _cx| async {
            Ok(serde_json::json!({"found": true, "value": {}}))
        }),
    )
    .expect("get_record stub");
    veles_core::services::register(
        decl(veles_types::plugin::method::REPO_DELETE_RECORD),
        veles_core::services::handler(move |p, _cx| {
            let d = d.clone();
            async move {
                d.lock().unwrap().push((
                    p["collection"].as_str().unwrap_or("").to_string(),
                    p["rkey"].as_str().unwrap_or("").to_string(),
                ));
                Ok(serde_json::Value::Null)
            }
        }),
    )
    .expect("delete_record stub");

    let out = veles_core::commands::run_direct(
        "atproto",
        &cmd,
        veles_core::commands::DirectCall {
            args: "delete https://impro.social/profile/bot.example/post/3kabcde".into(),
            principal: None,
            sender: None,
            session_id: String::new(),
        },
    )
    .await;
    assert!(out.contains("deleted at://did:plc:bot/"), "{out}");
    assert_eq!(
        deleted.lock().unwrap().as_slice(),
        &[("app.bsky.feed.post".to_string(), "3kabcde".to_string())]
    );

    // A foreign record is refused by the module before the delete stub
    // is touched.
    let out = veles_core::commands::run_direct(
        "atproto",
        &cmd,
        veles_core::commands::DirectCall {
            args: "delete at://did:plc:someoneelse/app.bsky.feed.post/3kabcde".into(),
            principal: None,
            sender: None,
            session_id: String::new(),
        },
    )
    .await;
    assert!(out.contains("not the bot's"), "{out}");
    assert_eq!(deleted.lock().unwrap().len(), 1, "still only the one");

    // The clone above holds the module's run closure: drop it BEFORE
    // the stop unmaps the library, or its drop glue runs against an
    // unmapped module (the harness's own held copy, not a registry
    // entry — the purge takes the registry's).
    drop(cmd);

    // Stopped: the command is gone with the module.
    let out = host.stop("atproto-cmd").await.expect("stop");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        veles_core::commands::direct_command("atproto").is_none(),
        "/atproto purged with the module"
    );
}

/// slopdrop-module (R5): the REAL slopdrop module fills core's seams on
/// start and the host's purge empties them on stop, so `tcl` goes to
/// the bridge while the module runs and back to the local sandbox when
/// it is gone; the linked version's data tree moves once, with a backup.
async fn slopdrop_module() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    std::env::set_var("VELES_HOME", home.path());

    // A stub slopdrop: every eval answers "42 from the bridge".
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let evals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let evals = evals.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let evals = evals.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let body = if req.starts_with("POST /api/eval") {
                        evals.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        r#"{"output":["42 from the bridge"],"is_error":false}"#
                    } else {
                        r#"{"bindings":[],"disabled":[],"fired":[]}"#
                    };
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
    }

    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[slopdrop]\nenabled = true\nurl = \"http://127.0.0.1:{port}\"\ntimeout_ms = 2000\n"
        ),
    )
    .unwrap();
    // The linked version's data tree, to be migrated once.
    let legacy = veles_core::config::data_dir().join("slopdrop");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("withheld.json"), "[]").unwrap();

    let root = home.path().join("modules");
    let dir = root.join("slopdrop");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &slopdrop_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"slopdrop\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    assert!(
        !veles_core::tcleval::backend_configured(),
        "no backend before the module"
    );
    assert!(veles_core::tcleval::public_reply().is_none());
    assert!(!veles_core::triggers::forwarder_configured());
    assert!(veles_core::commands::direct_command("slopdrop").is_none());

    let host = ModuleHost::new(test_bus(home.path()), "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert_eq!(started, vec!["slopdrop".to_string()], "started {started:?}");
    wait_for(&host, "slopdrop", "running", Duration::from_secs(10)).await;

    // The seams are filled, by the module's owner.
    assert!(
        veles_core::tcleval::backend_configured(),
        "the backend slot"
    );
    assert!(
        veles_core::tcleval::public_reply().is_some(),
        "the public policy"
    );
    assert!(
        veles_core::triggers::forwarder_configured(),
        "the forwarder"
    );
    assert!(
        veles_core::commands::direct_command("slopdrop").is_some(),
        "/slopdrop"
    );
    assert_eq!(
        veles_core::moduleroutes::served(),
        vec!["slopdrop".to_string()]
    );
    assert_eq!(
        veles_core::acl::tool_cap("tcl_state_eval"),
        Some(veles_core::acl::Cap::Shell)
    );
    let out = veles_core::tcleval::eval_line("expr {6 * 7}", false, "user:test", None).await;
    assert!(
        out.contains("42 from the bridge"),
        "tcl went to the bridge: {out}"
    );
    assert_eq!(evals.load(std::sync::atomic::Ordering::SeqCst), 1);
    let status = veles_core::moduleroutes::dispatch(
        "slopdrop",
        veles_core::moduleroutes::RouteRequest {
            method: "GET".into(),
            path: "/status".into(),
            ..Default::default()
        },
    )
    .await
    .expect("the module serves its routes");
    assert_eq!(status.status, 200, "{:?}", status.body);
    // The data moved once, with a backup.
    let state = veles_core::config::data_dir()
        .join("module-state")
        .join("slopdrop");
    assert!(
        state.join("withheld.json").exists(),
        "migrated into the module's state dir"
    );
    assert!(state.join(".migrated-from").exists());
    assert!(
        !legacy.exists() && legacy.with_file_name("slopdrop.pre-module").exists(),
        "the backup"
    );

    // A reload keeps every seam filled.
    let out = host.reload("slopdrop").await.expect("reload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    wait_for(&host, "slopdrop", "running", Duration::from_secs(10)).await;
    assert!(
        veles_core::tcleval::backend_configured(),
        "re-filled after a reload"
    );
    assert!(veles_core::commands::direct_command("slopdrop").is_some());

    // Stopped: every seam is core's own again.
    let out = host.stop("slopdrop").await.expect("stop");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        !veles_core::tcleval::backend_configured(),
        "the slot is empty"
    );
    assert!(
        veles_core::tcleval::public_reply().is_none(),
        "public /tcl fails closed"
    );
    assert!(!veles_core::triggers::forwarder_configured());
    assert!(veles_core::commands::direct_command("slopdrop").is_none());
    assert!(veles_core::moduleroutes::served().is_empty());
    assert_eq!(veles_core::acl::tool_cap("tcl_state_eval"), None);
    let out = veles_core::tcleval::eval_line("puts [expr {6 * 7}]", false, "user:test", None).await;
    assert!(
        out.contains("42") && !out.contains("bridge"),
        "the local sandbox again: {out}"
    );
    assert_eq!(
        evals.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no bridge call after the stop"
    );
    let st = host
        .status()
        .into_iter()
        .find(|s| s.plugin == "slopdrop")
        .expect("listed");
    // The module logs through its own `tracing` macros in the moved
    // files, so it is kept mapped (pinned by its callsites), never a
    // fault: `stopped`, or `kept-mapped` with no last error.
    assert!(
        st.state == "stopped" || (st.state == "kept-mapped" && st.last_error.is_none()),
        "{st:?}"
    );
    if !ModuleHost::so_mapped(&so) {
        println!("the module unmapped cleanly");
    }
}

/// telegram-module (R6): the REAL Telegram module against a stub Bot API
/// (`TELOXIDE_API_URL`): it reaches `running`, claims the `telegram`
/// route on the bus, answers a reload (the route claimed again), and a
/// stop ENDS the long poll through the dispatcher's shutdown token
/// within the deadline (no "stop did not return", no thread left) —
/// and with no `TELEGRAM_BOT_TOKEN` on the node the host spawns no
/// instance at all (the per-node switch). A reload that lands before
/// the poll runs (2026-10-10) ends the loop before the teardown, so no
/// panic outlives it.
async fn telegram_module() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The production hook: a module-thread panic is recorded, not lost.
    veles_core::lifecycle::install_fatal_panic_hook();

    let home = tempfile::tempdir().expect("scratch home");
    std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
    std::env::set_var("VELES_HOME", home.path());
    std::env::remove_var("TELEGRAM_BOT_TOKEN");

    // The stub Bot API: getMe answers a bot after 300 ms (a window for
    // a stop that comes before the poll, step 3b); getUpdates long-polls
    // for a second and answers nothing; everything else is `ok`.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let getmes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let polls = polls.clone();
        let getmes = getmes.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let polls = polls.clone();
                let getmes = getmes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    // teloxide spells the method as the request line's
                    // last segment (`/bot<token>/GetMe`); match on the
                    // lower-cased line.
                    let path = req.lines().next().unwrap_or("").to_ascii_lowercase();
                    let body = if path.contains("/getme") {
                        getmes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        r#"{"ok":true,"result":{"id":4242,"is_bot":true,"first_name":"stub","username":"velesstubbot","can_join_groups":true,"can_read_all_group_messages":false,"supports_inline_queries":false}}"#.to_string()
                    } else if path.contains("/getupdates") {
                        polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(800)).await;
                        r#"{"ok":true,"result":[]}"#.to_string()
                    } else {
                        r#"{"ok":true,"result":true}"#.to_string()
                    };
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
    }
    std::env::set_var("TELOXIDE_API_URL", format!("http://127.0.0.1:{port}/"));

    std::fs::write(
        home.path().join("config.toml"),
        "[platforms.telegram]\nenabled = true\n[platforms.telegram.extra]\nrequire_mention = true\n",
    )
    .unwrap();
    let root = home.path().join("modules");
    let dir = root.join("telegram");
    std::fs::create_dir_all(&dir).unwrap();
    let so = modulehost::install_file(&dir, &telegram_so()).expect("install");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"telegram\"\nkind = \"module\"\nentry = \"module.so\"\n",
    )
    .unwrap();

    // 1. No token on this node: the host spawns NO instance (the per-node
    //    switch), listed `off` with the reason, and asks for no lease.
    let bus = test_bus(home.path());
    let host = ModuleHost::new(bus.clone(), "node-under-test", fast());
    let started = host
        .start(
            std::slice::from_ref(&root),
            &[],
            &[],
            veles_core::pluginhost::raw_config_json(),
            Arc::new(Ungated),
        )
        .await;
    assert!(started.is_empty(), "no token, no instance: {started:?}");
    let off = host
        .status()
        .into_iter()
        .find(|s| s.plugin == "telegram")
        .expect("listed off");
    assert_eq!(off.state, "off", "{off:?}");
    assert!(
        off.last_error
            .as_deref()
            .unwrap_or("")
            .contains("TELEGRAM_BOT_TOKEN"),
        "{off:?}"
    );

    // 2. With a token: running, the route claimed, polling the stub.
    std::env::set_var("TELEGRAM_BOT_TOKEN", "4242:stub-token");
    let started = host.rescan().await;
    assert_eq!(started, vec!["telegram".to_string()], "started {started:?}");
    wait_for(&host, "telegram", "running", Duration::from_secs(15)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while polls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "never polled the stub"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        bus.outbound.lock().unwrap().contains_key("telegram"),
        "the telegram route is claimed"
    );
    assert!(
        veles_core::services::is_point("telegram.addressed"),
        "the module declared its point (PLAN-extension-api.md E4)"
    );

    // 3. A reload: the route is claimed again by the new load; the stop
    //    ended the long poll within the deadline (the answer would say
    //    otherwise).
    let getmes_before = getmes.load(std::sync::atomic::Ordering::SeqCst);
    let t0 = std::time::Instant::now();
    let out = host.reload("telegram").await.expect("reload");
    let took = t0.elapsed();
    assert!(
        out.iter()
            .any(|l| l.contains("unloaded") && !l.contains("did not return")),
        "{out:?}"
    );
    assert!(
        took < Duration::from_secs(8),
        "the stop waited out its deadline: {took:?}"
    );
    wait_for(&host, "telegram", "running", Duration::from_secs(15)).await;

    // 3b. A reload while the new instance's dispatcher is in ITS getMe:
    //     the token is handed out, but the dispatcher is idle until that
    //     call returns, and the token refuses (push-plugin's rescan +
    //     reload hit this on the VPS, 70 ms apart). The stop asks again
    //     until it takes and waits for the loop's end, so nothing of it
    //     panics in the runtime's shutdown, and the next instance holds.
    //     (The adapter's own getMe is the first call, the dispatcher's
    //     the second.)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while getmes.load(std::sync::atomic::Ordering::SeqCst) < getmes_before + 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the dispatcher never asked getMe"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let out = host.reload("telegram").await.expect("reload");
    assert!(
        out.iter()
            .any(|l| l.contains("unloaded") && !l.contains("did not return")),
        "{out:?}"
    );
    wait_for(&host, "telegram", "running", Duration::from_secs(15)).await;
    // The route is claimed again once the loop is online (after getMe).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !bus.outbound.lock().unwrap().contains_key("telegram") {
        assert!(
            std::time::Instant::now() < deadline,
            "the route was never claimed again"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let st = host
        .status()
        .into_iter()
        .find(|s| s.plugin == "telegram")
        .expect("listed");
    assert_eq!(st.state, "running", "{st:?}");
    assert_eq!(st.restarts, 0, "{st:?}");
    assert!(
        !st.last_error.as_deref().unwrap_or("").contains("panicked")
            && !veles_core::module::faulted_instances().contains_key("module:telegram"),
        "the stopped loop panicked in the runtime's shutdown: {st:?}"
    );

    // 4. Stopped: no thread of the instance survives, no fault.
    let before = polls.load(std::sync::atomic::Ordering::SeqCst);
    let out = host.stop("telegram").await.expect("stop");
    assert!(
        !veles_core::services::is_point("telegram.addressed"),
        "the point survived the teardown purge"
    );
    assert!(
        out.iter()
            .any(|l| l.contains("unloaded") && !l.contains("did not return")),
        "{out:?}"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let st = host
        .status()
        .into_iter()
        .find(|s| s.plugin == "telegram")
        .expect("listed");
    assert!(
        st.state == "stopped" || (st.state == "kept-mapped" && st.last_error.is_none()),
        "{st:?}"
    );
    assert!(
        busy_threads("module:telegram").is_empty(),
        "an instance thread survived the stop"
    );
    let after = polls.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        polls.load(std::sync::atomic::Ordering::SeqCst),
        after,
        "the long poll kept running after the stop (polled {before} → {after} → more)"
    );
    if !ModuleHost::so_mapped(&so) {
        println!("the module unmapped cleanly");
    }
    std::env::remove_var("TELEGRAM_BOT_TOKEN");
    std::env::remove_var("TELOXIDE_API_URL");
}

/// R11 selection/rollback, through the real echo dylib. This process
/// links the same runtime as the module; an integration-test rlib does
/// not. Appending inert bytes changes only the artifact digest, not its
/// ELF or manifest, which exercises a same-version payload replacement.
fn installer_module_lifecycle() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let bytes = std::fs::read(echo_so())?;
    let name = "echo-module";
    let source_root = |version: &str| temp.path().join(format!("source-{version}"));
    let paths = veles_runtime::veles_install::Paths::for_roots(
        temp.path().join("config"),
        temp.path().join("data"),
    );
    let mut original_manifest = None;
    for (version, suffix) in [("one", &b""[..]), ("two", &b"\0R11-test-payload\0"[..])] {
        let dir = source_root(version).join(name);
        std::fs::create_dir_all(&dir)?;
        let artifact = dir.join(format!("libveles_module_echo_{version}.so"));
        let mut changed = bytes.clone();
        changed.extend_from_slice(suffix);
        std::fs::write(&artifact, changed)?;
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                "name = \"{name}\"\nkind = \"module\"\nentry = \"{}\"\n",
                artifact.file_name().unwrap().to_string_lossy()
            ),
        )?;
        let mut selected_paths = paths.clone();
        selected_paths.module_roots = vec![source_root(version)];
        let service = veles_runtime::veles_install::InstallService::new(selected_paths);
        let plan = service.preview_component(name)?;
        let installed = service.install_component(name, plan["plan_sig"].as_str())?;
        let selected = modulehost::current_so(&paths.data_dir.join("modules").join(name))
            .context("no selected module")?;
        assert_eq!(
            modulehost::describe(&selected)
                .map_err(anyhow::Error::msg)?
                .name,
            name
        );
        let manifest = std::fs::read_to_string(
            paths
                .data_dir
                .join("modules")
                .join(name)
                .join("plugin.toml"),
        )?;
        assert!(
            manifest.contains(&format!(
                "entry = \"{}\"",
                selected.file_name().unwrap().to_string_lossy()
            )),
            "manifest names the selected content-addressed fallback: {manifest}"
        );
        let retained = installed["component"]["source"]
            .as_str()
            .map(std::path::PathBuf::from)
            .context("managed module payload path")?;
        assert_ne!(
            retained, selected,
            "the immutable payload and selection are separate paths"
        );
        assert_eq!(std::fs::read(&retained)?, std::fs::read(&selected)?);
        assert_eq!(service.verify()?["ok"], true);
        if version == "one" {
            original_manifest = Some(std::fs::read_to_string(
                paths
                    .data_dir
                    .join("modules")
                    .join(name)
                    .join("plugin.toml"),
            )?);
        }
    }
    let service = veles_runtime::veles_install::InstallService::new(paths.clone());
    let review = service.preview_rollback(name)?;
    service.rollback(
        name,
        review["plan_sig"].as_str().context("rollback plan_sig")?,
    )?;
    let selected = modulehost::current_so(&paths.data_dir.join("modules").join(name)).unwrap();
    assert_eq!(std::fs::read(&selected)?, bytes);
    // The old fallback is part of the selection, not merely a loadable
    // payload. Restoring a different manifest would alter discovery.
    assert_eq!(
        std::fs::read_to_string(
            paths
                .data_dir
                .join("modules")
                .join(name)
                .join("plugin.toml"),
        )?,
        original_manifest.context("original module manifest")?
    );
    assert_eq!(
        modulehost::describe(&selected)
            .map_err(anyhow::Error::msg)?
            .name,
        name
    );
    assert_eq!(service.verify()?["ok"], true);
    Ok(())
}

/// `veles module-host-test <case>`. Panics on failure (the exit code
/// carries it); prints `OK <case>` on success. Every instance is torn
/// down before the process exits, so no module-referencing task or
/// registration outlives the test into the runtime's own shutdown
/// (where a module's drop glue would run after its dlclose).
pub async fn run(case: &str) -> anyhow::Result<()> {
    match case {
        "installer-module-lifecycle" => installer_module_lifecycle()?,
        "load-unload" => load_unload().await,
        "conformance" => conformance_case().await,
        "conformance-js" => conformance_js_case().await,
        "failover-example" => failover_example_case().await,
        "conformance-racket" => conformance_racket_case().await,
        "conformance-lisp" => conformance_lisp_case().await,
        "stray-spawn" => stray_spawn().await,
        "spin-keeps-mapped" => spin_keeps_mapped().await,
        "quit-cut-short" => quit_cut_short().await,
        "standby-quit" => standby_quit().await,
        "stale-call" => stale_call().await,
        "fault-restart" => fault_restart().await,
        "key-mismatch" => key_mismatch().await,
        "rescan" => rescan().await,
        "irc-module" => irc_module().await,
        "slopdrop-module" => slopdrop_module().await,
        "atproto-cmd-module" => atproto_cmd_module().await,
        "telegram-module" => telegram_module().await,
        "signal-pin" => signal_pin().await,
        "irc-live" => irc_live().await,
        other => anyhow::bail!("unknown case `{other}`"),
    }
    println!("OK {case}");
    // Leave no instance running: a supervisor holding a module's runtime
    // past process shutdown is the segfault the harness would otherwise
    // prove (the production gateway's own shutdown_all does the same).
    if let Some(h) = modulehost::global() {
        h.shutdown_all(Duration::from_secs(5)).await;
    }
    Ok(())
}
