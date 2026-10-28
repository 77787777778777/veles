//! Plugin configuration, read out of the `platforms.atproto` subtree the
//! gateway hands the plugin at `initialize` (its declared config section),
//! plus the app password it asks for over `secret.get` — never a file or an
//! environment the plugin reads itself (a plugin process starts with an
//! empty environment but a short allowlist, and has no secrets source).
//!
//! The knobs are listed for the manifest's `--describe` surface, so the
//! setup UIs and the installer render a form generically.

use serde_json::Value;
use veles_types::knob::Knob;

/// The Jetstream v2 public endpoint (Bluesky's, us-east; no key for the
/// live tail).
pub const JETSTREAM_V2: &str =
    "wss://jetstream.us-east.bsky.network/xrpc/network.bsky.jetstream.subscribeEvents";

/// A legacy Jetstream v1 endpoint: `jetstream{1,2}.{us-east,us-west}…/subscribe`.
fn is_jetstream_v1(url: &str) -> bool {
    url.trim_end_matches('/').ends_with("/subscribe")
}

/// The secret name the bot's app password is declared under (the plugin
/// asks the gateway for exactly this; nothing else is given out).
pub const APP_PASSWORD_SECRET: &str = "ATPROTO_APP_PASSWORD";

/// How the plugin is configured. Everything optional except what makes the
/// platform live: a PDS to post through and a bot identity to gate mentions
/// on. `from_section` returns None (platform skipped) when those are absent.
#[derive(Debug, Clone)]
pub struct AtprotoConfig {
    /// The bot account's DID (`did:plc:…`). The mention-gate matches facets
    /// against the DID, not the handle, because a handle is re-pointable.
    pub bot_did: String,
    /// The bot account's handle, for display and for building outbound
    /// mention facets back.
    pub bot_handle: String,
    /// The PDS base URL we post through and poll notifications on.
    pub pds_url: String,
    /// The operator's DID. Mutuals of this account are the `user` tier; it
    /// is pinned `owner`.
    pub operator_did: String,
    /// Co-operators (`platforms.atproto.co_operators`): further DIDs the
    /// door admits as the operator's own — an account the operator runs,
    /// such as the test account. Admission only: what a co-operator may
    /// DO is its `[acl]` rule (the test account's is an owner held by
    /// ticket, USER 2026-10-11), and its follows never make anyone a
    /// mutual — only the operator's graph does, or whoever holds a
    /// co-operator's password could admit anyone by following them.
    pub co_operators: Vec<String>,
    /// Jetstream v2 `subscribeEvents` endpoint (the network-wide hint lane).
    pub jetstream_url: String,
    /// Whether the Jetstream lane runs at all (a HINT lane — the
    /// authoritative notification poll covers what it misses).
    pub jetstream_enabled: bool,
    /// The local firehose on the co-located vlpds. PLANNED, NOT BUILT.
    pub firehose_url: Option<String>,
    /// App password for the bot session. Resolved via `secret.get`.
    pub app_password: String,
    /// Slow-poll cadence for the authoritative notification lane.
    pub notif_poll_secs: u64,
    /// What a sender who is neither the operator nor a mutual gets.
    /// "ignore" (default) drops them before a turn; "guest" opens the world.
    pub strangers_policy: String,
    /// The PDS's PUBLIC base URL, for the blob links a `/tcl` reply posts.
    pub public_url: Option<String>,
    /// Where a published `/tcl` result is READ (the viewer + short links).
    pub viewer_base: String,
    /// The owner-pointer record NSID that references a hosted `/tcl`
    /// result's blobs (so vlpds's unreferenced-blob sweep keeps them).
    /// Config: `platforms.atproto.record_nsid`; today's value is the live
    /// default.
    pub record_nsid: String,
    /// Automatic alt text (`[platforms.atproto.alt_text]`, `crate::alttext`).
    pub alt_text: crate::alttext::AltSettings,
}

