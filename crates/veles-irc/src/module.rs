//! The IRC platform as an in-process MODULE (PLAN-plugins-installer.md
//! R4): one instance per configured network, each supervised by the
//! module host under its unchanged lease (`singleton:irc:{instance}`).
//!
//! The adapter code (`client.rs`, `tools.rs`, …) is unchanged in what it
//! DOES; this file is the host-facing shell: the manifest, `start(cx)`
//! and `stop(reason)`. The module FILE is crates/veles-module-irc, which
//! exports this shell; veles-irc itself is a plain library the gateway
//! links for its config surfaces and never loads as a module.
//!
//! - `start` parks the store the context carries (the history seam
//!   `veles-cli` used to wire from the other side of the dependency),
//!   registers the `irc` tool, then LAUNCHES the connect/reconnect loop
//!   onto the instance runtime (`cx.handle`) and returns. Launching
//!   rather than blocking is what makes a reload fast: teardown's
//!   `runtime.shutdown_timeout` drops the loop task instead of waiting
//!   out a blocking `start`.
//! - `stop(reason)` sends QUIT on the connection (`IrcCommand::Quit`),
//!   so the network sees a clean parting line, then unregisters the
//!   instance's handle and unparks its store — the two module-local
//!   statics that would otherwise hold entries for an instance that no
//!   longer exists.
//!
//! Everything else teardown needs is already owner-tracked from R2/R3:
//! the tool registration (purged by `purge_owner_tools`), the tap
//! senders/ops (`purge_owner_presences`), and the bus outbound route
//! (the manifest declares `platforms: [irc]`, so the host unregisters
//! `irc/<instance>` itself).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use veles_runtime::anyhow;
use veles_runtime::veles_core;
use veles_runtime::veles_core::rustplugins::{ModuleContext, VelesPlugin};
use veles_runtime::veles_types::plugin::{Instances, Manifest, PlatformDecl};

use crate::client;
use crate::IrcConfig;

/// The module object the host constructs once per instance load
/// (`export!`'s `construct` runs inside each instance's `Loaded`), so
/// one `IrcModule` IS one network's instance — it may carry per-instance
/// state, and does: the network it launched, set by `start` and read by
/// `stop`. `stop`'s trait signature gets no instance, so without this it
/// cannot tell which connection is its own.
#[derive(Default)]
pub struct IrcModule {
    /// `cfg.name` as `start` computed it (for a named instance this IS
    /// `cx.instance`). A `Mutex` because `start` and `stop` run on
    /// different threads of the host runtime, and the trait hands `&self`.
    network: std::sync::Mutex<Option<String>>,
}

/// The manifest the module exports (crates/veles-module-irc).
pub fn manifest() -> Manifest {
    Manifest {
        name: "irc".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description: "IRC platform: irc-rs transport, SASL PLAIN/EXTERNAL (CertFP), \
                      formatting-safe; one instance per network."
            .into(),
        // One instance per `[[platforms.irc.networks]]` entry, named by
        // its `name` — the same fan-out `IrcConfig::all_from_config`
        // does when linked. A single-network config (the `IRC_*` env
        // shape) has no `networks` array, so `instances_of` falls back
        // to the `""` instance that reads the top-level block + env.
        instances: Instances::PerEntry {
            array: "platforms.irc.networks".into(),
            name: "name".into(),
        },
        // The SAME key the linked adapter held (`arbiter::lease_key_irc`
        // = "singleton:irc:<net>"), so a mixed-version pool — this node
        // a module, a peer still linked — arbitrates the one network
        // connection between them instead of both connecting.
        lease: Some("singleton:irc:{instance}".into()),
        platforms: vec![PlatformDecl {
            key: "irc".into(),
            public_feed: false,
        }],
        // What `start` reads out of `cx.config`: the platform block
        // itself. The adapter's local ask/ACL handling reads core's
        // live ACL (`acl::live_config`), not a config subtree, so `acl`
        // is not a section here.
        config_sections: vec!["platforms.irc".into()],
        knobs: crate::config_surface(),
        // In-process: secrets are read from the config home's dotenv
        // directly (modules are fully trusted code), not proxied.
        secrets: Vec::new(),
        services: Vec::new(),
        // The trigger rows (triggers_hook.rs): a Slopdrop row forwards to
        // the bridge, a Bluesky row reads the link as the bot. Each row
        // kind is refused, with the reason, where nothing provides it.
        uses: [
            (
                veles_runtime::veles_types::plugin::method::TRIGGERS_FORWARD,
                "Slopdrop trigger rows",
            ),
            (
                veles_runtime::veles_types::plugin::method::TRIGGERS_MASKS,
                "the forward mask of Slopdrop rows",
            ),
            (
                veles_runtime::veles_types::plugin::method::APPVIEW_QUERY,
                "Bluesky trigger rows: link previews as the bot",
            ),
            (
                veles_runtime::veles_types::plugin::method::REPO_GET_RECORD,
                "Bluesky trigger rows: what a block hides, read raw",
            ),
        ]
        .into_iter()
        .map(|(name, why)| veles_runtime::veles_types::plugin::UseDecl {
            name: name.into(),
            optional: true,
            description: why.into(),
        })
        .collect(),
        hooks: Vec::new(),
        // Off on a node whose `.env` says so (the linked adapter's switch).
        enabled_env: Some("IRC_ENABLED".into()),
        required_env: None,
        points: crate::points::point_decls(),
    }
}

