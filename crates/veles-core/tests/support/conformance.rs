//! The conformance suite (R8 / M8): the same scenarios against every
//! plugin tier that can hold tools, commands and hooks — the native
//! (ABI 4) tier in `tests/plugin_so.rs` and the module tier in
//! veles-cli's `modulehosttest.rs` (`veles module-host-test
//! conformance`). A [`Tier`] is what a rig implements: how its fixture
//! is loaded and unloaded, and what the fixture registers. The
//! scenarios are what both tiers must pass, word for word.
//!
//! Included by `#[path]` from both rigs: a file under `tests/support/`
//! is not an integration-test crate of its own.
#![allow(dead_code)]

// The including crate's view: in veles-core's own tests `veles_core`
// is an extern crate; in veles-cli it reaches the parent through
// `veles_runtime::veles_core` — `super::*` carries either.
#[allow(unused_imports)]
use super::*;
use std::future::Future;
use std::pin::Pin;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// The names a tier's fixture registers, so the scenarios can ask for
/// them by name.
pub struct Fixture {
    /// The owner tag its registrations carry (`so:<name>`,
    /// `module:<name>`).
    pub owner: &'static str,
    /// A tool whose answer contains `tool_marker`.
    pub tool: &'static str,
    pub tool_marker: &'static str,
    /// A tool that PANICS. `None` for a tier whose panic policy is not
    /// "answer an error" (none today).
    pub panic_tool: Option<&'static str>,
    /// A slash command whose render contains `command_marker`.
    pub command: &'static str,
    pub command_marker: &'static str,
    /// A hook, the args text that earns a verdict, and that verdict.
    pub hook: &'static str,
    pub hook_magic: &'static str,
    pub hook_verdict: &'static str,
    /// The tag its override of [`POINT`] appends to what `next` answers
    /// (PLAN-extension-api.md D2). `None` for a tier with no override.
    pub point_tag: Option<&'static str>,
}

/// The one scratch store every dispatch in this process shares. A fresh
/// SQLite file per call was ~14 fdatasyncs each; on a disk-backed TMPDIR
/// (the nix sandbox's /build is the SSD, the local gate's /dev/shm is
/// not) two dispatches took longer than the 150 ms timer plugin_so's
/// cancel case was racing, so the flake's check was red where every
/// local run passed (2026-10-13). No test reads this store: the tools
/// under test are the fixtures' own.
pub fn shared_store() -> std::sync::Arc<std::sync::Mutex<veles_core::store::Store>> {
    type Shared = std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>;
    static STORE: std::sync::OnceLock<(tempfile::TempDir, Shared)> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| {
            let dir = tempfile::TempDir::new().expect("scratch store dir");
            let store =
                veles_core::store::Store::open(&dir.path().join("state.db")).expect("store");
            (dir, std::sync::Arc::new(std::sync::Mutex::new(store)))
        })
        .1
        .clone()
}

/// One plugin tier under test.
pub trait Tier {
    fn fixture(&self) -> Fixture;
    /// Load (or start) the fixture; loading a loaded fixture is a
    /// reload.
    fn load(&mut self) -> BoxFut<'_, ()>;
    /// Unload it; answers the host's line.
    fn unload(&mut self) -> BoxFut<'_, String>;

    /// The fixture's tools registered right now, by name — the ones
    /// carrying its owner, so a sibling test's copy does not count.
    fn tool_names(&self) -> Vec<String> {
        let owner = self.fixture().owner;
        veles_core::tools::global_tools_with_owners()
            .into_iter()
            .filter(|(_, o)| *o == Some(owner))
            .map(|(n, _)| n)
            .collect()
    }

