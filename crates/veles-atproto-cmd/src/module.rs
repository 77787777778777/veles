//! The `/atproto` command as an in-process MODULE (the shape of
//! PLAN-plugins-installer.md R5): the host-facing shell — the manifest,
//! `start(cx)`, `stop`. The module FILE is crates/veles-module-atproto-cmd,
//! which exports this.
//!
//! There is nothing to start but the command: no config of its own, no
//! state dir, no tasks. The byte-moving lives in the atproto plugin,
//! wherever it runs; without it the command says so (the service
//! registry's own "no running plugin provides …" names it).

use std::future::Future;
use std::pin::Pin;

use veles_core::rustplugins::{ModuleContext, VelesPlugin};
use veles_types::plugin::{Instances, Manifest};

#[derive(Default)]
pub struct AtprotoCmdModule;

/// The manifest the module exports (crates/veles-module-atproto-cmd).
/// One instance, no lease: the command is stateless, so every node may
/// load it — the delete itself lands where the atproto plugin runs.
pub fn manifest() -> Manifest {
    Manifest {
        name: "atproto-cmd".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description: "the /atproto owner command: basic administration of the bot's atproto \
                      account (delete one of its own records, verify-first), independent of \
                      the slopdrop bridge"
            .into(),
        instances: Instances::Single,
        lease: None,
        platforms: Vec::new(),
        config_sections: Vec::new(),
        knobs: Vec::new(),
        secrets: Vec::new(),
        services: Vec::new(),
        // The delete moves bytes through the atproto plugin's generic
        // repo services, where it runs; without it the command says so.
        uses: [
            (
                veles_types::plugin::method::REPO_WHOAMI,
                "whose repo a delete would land in (the session DID)",
            ),
            (
                veles_types::plugin::method::REPO_GET_RECORD,
                "the record must be there before it is deleted",
            ),
            (
                veles_types::plugin::method::REPO_DELETE_RECORD,
                "the delete itself",
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

impl VelesPlugin for AtprotoCmdModule {
    fn name(&self) -> &str {
        "atproto-cmd"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn start(
        &self,
        _cx: ModuleContext,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            crate::command::register().map_err(|e| anyhow::anyhow!("/atproto: {e}"))?;
            veles_core::module::log(
                tracing::Level::INFO,
                "atproto-cmd: /atproto registered (delete one of the bot's own records, \
                 verify-first)",
            );
            Ok(())
        })
    }
}
