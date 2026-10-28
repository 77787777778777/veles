//! The host-facing shell: the manifest, `start(cx)`, `stop`. The module
//! FILE is crates/veles-module-lang-racket, which exports this.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use veles_runtime::anyhow;
use veles_runtime::veles_core;
use veles_runtime::veles_core::rustplugins::{ModuleContext, VelesPlugin};
use veles_runtime::veles_types::plugin::{Instances, Manifest};

use crate::engine::RacketEngine;

/// One instance: the engine from `start` to `stop` — the engine itself
/// outlives the instance (see engine.rs).
#[derive(Default)]
pub struct RacketHost {
    engine: std::sync::Mutex<Option<Arc<RacketEngine>>>,
}

/// The manifest the module exports (crates/veles-module-lang-racket).
pub fn manifest() -> Manifest {
    Manifest {
        name: "lang-racket".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description: "Racket plugins (Racket CS 9.3 embedded): the host of every \
                      `kind = \"racket\"` plugin on this node. Process-global: it stays \
                      mapped for the gateway's life, and a replaced file is a restart."
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

impl VelesPlugin for RacketHost {
    fn name(&self) -> &str {
        "lang-racket"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// Boot Racket (or reuse the booted engine with this instance's
    /// runtime), register as the `racket` host (under this instance's
    /// owner, read from the thread) and load every enabled `racket`
    /// plugin.
    fn start(
        &self,
        cx: ModuleContext,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let t0 = std::time::Instant::now();
            let fresh = cx.handle.clone();
            let engine = RacketEngine::get_or_boot(fresh, Path::new(&cx.dirs.state))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let reused = engine.uptime() > t0.elapsed() + std::time::Duration::from_millis(1);
            *self.engine.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&engine));
            let host: Arc<dyn veles_core::scripthost::ScriptHost> = engine.clone();
            veles_core::scripthost::register_host(host);
            let lines = veles_core::scripthost::load_all_for_kind(crate::KIND).await;
            veles_core::module::log(
                veles_runtime::tracing::Level::INFO,
                &format!(
                    "lang-racket: Racket CS {} {}; {} script(s) loaded: {}",
                    engine.version(),
                    if reused {
                        format!("reused (booted {:.0?} ago)", engine.uptime())
                    } else {
                        format!("booted in {:.0?}", t0.elapsed())
                    },
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

    /// Unload every script; Racket stays booted. The seam purges the
    /// scripts' registrations when the host's owner is purged.
    fn stop(&self, reason: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let reason = reason.to_string();
        let engine = self.engine.lock().unwrap_or_else(|p| p.into_inner()).take();
        Box::pin(async move {
            let Some(engine) = engine else {
                veles_core::module::log(
                    veles_runtime::tracing::Level::INFO,
                    &format!("lang-racket: stop (\"{reason}\"): no engine was running"),
                );
                return;
            };
            let n = engine.unload_all().await;
            engine.detach();
            veles_core::module::log(
                veles_runtime::tracing::Level::INFO,
                &format!(
                    "lang-racket: stop (\"{reason}\"): {n} script(s) unloaded; Racket CS stays \
                     booted ({:.0?} up) — its engine thread keeps this module mapped for the \
                     gateway's life, a `start` reuses it, a replaced module file is a restart",
                    engine.uptime()
                ),
            );
        })
    }
}