    /// Dispatch a global tool the way a session would: a registry built
    /// now (so it sees what was just loaded) over the process's one
    /// scratch store.
    fn tool(&self, name: &str, args: serde_json::Value) -> BoxFut<'_, String> {
        let name = name.to_string();
        Box::pin(async move {
            let reg = veles_core::tools::builtin_registry(shared_store(), None, "conformance");
            reg.dispatch(&name, args).await
        })
    }

    /// Render a plugin command, `None` when it is not registered.
    fn command(&self, name: &str, args: &str) -> BoxFut<'_, Option<String>> {
        let name = name.to_string();
        let args = args.to_string();
        Box::pin(async move {
            let cmd = veles_core::commands::plugin_command(&name)?;
            veles_core::commands::render_plugin_async(&cmd, &args)
                .await
                .ok()
        })
    }

    /// Fire a hook on the rust handlers, the way `HookBus::fire` does:
    /// on a blocking thread (a handler may block on the engine — a
    /// script tier's does — which an executor thread must never do).
    fn hook(&self, name: &str, args: &str) -> BoxFut<'_, Option<String>> {
        let (n, a) = (name.to_string(), args.to_string());
        Box::pin(async move {
            veles_core::rt::spawn_blocking(move || veles_core::rustplugins::run_hooks(&n, &a))
                .await
                .unwrap_or(None)
        })
    }
}

/// Every scenario, in order; each loads what it needs and leaves the
/// fixture unloaded. Answers the names that passed (a failure panics
/// with the scenario's name in the message).
pub async fn run_all(t: &mut dyn Tier) -> Vec<&'static str> {
    let mut passed = Vec::new();
    loads_and_registers(t).await;
    passed.push("loads_and_registers");
    unload_purges_everything(t).await;
    passed.push("unload_purges_everything");
    reload_does_not_double(t).await;
    passed.push("reload_does_not_double");
    if t.fixture().panic_tool.is_some() {
        a_panicking_tool_answers_an_error_and_the_host_lives(t).await;
        passed.push("a_panicking_tool_answers_an_error_and_the_host_lives");
    }
    if t.fixture().point_tag.is_some() {
        an_override_wraps_next_and_unload_restores(t).await;
        passed.push("an_override_wraps_next_and_unload_restores");
    }
    passed
}

