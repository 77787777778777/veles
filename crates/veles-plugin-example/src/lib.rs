//! veles-plugin-example — the reference RUST plugin (Phase A, in-tree).
//!
//! This crate is the worked example the plugin plan Part 3 calls for:
//! the smallest thing that exercises every surface a Rust plugin gets.
//! It is linked into the `veles` binary and registered from the
//! composition root (`veles-cli`) before any agent or gateway boots.
//!
//! What it demonstrates, one feature per surface:
//!
//!   - A `post_llm_call` HOOK: observes every finished assistant turn.
//!     It is pure observation (returns `None`), so it never changes
//!     behavior — the point is to show the sync handler signature and
//!     the args_json it receives. Flip `VELES_PLUGIN_EXAMPLE_ANNOUNCE`
//!     to make it park a plugin-action note instead, which is how a
//!     hook that DOES something stays visible.
//!
//!   - A `rust_echo` TOOL: echoes its input plus a word count, so a
//!     session can prove a Rust tool is resident and callable. It
//!     exists because "a Rust plugin can register a tool" was the claim
//!     Phase A had to make true; this is the proof you can invoke.
//!
//! Deliberately NOT here: a gateway filter (the Tcl `tcl-alias-filter`
//! example covers that decision shape), and any network/state. Keep
//! this crate boring — it is the template people copy.

use std::sync::Arc;
use veles_core::rustplugins::{RustHookFn, VelesPlugin};
use veles_core::tools::Tool;

/// The plugin. Stateless; all interesting bits live in the hook closure
/// and the tool.
pub struct ExamplePlugin;

impl ExamplePlugin {
    /// Build the post_llm_call handler. Kept as its own function so the
    /// behavior is unit-testable without spinning a registry.
    fn post_llm_call_handler() -> RustHookFn {
        Arc::new(|args_json: &str| {
            // Pure observation: parse just enough to log something true,
            // return None so the next handler (or the engine) decides.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) {
                let provider = v.get("provider").and_then(|p| p.as_str()).unwrap_or("?");
                let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
                tracing::debug!("example-plugin: turn finished on {provider}/{model}");
                if std::env::var("VELES_PLUGIN_EXAMPLE_ANNOUNCE").as_deref() == Ok("1") {
                    veles_core::notes::record_plugin_note(
                        &format!("example plugin saw a turn on {provider}/{model}"),
                        "",
                    );
                }
            }
            None
        })
    }
}

impl VelesPlugin for ExamplePlugin {
    fn name(&self) -> &str {
        "example"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn hooks(&self) -> Vec<(String, RustHookFn)> {
        vec![("post_llm_call".to_string(), Self::post_llm_call_handler())]
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(RustEchoTool)]
    }
}

/// The constructor the composition root calls. A function (not a const)
/// so Phase B's loader can call the same shape from a `.so`.
pub fn plugin() -> Arc<dyn VelesPlugin> {
    Arc::new(ExamplePlugin)
}

/// `rust_echo` — returns its input plus a word count. The schema is the
/// contract; `call` is total (never panics), because a plugin tool that
/// panics would take down a turn.
struct RustEchoTool;

#[async_trait::async_trait]
impl Tool for RustEchoTool {
    fn name(&self) -> &str {
        "rust_echo"
    }
    fn description(&self) -> &str {
        "Echo the input text back with a word count. Demonstrates a Rust plugin tool."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Text to echo"}
            },
            "required": ["text"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> String {
        let text = args
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let words = text.split_whitespace().count();
        format!("{text}\n[words: {words}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veles_core::rustplugins::{register_plugin, run_hooks};

    #[test]
    fn plugin_declares_its_surfaces() {
        let p = ExamplePlugin;
        assert_eq!(p.name(), "example");
        assert_eq!(p.hooks().len(), 1, "one hook");
        assert_eq!(p.hooks()[0].0, "post_llm_call");
        assert_eq!(p.tools().len(), 1, "one tool");
        assert_eq!(p.tools()[0].name(), "rust_echo");
    }

    /// The hook is observation-only: whatever the turn, it returns None
    /// (no verdict), so it can never alter engine behavior.
    #[test]
    fn post_llm_call_is_pure_observation() {
        register_plugin(plugin());
        assert!(
            run_hooks("post_llm_call", r#"{"provider":"p","model":"m"}"#).is_none(),
            "observation must not produce a verdict"
        );
        // Malformed args are tolerated — a hook must never panic on a
        // payload shape it did not expect.
        assert!(run_hooks("post_llm_call", "not json").is_none());
    }

    #[tokio::test]
    async fn rust_echo_counts_words() {
        let tool = RustEchoTool;
        let out = tool
            .call(serde_json::json!({"text": "one two three"}))
            .await;
        assert!(out.contains("one two three"), "{out}");
        assert!(out.contains("[words: 3]"), "{out}");
        // Empty input is fine — total function.
        let out = tool.call(serde_json::json!({})).await;
        assert!(out.contains("[words: 0]"), "{out}");
    }
}
