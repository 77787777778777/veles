// An adapter/composition crate: its spawn sites are its OWN work (a
// connection task, the composition root driving a runtime), not a module
// calling into core. The M3 hop (clippy.toml) is core/frontends business.
#![allow(clippy::disallowed_methods)]

//! veles-plugin-atproto — the atproto/Bluesky platform, as a veles process
//! plugin (PLAN-plugins-installer.md, Phase 3). It was a crate linked into
//! the gateway; now it is an executable the gateway spawns and talks
//! JSON-RPC to, linking only `veles-plugin-sdk` + `veles-types`.
//!
//! Two halves in one crate, sharing one upstream connection set:
//!
//! 1. **The event filter.** Owns the bot's upstream lanes — the Jetstream
//!    v2 stream (network-wide hint lane, NO signatures; on the VPS the
//!    local jetstream-relay, which also serves courier) and the
//!    notification poll on our own PDS (the authoritative backstop).
//!    Chat is invisible to both lanes.
//!
//! 2. **The platform plugin.** Consumes the filter, gates on an explicit
//!    mention of the bot's DID, resolves the sender's relation (the door),
//!    hands the turn to the gateway (`host.inbound`), and posts the reply
//!    (`deliver`). The repo services (post.create, repo.delete_record,
//!    hosted.delete) do the byte-moving an approve/delete needs, and
//!    appview.query reads as the bot (the trigger engine's link previews),
//!    and repo.get_record reads one public record raw (what a block hides
//!    from those previews), because the PDS session lives here, not in core.
//!
//! What it needs from the gateway it asks for over the protocol: the config
//! subtree at `initialize` (`platforms.atproto`), the app password by
//! `secret.get`, a mutual's `user` grant by `pairing.grant_scoped`, and the
//! thread-room roster by `thread_room.register`. It never reads a config
//! file, an environment, or a store itself.

pub mod alttext;
pub mod config;
pub mod event;
pub mod filter;
pub mod identity;
pub mod notify;
pub mod pds;
pub mod post;
pub mod rich;

mod adapter;
pub use adapter::AtprotoAdapter;

/// The plugin's config knobs, for the manifest's `--describe` surface (the
/// setup UIs and the installer render the form generically from it).
pub use config::config_surface;

/// The platform key everywhere — ACL scopes, `[display.platforms.*]`, the
/// session key. Single-instance, so it registers the default route.
pub const PLATFORM: &str = "atproto";

