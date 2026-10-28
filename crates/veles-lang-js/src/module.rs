//! The host-facing shell: the manifest, `start(cx)`, `stop`. The module
//! FILE is crates/veles-module-lang-js, which exports this.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use veles_runtime::anyhow;
use veles_runtime::veles_core;
use veles_runtime::veles_core::rustplugins::{ModuleContext, VelesPlugin};
use veles_runtime::veles_types::plugin::{Instances, Manifest};

use crate::engine::JsEngine;

/// One instance: the engine, from `start` to `stop`.
#[derive(Default)]
pub struct JsHost {
    engine: std::sync::Mutex<Option<Arc<JsEngine>>>,
}

/// The manifest the module exports (crates/veles-module-lang-js).
pub fn manifest() -> Manifest {
    Manifest {
        name: "lang-js".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description: "JavaScript/TypeScript plugins (QuickJS-ng; TypeScript stripped by oxc): \
                      the host of every `kind = \"js\"` plugin on this node."
            .into(),
        instances: Instances::Single,
        // Every node runs its own scripts: no lease.
        lease: None,
        platforms: Vec::new(),
        config_sections: Vec::new(),
        knobs: Vec::new(),
        secrets: Vec::new(),
        services: Vec::new(),
        uses: Vec::new(),
        hooks: Vec::new(),
        enabled_env: None,
        required_env: None,
        points: Vec::new(),
    }
}

impl VelesPlugin for JsHost {
    fn name(&self) -> &str {
        "lang-js"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// Build the engine, keep its job driver on the instance runtime,
    /// register as the `js` host (under this instance's owner, read
    /// from the thread) and load every enabled `js` plugin.
    fn start(
        &self,
        cx: ModuleContext,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let engine = JsEngine::new(cx.handle.clone())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            *self.engine.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&engine));
            cx.handle.spawn(engine.drive());
            let host: Arc<dyn veles_core::scripthost::ScriptHost> = engine.clone();
            veles_core::scripthost::register_host(host);
            let lines = veles_core::scripthost::load_all_for_kind(crate::KIND).await;
            veles_core::module::log(
                veles_runtime::tracing::Level::INFO,
                &format!(
                    "lang-js: QuickJS-ng up; {} script(s) loaded: {}",
                    engine.loaded().len(),
                    if lines.is_empty() {
                        "none enabled".to_string()
                    } else {
                        lines.join(" | ")
                    }
                ),
            );
            Ok(())
        })
    }

    /// Drop every script's context and the engine. The seam purges the
    /// scripts' registrations when the host's owner is purged.
    fn stop(&self, reason: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let reason = reason.to_string();
        let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner()).take();
        Box::pin(async move {
            let Some(engine) = engine else {
                veles_core::module::log(
                    veles_runtime::tracing::Level::INFO,
                    &format!("lang-js: stop (\"{reason}\"): no engine was running"),
                );
                return;
            };
            let n = engine.unload_all().await;
            veles_core::module::log(
                veles_runtime::tracing::Level::INFO,
                &format!(
                    "lang-js: stop (\"{reason}\"): {n} script context(s) dropped after {:.0?}",
                    engine.uptime()
                ),
            );
        })
    }
}