/// After a load the fixture's tool answers, its command renders and
/// its hook has a verdict on the magic text and none otherwise.
pub async fn loads_and_registers(t: &mut dyn Tier) {
    let f = t.fixture();
    t.load().await;
    let out = t
        .tool(f.tool, serde_json::json!({"text": "conformance"}))
        .await;
    assert!(
        out.contains(f.tool_marker),
        "[loads_and_registers] the tool `{}` answered without its marker: {out}",
        f.tool
    );
    let rendered = t.command(f.command, "conformance").await;
    assert!(
        rendered
            .as_deref()
            .is_some_and(|r| r.contains(f.command_marker)),
        "[loads_and_registers] the command `{}` did not render: {rendered:?}",
        f.command
    );
    assert_eq!(
        t.hook(f.hook, &format!("{{\"content\":\"{}\"}}", f.hook_magic))
            .await
            .as_deref(),
        Some(f.hook_verdict),
        "[loads_and_registers] the hook `{}` had no verdict on the magic text",
        f.hook
    );
    assert!(
        t.hook(f.hook, r#"{"content":"plain"}"#).await.is_none(),
        "[loads_and_registers] the hook `{}` had an opinion on plain text",
        f.hook
    );
    t.unload().await;
}

/// After an unload nothing the fixture registered is reachable: no
/// tool under its owner, no command, no hook verdict.
pub async fn unload_purges_everything(t: &mut dyn Tier) {
    let f = t.fixture();
    t.load().await;
    assert!(
        t.tool_names().iter().any(|n| n == f.tool),
        "[unload_purges_everything] precondition: the tool is registered"
    );
    let line = t.unload().await;
    assert!(
        line.contains("unloaded"),
        "[unload_purges_everything] the host's line must say so: {line}"
    );
    assert!(
        t.tool_names().is_empty(),
        "[unload_purges_everything] tools survived the unload: {:?}",
        t.tool_names()
    );
    assert!(
        t.command(f.command, "x").await.is_none(),
        "[unload_purges_everything] the command `{}` survived the unload",
        f.command
    );
    assert!(
        t.hook(f.hook, &format!("{{\"content\":\"{}\"}}", f.hook_magic))
            .await
            .is_none(),
        "[unload_purges_everything] the hook `{}` still answers",
        f.hook
    );
}

/// Loading twice (a reload) leaves one of each registration, not two.
pub async fn reload_does_not_double(t: &mut dyn Tier) {
    let f = t.fixture();
    t.load().await;
    t.load().await;
    let names = t.tool_names();
    assert_eq!(
        names.iter().filter(|n| n.as_str() == f.tool).count(),
        1,
        "[reload_does_not_double] `{}` after two loads: {names:?}",
        f.tool
    );
    let out = t.tool(f.tool, serde_json::json!({"text": "again"})).await;
    assert!(
        out.contains(f.tool_marker),
        "[reload_does_not_double] the tool stopped answering after the reload: {out}"
    );
    t.unload().await;
}

/// A tool that panics answers an error string; the host lives and the
/// fixture's other tool still answers.
pub async fn a_panicking_tool_answers_an_error_and_the_host_lives(t: &mut dyn Tier) {
    let f = t.fixture();
    let Some(panic_tool) = f.panic_tool else {
        return;
    };
    t.load().await;
    let out = t.tool(panic_tool, serde_json::json!({})).await;
    assert!(
        out.contains("error") && out.to_lowercase().contains("panic"),
        "[a_panicking_tool…] the panic must come back as an error that says so: {out}"
    );
    let out = t
        .tool(f.tool, serde_json::json!({"text": "still here"}))
        .await;
    assert!(
        out.contains(f.tool_marker),
        "[a_panicking_tool…] the host did not survive the panic: {out}"
    );
    t.unload().await;
}

/// The override point every tier's fixture overrides
/// (PLAN-extension-api.md D2): core's default answers `{"via":["core"]}`.
pub const POINT: &str = "conformance.point";

/// Declare [`POINT`] once per process, as core.
pub fn declare_point() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        veles_core::services::declare_point(
            veles_core::services::PointDecl {
                name: POINT.into(),
                summary: "the conformance suite's point".into(),
                input: serde_json::json!({"type": "object"}),
                output: serde_json::json!({"type": "object", "required": ["via"]}),
                on_error: veles_core::services::OnError::FallThrough,
                deadline_ms: 20_000,
            },
            veles_core::services::handler(|_params, _cx| async {
                Ok(serde_json::json!({"via": ["core"]}))
            }),
        )
        .expect("declare the conformance point");
    });
}

/// The fixture overrides [`POINT`] by wrapping `next`: the answer is
/// core's with the fixture's tag after it. Unloading the fixture puts
/// core's answer back.
pub async fn an_override_wraps_next_and_unload_restores(t: &mut dyn Tier) {
    let f = t.fixture();
    let Some(tag) = f.point_tag else {
        return;
    };
    declare_point();
    t.load().await;
    let v = veles_core::services::ask(POINT, serde_json::json!({}), None)
        .await
        .unwrap_or_else(|e| panic!("[an_override_wraps_next…] the point failed: {e}"));
    assert_eq!(
        v["via"],
        serde_json::json!(["core", tag]),
        "[an_override_wraps_next…] the override must wrap what `next` answered: {v}"
    );
    t.unload().await;
    let v = veles_core::services::ask(POINT, serde_json::json!({}), None)
        .await
        .unwrap_or_else(|e| {
            panic!("[an_override_wraps_next…] the point failed after the unload: {e}")
        });
    assert_eq!(
        v["via"],
        serde_json::json!(["core"]),
        "[an_override_wraps_next…] the unload must put core's answer back: {v}"
    );
}