impl AtprotoConfig {
    /// Build from the `platforms.atproto` subtree the gateway passed at
    /// `initialize`, plus the app password resolved over `secret.get`.
    /// None = platform skipped (not enabled, or no bot identity).
    ///
    /// `section` is the value at `platforms.atproto` (an object). The
    /// gateway already gated on `enabled`; a plugin does not re-read it —
    /// it is only spawned when the platform is on.
    pub fn from_section(section: &Value, app_password: Option<String>) -> Option<Self> {
        let get = |k: &str| section.get(k).and_then(Value::as_str).map(str::to_string);
        let app_password = app_password.filter(|p| !p.is_empty())?;

        let bot_handle = get("bot_handle")?;
        let pds_url = get("pds_url").unwrap_or_else(|| "http://127.0.0.1:2583".to_string());
        let bot_did = get("bot_did").unwrap_or_default();
        let operator_did = get("operator_did").unwrap_or_default();
        let co_operators: Vec<String> = section
            .get("co_operators")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|d| d.starts_with("did:") && *d != operator_did)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let jetstream_url = match get("jetstream_url") {
            Some(url) if is_jetstream_v1(&url) => {
                tracing::warn!(
                    "platforms.atproto.jetstream_url {url} is Jetstream v1; using {JETSTREAM_V2}"
                );
                JETSTREAM_V2.to_string()
            }
            Some(url) => url,
            None => JETSTREAM_V2.to_string(),
        };
        let jetstream_enabled = section
            .get("jetstream_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let firehose_url = get("firehose_url");
        let notif_poll_secs = section
            .get("notif_poll_secs")
            .and_then(Value::as_u64)
            .unwrap_or(30);
        let strangers_policy = get("strangers_policy").unwrap_or_else(|| "ignore".to_string());
        let public_url = get("public_url");
        let viewer_base = get("viewer_base")
            .unwrap_or_else(|| "https://7778777.online".to_string())
            .trim_end_matches('/')
            .to_string();
        // The owner-pointer record NSID. Today's value is the live default;
        // a rename is a config edit, not a rebuild.
        let record_nsid =
            get("record_nsid").unwrap_or_else(|| "online.7778777.tcl.output".to_string());
        let alt_text = crate::alttext::AltSettings::from_section(
            section.get("alt_text").unwrap_or(&Value::Null),
        );

        Some(Self {
            bot_did,
            bot_handle,
            pds_url,
            operator_did,
            co_operators,
            jetstream_url,
            jetstream_enabled,
            firehose_url,
            app_password,
            notif_poll_secs,
            strangers_policy,
            public_url,
            viewer_base,
            record_nsid,
            alt_text,
        })
    }
}

#[cfg(test)]
mod co_operator_tests {
    /// `co_operators` keeps DIDs only, and never the operator itself.
    #[test]
    fn co_operators_parse_dids_only() {
        let section = serde_json::json!({
            "bot_handle": "bot.example",
            "operator_did": "did:plc:op",
            "co_operators": ["did:plc:tester", " did:plc:other ", "not-a-did", "did:plc:op", 7],
        });
        let cfg = super::AtprotoConfig::from_section(&section, Some("pw".into())).unwrap();
        assert_eq!(cfg.co_operators, vec!["did:plc:tester", "did:plc:other"]);
        let none = super::AtprotoConfig::from_section(
            &serde_json::json!({"bot_handle": "bot.example"}),
            Some("pw".into()),
        )
        .unwrap();
        assert!(none.co_operators.is_empty());
    }
}

