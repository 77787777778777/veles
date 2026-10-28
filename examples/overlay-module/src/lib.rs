//! greeter: an overlay module for veles.
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
        name: "greeter".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: veles_runtime::veles_types::plugin::PROTOCOL,
        description: "an overlay module for veles".into(),
        // The rest at its default: one instance, no lease, no platform,
        // no services, no override points.
        ..Default::default()
    }
}

#[derive(Default)]
pub struct Module;

impl veles_core::rustplugins::VelesPlugin for Module {
    fn name(&self) -> &str {
        "greeter"
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
        "greeter_hello"
    }
    fn description(&self) -> &str {
        "say hello from greeter"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"who": {"type": "string"}}})
    }
    async fn call(&self, args: serde_json::Value) -> String {
        let who = args["who"].as_str().unwrap_or("world");
        format!("hello, {who}, from greeter")
    }
}

veles_core::export!(Module, manifest());