/// Pick this process's rustls crypto provider (ring) before anything
/// builds a TLS config. The plugin links rustls through reqwest (the
/// PDS client) and tokio-tungstenite (Jetstream); built ALONE (the
/// static musl package) only `ring` is enabled, but built in one cargo
/// invocation with the workspace — the gateway image's `bin/` copy —
/// cargo unifies the `irc` crate's `aws-lc-rs` into the same rustls, and
/// `ClientConfig::builder()` then panics with "Could not automatically
/// determine the process-level CryptoProvider" on the first connect
/// (2026-10-09: the Jetstream task died at start, the notification poll
/// carried on). Installing explicitly makes the binary independent of
/// what else was built beside it. Idempotent: a second install is a
/// no-op `Err`, ignored.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The plugin's manifest (`--describe` prints it, the gateway reads it at
/// spawn). One process; the lease matches the old linked adapter's, so the
/// pool runs mixed versions while nodes upgrade.
pub fn manifest() -> veles_plugin_sdk::Manifest {
    use veles_plugin_sdk::proto::{Instances, PlatformDecl, SecretDecl, ServiceDecl};
    veles_plugin_sdk::Manifest {
        name: "atproto".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: veles_plugin_sdk::PROTOCOL,
        description: "atproto/Bluesky: the event filter + the public-feed platform".into(),
        instances: Instances::Single,
        lease: Some("singleton:atproto".into()),
        platforms: vec![PlatformDecl {
            key: PLATFORM.into(),
            public_feed: true,
        }],
        config_sections: vec!["platforms.atproto".into()],
        knobs: config_surface(),
        secrets: vec![SecretDecl {
            name: config::APP_PASSWORD_SECRET.into(),
            description: "the bot account's app password (createSession)".into(),
            required: true,
        }],
        // Generic repo services (R5): what is posted, deleted or taken
        // down is the caller's decision (the slopdrop module's approve
        // and delete); the plugin keeps no knowledge of any ledger. No
        // cap: internal, core and in-process modules only.
        services: vec![
            ServiceDecl {
                name: veles_plugin_sdk::proto::method::POST_CREATE.into(),
                description: "post text as the bot, threaded under a post when asked".into(),
                cap: None,
            },
            ServiceDecl {
                name: veles_plugin_sdk::proto::method::REPO_DELETE_RECORD.into(),
                description: "delete one record of the bot's repo (collection + rkey)".into(),
                cap: None,
            },
            // Which account the repo services act as: public, like the
            // reads below — a caller about to delete must be able to
            // check ownership without a grant.
            ServiceDecl {
                name: veles_plugin_sdk::proto::method::REPO_WHOAMI.into(),
                description: "the bot's session DID and handle (ownership checks)".into(),
                cap: Some(veles_plugin_sdk::proto::SERVICE_CAP_ANY.into()),
            },
            ServiceDecl {
                name: veles_plugin_sdk::proto::method::HOSTED_DELETE.into(),
                description: "take a hosted copy down: its short links and pointer records".into(),
                cap: None,
            },
            // A read, so every caller may: a script can preview a link
            // too. The writes above stay internal (core and modules):
            // third-party code must not post or delete as the bot.
            ServiceDecl {
                name: veles_plugin_sdk::proto::method::APPVIEW_QUERY.into(),
                description: "one read-only AppView query (app.bsky.*.get*) as the bot".into(),
                cap: Some(veles_plugin_sdk::proto::SERVICE_CAP_ANY.into()),
            },
            // Public data, so every caller may, as above.
            ServiceDecl {
                name: veles_plugin_sdk::proto::method::REPO_GET_RECORD.into(),
                description: "one public record by AT URI, raw, with no viewer (so what a \
                              block hides reads), or found: false when it was deleted"
                    .into(),
                cap: Some(veles_plugin_sdk::proto::SERVICE_CAP_ANY.into()),
            },
        ],
        uses: vec![
            veles_plugin_sdk::proto::UseDecl {
                name: veles_plugin_sdk::proto::method::VISION_DESCRIBE.into(),
                optional: true,
                description: "automatic alt text for images posted without it (core)".into(),
            },
            veles_plugin_sdk::proto::UseDecl {
                name: veles_plugin_sdk::proto::method::BSKY_EVENT.into(),
                optional: true,
                description: "Bluesky event triggers: likes, reposts, follows, quotes, replies \
                              and mentions to the slopdrop module"
                    .into(),
            },
        ],
        hooks: Vec::new(),
        enabled_env: None,
        required_env: None,
        points: Vec::new(),
    }
}

#[cfg(test)]
mod manifest_tests {
    /// The installer and both setup frontends must receive the same
    /// connector-owned options from --describe, not a second hand list.
    #[test]
    fn manifest_carries_the_atproto_config_surface() {
        let expected = super::config_surface();
        let encoded = serde_json::to_value(super::manifest()).expect("manifest serializes");
        let actual: Vec<veles_types::Knob> = serde_json::from_value(encoded["knobs"].clone())
            .expect("manifest exposes its config knobs");
        assert_eq!(actual, expected);
        assert!(actual.iter().any(|k| k.path == "platforms.atproto.pds_url"));
    }
}

#[cfg(test)]
mod crypto_tests {
    /// After the install a config builds whatever provider set the
    /// build enabled; without it a two-provider build panics here.
    #[test]
    fn a_tls_config_builds_after_the_provider_is_installed() {
        super::install_crypto_provider();
        super::install_crypto_provider(); // twice: idempotent
        let cfg = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        assert!(cfg.alpn_protocols.is_empty());
        assert_eq!(
            rustls::crypto::CryptoProvider::get_default().map(|p| !p.cipher_suites.is_empty()),
            Some(true),
            "a process-level provider is installed"
        );
    }
}