/// The knobs the manifest's `--describe` surface carries, so the setup UIs
/// and the installer render this platform's form generically (D4: the
/// config surface is part of what a plugin provides).
pub fn config_surface() -> Vec<Knob> {
    vec![
        Knob::blob_documented(
            "platforms.atproto.enabled",
            "false",
            "off until true; the app password is the declared secret",
        ),
        Knob::blob_documented(
            "platforms.atproto.pds_url",
            "\"http://127.0.0.1:2583\"",
            "the PDS we post through and poll; loopback on the co-located vlpds node",
        ),
        Knob::blob_documented(
            "platforms.atproto.bot_handle",
            "\"veles.example\"",
            "the bot account's handle; the mention-gate resolves its DID",
        ),
        Knob::blob_documented(
            "platforms.atproto.bot_did",
            "\"did:plc:…\"",
            "the mention-gate matches THIS, not the handle (a handle is re-pointable)",
        ),
        Knob::blob_documented(
            "platforms.atproto.operator_did",
            "\"did:plc:…\"",
            "mutuals of this account are the user tier; it is pinned owner",
        ),
        Knob::blob_documented(
            "platforms.atproto.co_operators",
            "[]",
            "DIDs admitted like the operator (an account it runs); their [acl] rule decides what \
             they may do, and their follows make no one a mutual",
        ),
        Knob::blob_documented(
            "platforms.atproto.jetstream_enabled",
            "true",
            "the network-wide HINT lane; off loses nothing the notification poll covers",
        ),
        Knob::blob_documented(
            "platforms.atproto.notif_poll_secs",
            "30",
            "authoritative listNotifications+updateSeen cadence on our own PDS",
        ),
        Knob::blob_documented(
            "platforms.atproto.public_url",
            "\"https://veles.example\"",
            "the PDS's public base; unset = /tcl results are withheld without a link",
        ),
        Knob::blob_documented(
            "platforms.atproto.viewer_base",
            "\"https://example\"",
            "the static site serving the /tcl viewer page (/tcl/) and its short links (/t/)",
        ),
        Knob::blob_documented(
            "platforms.atproto.record_nsid",
            "\"online.7778777.tcl.output\"",
            "the owner-pointer record NSID referencing a hosted /tcl result's blobs",
        ),
        Knob::blob_documented(
            "platforms.atproto.jetstream_url",
            "\"wss://jetstream.us-east.bsky.network/xrpc/network.bsky.jetstream.subscribeEvents\"",
            "the Jetstream v2 endpoint of the hint lane (a v1 /subscribe URL is replaced); \
             a node with a local jetstream-relay shadows it in [node.overrides]",
        ),
        Knob::blob_documented(
            "platforms.atproto.alt_text.enabled",
            "true",
            "automatic alt text: a bot reply under an image posted without alt (the operator; opted-in mutuals)",
        ),
        Knob::blob_documented(
            "platforms.atproto.alt_text.operator",
            "true",
            "include the operator's own posts (mutuals opt in with `/alt on`)",
        ),
        Knob::blob_documented(
            "platforms.atproto.alt_text.grace_secs",
            "120",
            "wait this long after the post, then re-read it: alt added meanwhile means no reply",
        ),
        Knob::blob_documented(
            "platforms.atproto.alt_text.per_hour",
            "30",
            "alt-text replies per hour, all authors together",
        ),
        Knob::blob_documented(
            "platforms.atproto.alt_text.per_author_per_hour",
            "10",
            "alt-text replies per hour for one author",
        ),
        Knob::blob_documented(
            "platforms.atproto.strangers_policy",
            "\"ignore\"",
            "non-mutual, non-operator senders: ignore (default, the door) or guest (the world, deferred)",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_password_means_skipped() {
        let s = json!({"bot_handle": "b"});
        assert!(AtprotoConfig::from_section(&s, None).is_none());
        assert!(AtprotoConfig::from_section(&s, Some("".into())).is_none());
    }

    #[test]
    fn no_handle_means_skipped() {
        let s = json!({});
        assert!(AtprotoConfig::from_section(&s, Some("p".into())).is_none());
    }

    #[test]
    fn a_minimal_section_fills_the_defaults() {
        let s = json!({"bot_handle": "veles.example"});
        let c = AtprotoConfig::from_section(&s, Some("pw".into())).expect("configured");
        assert_eq!(c.bot_handle, "veles.example");
        assert_eq!(c.app_password, "pw");
        assert_eq!(c.pds_url, "http://127.0.0.1:2583");
        assert!(c.jetstream_enabled);
        assert_eq!(c.jetstream_url, JETSTREAM_V2);
        assert_eq!(c.notif_poll_secs, 30);
        assert_eq!(c.strangers_policy, "ignore");
    }

    #[test]
    fn a_v1_jetstream_url_is_replaced_by_v2() {
        let s = json!({"bot_handle": "b", "jetstream_url": "wss://jetstream2.us-east.bsky.network/subscribe"});
        let c = AtprotoConfig::from_section(&s, Some("p".into())).unwrap();
        assert_eq!(c.jetstream_url, JETSTREAM_V2);
    }

    #[test]
    fn pins_override_the_defaults() {
        let s = json!({"bot_handle": "b", "pds_url": "http://127.0.0.1:9999", "notif_poll_secs": 7, "viewer_base": "https://v.example/"});
        let c = AtprotoConfig::from_section(&s, Some("p".into())).unwrap();
        assert_eq!(c.pds_url, "http://127.0.0.1:9999");
        assert_eq!(c.notif_poll_secs, 7);
        assert_eq!(c.viewer_base, "https://v.example"); // trailing slash off
    }

    #[test]
    fn the_record_nsid_defaults_to_today_and_is_overridable() {
        let s = json!({"bot_handle": "b"});
        let c = AtprotoConfig::from_section(&s, Some("p".into())).unwrap();
        assert_eq!(c.record_nsid, "online.7778777.tcl.output");
        let s2 = json!({"bot_handle": "b", "record_nsid": "com.example.tcl.output"});
        let c2 = AtprotoConfig::from_section(&s2, Some("p".into())).unwrap();
        assert_eq!(c2.record_nsid, "com.example.tcl.output");
    }
}
