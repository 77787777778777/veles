//! The slopdrop bridge as an in-process MODULE (PLAN-plugins-installer.md
//! R5): the host-facing shell — the manifest, `start(cx)`, `stop`. The
//! module FILE is crates/veles-module-slopdrop, which exports this.
//!
//! `start` reads its `[slopdrop]` section out of the context, takes the
//! state dir the host hands it (migrating the linked version's tree
//! once, keeping a backup), and fills every seam core opened for it —
//! all owner-tracked, so the host's purge on unload restores core's own
//! behaviour: the local Tcl sandbox, a fail-closed public `/tcl`, no
//! `triggers.forward` provider (a refusing Slopdrop trigger row), 503 on
//! the mirror's routes. It then
//! launches the timer poll onto the instance runtime (teardown's
//! `shutdown_timeout` drops it). `stop` has nothing to say: there is no
//! connection to part from, and the purge is the host's.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use veles_core::rustplugins::{ModuleContext, VelesPlugin};
use veles_types::plugin::{Instances, Manifest};

#[derive(Default)]
pub struct SlopdropModule;

/// The manifest the module exports (crates/veles-module-slopdrop). One
/// instance, no lease: the bridge is an HTTP client, and every node
/// with `[slopdrop]` configured may run it (the ledger and the asks act
/// on the node that runs the atproto plugin, which is where `/tcl`
/// replies to a public feed are recorded).
pub fn manifest() -> Manifest {
    Manifest {
        name: "slopdrop".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description: "the slopdrop bridge: tcl in a persistent interpreter, the public /tcl \
                      safety layer and ledger, the trigger forwarder, the state mirror"
            .into(),
        instances: Instances::Single,
        lease: None,
        platforms: Vec::new(),
        config_sections: vec!["slopdrop".into()],
        knobs: crate::config_surface(),
        // The bearer env names are config (`token_env`), so they are
        // declared at start (`secrets::declare_platform_secret`), not here.
        secrets: Vec::new(),
        // The trigger bridge, while `[slopdrop]` is enabled.
        services: crate::triggers::service_decls(),
        // The ledger's approve and delete move bytes through the atproto
        // plugin, where it runs; without it they say so.
        uses: [
            (
                veles_types::plugin::method::POST_CREATE,
                "an approve reposts a withheld /tcl reply",
            ),
            (
                veles_types::plugin::method::REPO_DELETE_RECORD,
                "a delete takes its posts down",
            ),
            (
                veles_types::plugin::method::HOSTED_DELETE,
                "a delete takes its hosted copy down",
            ),
        ]
        .into_iter()
        .map(|(name, why)| veles_types::plugin::UseDecl {
            name: name.into(),
            optional: true,
            description: why.into(),
        })
        .collect(),
        hooks: Vec::new(),
        enabled_env: None,
        required_env: None,
        points: Vec::new(),
    }
}