impl IrcModule {
    /// Build this instance's effective `IrcConfig` from its context.
    ///
    /// The config arrives in `cx.config["platforms.irc"]` as JSON (the
    /// process tier's shape); the adapter's `apply_mapping` reads a
    /// `serde_yaml::Value`, and the file was TOML, so the JSON round-
    /// trips through yaml. A named instance's
    /// `[[platforms.irc.networks]]` entry is layered over the block; a
    /// `""` instance (the env-driven single network) applies the `IRC_*`
    /// env/dotenv over the block, exactly as `single_from_config` did.
    ///
    /// Verbosity is NOT applied from a `VelesConfig` here (the module
    /// is handed sections, not the whole config): the linked adapter's
    /// `apply_verbosity` set the Normal baseline (`tool_progress =
    /// Posture`, `show_route = true`, `show_reasoning = false`), and
    /// this starts from the same baseline explicitly — a `[platforms.
    /// irc]` `tool_progress`/`show_*` key or `IRC_*` env var still
    /// overrides it in `apply_mapping`/`apply_env` below, exactly as
    /// they did over `apply_verbosity`.
    fn config_for(cx: &ModuleContext) -> anyhow::Result<IrcConfig> {
        let plat = cx
            .config
            .get("platforms.irc")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let block: serde_yaml::Value = serde_json::from_value(plat)
            .map_err(|e| anyhow::anyhow!("platforms.irc: not a mapping: {e}"))?;

        let mut cfg = IrcConfig {
            // The Normal verbosity baseline (see the doc above).
            show_reasoning: false,
            tool_progress: crate::ToolProgress::Posture,
            show_route: true,
            ..IrcConfig::default()
        };

        crate::apply_mapping(&mut cfg, &block);

        if cx.instance.is_empty() {
            // Single-network: the env wins over the block, as it always
            // has (`single_from_config` ends with `apply_env`).
            let home = veles_core::config::config_dir();
            let env = |k: &str| veles_core::providers::resolve_env_or_dotenv(k, &home);
            crate::apply_env(&mut cfg, &env);
            if cfg.name.is_empty() {
                cfg.name = cfg.server.clone();
            }
        } else {
            // A named instance: its `networks` entry over the block
            // (`all_from_config`'s layering), named by the instance.
            if let Some(entry) = block
                .get("networks")
                .and_then(|n| n.as_sequence())
                .and_then(|arr| {
                    arr.iter()
                        .find(|e| {
                            e.get("name").and_then(|n| n.as_str()) == Some(cx.instance.as_str())
                        })
                        .cloned()
                })
            {
                crate::apply_mapping(&mut cfg, &entry);
            }
            cfg.name = cx.instance.clone();
        }
        Ok(cfg)
    }
}

impl VelesPlugin for IrcModule {
    fn name(&self) -> &str {
        "irc"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn start(
        &self,
        cx: ModuleContext,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let cfg = Self::config_for(&cx)?;

            // Record this instance's network so `stop` (which the trait
            // gives no instance) quits only THIS connection. Done before
            // the loop launches, so even a `stop` that lands mid-connect
            // names the right network.
            if let Ok(mut n) = self.network.lock() {
                *n = Some(cfg.name.clone());
            }

            // The store seam: the instance parks what the context
            // carries, under ITS network name, so `!history` and trigger
            // context read the network they name. (Linked, veles-cli
            // parked the gateway's store per network; the module does
            // it for itself now.) `None` on a host with no store (a
            // test harness) — history then answers "no store", honestly.
            if let Some(store) = cx.store.clone() {
                client::park_store(store, &cfg.name);
            }

            // The `irc` tool. Registered per `start` — NOT behind a
            // `Once` — because a reload purges the instance's
            // registrations (`purge_owner_tools`) and the tool must come
            // back with the new load. `register_global_tool` replaces by
            // name, so a two-network gateway's second `start` refreshes
            // the same tool rather than stacking it. (The tool resolves
            // its network per call; one registration serves every
            // network.)
            veles_core::tools::register_global_tool(Arc::new(crate::tools::IrcTool::default()));
            // The tool gates its OWN capabilities per action (reading a
            // buffer is not opping a nick); declared so the engine's
            // fail-closed gate for unclassified tools does not refuse
            // it for everyone but the owner.
            veles_core::acl::declare_self_gated("irc");
            // The override point this module declares (PLAN-extension-api.md
            // E4): whether a channel line is for the bot. Under the
            // instance's owner, every start: a reload purges and declares
            // again.
            crate::points::declare().map_err(|e| anyhow::anyhow!("{e}"))?;

            // The connect/reconnect loop, LAUNCHED on the instance
            // runtime: a reload's `runtime.shutdown_timeout` drops it.
            // The bus comes from the host (the module host hands every
            // instance the gateway's bus through the context).
            let bus = cx.bus.clone();
            let name = cfg.name.clone();
            cx.handle.spawn(async move {
                if let Err(e) = client::run_adapter(&cfg, bus).await {
                    veles_core::module::log(
                        veles_runtime::tracing::Level::ERROR,
                        &format!("irc/{name}: adapter loop ended: {e:#}"),
                    );
                }
            });
            Ok(())
        })
    }

    fn stop(&self, reason: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let reason = reason.to_string();
        // `stop` gets no instance (the trait's signature predates
        // instances that differ inside one module), so it reads the
        // network `start` recorded on THIS object and quits only that
        // connection. A one-network reload must NOT touch the others —
        // before this it called `quit_all` and reloading `irc:synirc`
        // dropped `irc.IRC4Fun.net` too (2026-10-08 live run), and that
        // instance never noticed its connection was gone (its own
        // reconnect loop had not been told). When `start` never ran
        // (a refused load) there is no network and nothing to quit.
        let own = self.network.lock().ok().and_then(|n| n.clone());
        Box::pin(async move {
            if let Some(net) = own {
                let t0 = std::time::Instant::now();
                let sent = client::quit_network(&net, client::bot_presence(), &reason).await;
                // The server shows the reason only if it keeps it: an
                // UnrealIRCd network (SynIRC) replaces it with the nick
                // on a connection younger than its
                // `anti-spam-quit-message-time`.
                veles_core::module::log(
                    veles_runtime::tracing::Level::INFO,
                    &format!(
                        "irc/{net}: stop: {} (\"{reason}\", {:?})",
                        if sent {
                            "QUIT sent"
                        } else {
                            "no connection took the QUIT"
                        },
                        t0.elapsed()
                    ),
                );
                // Leave no entries behind: the handle registry and the
                // parked store are this module's own statics, and a torn-
                // down instance must not answer for a network that is
                // gone. (A reload re-registers/re-parks in `start`.) Only
                // THIS network's entries — the others belong to the other
                // instance.
                client::unregister_handle(&net, client::bot_presence());
                client::unpark_store(&net);
            }
        })
    }
}

#[cfg(test)]
mod installer_surface_tests {
    #[test]
    fn manifest_carries_the_irc_config_surface() {
        let encoded = serde_json::to_value(super::manifest()).expect("manifest serializes");
        let actual: Vec<veles_runtime::veles_types::Knob> =
            serde_json::from_value(encoded["knobs"].clone()).expect("module exposes knobs");
        assert_eq!(actual, crate::config_surface());
        assert!(actual.iter().any(|k| k.path == "platforms.irc.server"));
    }
}