impl VelesPlugin for SlopdropModule {
    fn name(&self) -> &str {
        "slopdrop"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// `tcl_state_eval`, registered under the module's owner by the
    /// host's `register_plugin` (its cap is declared in `start`).
    fn tools(&self) -> Vec<Arc<dyn veles_core::tools::Tool>> {
        vec![Arc::new(crate::eval::SlopdropEvalTool::default())]
    }

    fn start(
        &self,
        cx: ModuleContext,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            // The section, from the context (the host read the raw
            // config); later reads follow the file.
            let raw = serde_json::json!({
                "slopdrop": cx.config.get("slopdrop").cloned().unwrap_or(serde_json::Value::Null)
            });
            let section = crate::settings::section_from(&raw);
            crate::settings::install(section.clone());

            // The state dir, and the one-time move of the linked
            // version's tree (a backup stays beside it).
            crate::data::set_state_dir(Some(std::path::PathBuf::from(&cx.dirs.state)));
            match crate::data::migrate_once() {
                Ok(crate::data::Migration::Moved { files, backup }) => {
                    let line = format!(
                        "slopdrop: moved {files} file(s) from the linked version's data dir into {}; the old tree is kept at {}",
                        crate::data::dir().display(),
                        backup.display()
                    );
                    veles_core::module::log(tracing::Level::INFO, &line);
                    veles_core::notes::record_plugin_note("slopdrop data migrated", &line);
                }
                Ok(crate::data::Migration::Nothing) => {}
                Err(e) => veles_core::module::log(
                    tracing::Level::WARN,
                    &format!("slopdrop: migrating the linked version's data dir: {e}"),
                ),
            }

            // The seams. Each records this instance's owner (the thread
            // tag), so the host's purge takes exactly these back out.
            veles_core::acl::declare_tool_cap(crate::eval::TOOL_NAME, veles_core::acl::Cap::Shell);
            for name in [&section.token_env, &section.admin_token_env]
                .into_iter()
                .flatten()
            {
                veles_core::secrets::declare_platform_secret(name);
            }
            veles_core::tcleval::set_backend(Arc::new(crate::eval::Backend));
            veles_core::tcleval::set_public_reply(Arc::new(crate::public::Policy));
            // The trigger bridge is a service the IRC adapter calls; only
            // a node whose bridge is enabled provides it.
            if section.is_enabled() {
                crate::triggers::register_services()
                    .map_err(|e| anyhow::anyhow!("the trigger services: {e}"))?;
                // Bluesky events (the atproto plugin's notifications) run
                // the trigger rows, which live in the gateway's store.
                crate::bsky::park_store(cx.store.clone());
                crate::bsky::park_bus(cx.bus.clone());
                veles_core::services::register(
                    veles_core::services::ServiceDecl {
                        name: veles_types::plugin::method::BSKY_EVENT.into(),
                        description: "run one Bluesky notification through the atproto trigger \
                                      rows and slopdrop; the shaped answers to post"
                            .into(),
                        // Any caller may ASK; the handler answers only the
                        // atproto plugin (`bsky::event_service`).
                        cap: Some(veles_core::services::SERVICE_CAP_ANY.into()),
                    },
                    veles_core::services::handler(crate::bsky::event_service),
                )
                .map_err(|e| anyhow::anyhow!("the bsky.event service: {e}"))?;
            }
            veles_core::syncask::register_source(crate::ask::SOURCE, Arc::new(crate::ask::Source));
            veles_core::moduleroutes::register_routes("slopdrop", Arc::new(crate::routes::Routes));
            veles_core::bus::register_delivery_observer(
                "slopdrop-ledger",
                Arc::new(crate::receipts::Observer),
            );
            crate::command::register().map_err(|e| anyhow::anyhow!("/slopdrop: {e}"))?;

            veles_core::module::log(
                tracing::Level::INFO,
                &if section.is_enabled() {
                    format!(
                        "slopdrop: bridge at {} ({} scorer endpoint(s)); state in {}",
                        section.url.as_deref().unwrap_or("?"),
                        section.output_classifier.len(),
                        crate::data::dir().display()
                    )
                } else {
                    "slopdrop: [slopdrop] is not enabled on this node — tcl stays local, \
                     triggers forward nothing; the public /tcl policy and the ledger are in place"
                        .to_string()
                },
            );

            // The timer poll, on the instance runtime: teardown drops it.
            cx.handle.spawn(crate::triggers::run_timers());
            Ok(())
        })
    }

    fn stop(&self, _reason: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod installer_surface_tests {
    #[test]
    fn manifest_carries_the_slopdrop_config_surface() {
        let encoded = serde_json::to_value(super::manifest()).expect("manifest serializes");
        let actual: Vec<veles_types::Knob> =
            serde_json::from_value(encoded["knobs"].clone()).expect("module exposes knobs");
        assert_eq!(actual, crate::config_surface());
        assert!(actual.iter().any(|k| k.path.starts_with("slopdrop.")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_one_instance_and_its_section() {
        let m = manifest();
        assert_eq!(m.name, "slopdrop");
        assert_eq!(m.instances, Instances::Single);
        assert_eq!(m.lease, None);
        assert_eq!(m.config_sections, vec!["slopdrop".to_string()]);
        assert!(
            m.platforms.is_empty(),
            "no platform: it is not a connection"
        );
        assert_eq!(SlopdropModule.name(), "slopdrop");
        assert_eq!(SlopdropModule.tools().len(), 1);
    }
}
