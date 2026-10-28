//! The messages a platform exchanges with the gateway.
//!
//! Moved from `veles_core::bus` (2026-10-07), which re-exports them: the
//! gateway's `GatewayBus`, the routing and the persistence stay there.

use serde::{Deserialize, Serialize};

/// A message arriving from a platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Platform key ("telegram", "ircx", …).
    pub platform: String,
    /// WHICH adapter instance of that platform this arrived on.
    ///
    /// Empty/None for a platform with one adapter. IRC sets the network
    /// name, because two networks are two connections with two audiences
    /// and a reply must go back out the way it came in — see
    /// `GatewayBus::outbound`. Deliberately separate from `platform`:
    /// the platform name is what the ACL scopes on, what
    /// `[display.platforms.*]` is keyed by and what the session key
    /// carries, and none of those should fork per network.
    #[serde(default)]
    pub route: Option<String>,
    /// Stable chat/channel identifier within the platform.
    pub chat_id: String,
    /// Platform-native message id (telegram message id, IRC tag id, …).
    ///
    /// This is the identity of the message ON ITS PLATFORM, and it is what
    /// a threaded adapter answers under: an atproto post's at-uri, so the
    /// reply lands under the post that asked. It must therefore always be
    /// the real id the platform gave, never a synthetic one — see
    /// `dedup_id` for exactly-once delivery. None = platform has no stable
    /// id.
    #[serde(default)]
    pub msg_id: Option<String>,
    /// The exactly-once key for THIS delivery, when it is not `msg_id`.
    ///
    /// One message id is claimed once in the inbound ledger, so a message
    /// the gateway re-injects into its own pipeline — a queued row handed
    /// back through `handle` to get its turn — cannot reuse the id it
    /// already spent: the claim would read as a redelivery and the answer
    /// would be dropped. Such a re-injection sets this to a fresh unique
    /// value and leaves `msg_id` holding the real platform id, so the
    /// reply still threads under the message that asked.
    ///
    /// `dedup_key` prefers this field and falls back to `msg_id`.
    /// Adapters on the wire never set it.
    #[serde(default)]
    pub dedup_id: Option<String>,
    /// Sender display name.
    pub sender: String,
    /// Verified sender identity, `platform:principal` (e.g. `irc:wrath`
    /// from the Services account, `telegram:12345` from the user id).
    /// None = the platform could not verify the sender — the ACL treats
    /// it as an unverified guest unless a mask/scope rule matches.
    /// Display `sender` is NEVER used for authorization.
    #[serde(default)]
    pub sender_id: Option<String>,
    /// What the adapter observed about the sender: wire mask, network,
    /// channel. IRC fills all three; platforms without hostmasks leave
    /// it at default and ACL resolution degrades to exact identities.
    #[serde(default)]
    pub sender_ctx: crate::SenderContext,
    /// Raw text.
    pub text: String,
    /// Rolling channel context an adapter attached (IRC observe
    /// parity): a block of the buffer's recent lines, for the model to
    /// read. The engine persists it as the ⇒ labelled `irc_context`
    /// injection — the transcript SHOWS it whatever `agent.injections`
    /// says; the knob only decides whether the model spends context on
    /// it.
    #[serde(default)]
    pub context: Option<String>,
}

impl InboundMessage {
    /// The adapter instance this arrived on, `""` when the platform has
    /// only one.
    pub fn route(&self) -> &str {
        self.route.as_deref().unwrap_or(DEFAULT_ROUTE)
    }
}

/// A reply the adapter should deliver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub chat_id: String,
    /// The platform-native id of the message this reply is answering, when
    /// the platform threads replies (atproto post at-uri, telegram message
    /// id, …). The gateway sets it from the inbound `msg_id` that started
    /// the turn, so a threaded adapter replies at the right LEVEL (the
    /// specific post that mentioned it) rather than the conversation root.
    /// None = reply to the conversation as a whole (IRC/TUI behavior, and
    /// the default for platforms with flat replies).
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Final assembled text (streaming is off by default, hermes parity).
    pub text: String,
    /// Reasoning to show (display copy — plugin-summarized when long).
    pub reasoning: Option<String>,
    /// Route that produced it, for the runtime footer.
    pub provider: String,
    pub model: String,
    /// Reply vs mid-turn progress (hermes tool_progress parity).
    #[serde(default)]
    pub kind: MessageKind,
    /// Set on `MessageKind::Ask`: the question the adapter must render
    /// (buttons on telegram, a numbered list on IRC). The answer goes
    /// back through the ask registry (`veles_core::ask`), NEVER through `send_inbound`.
    #[serde(default)]
    pub ask: Option<crate::ask::AskRequest>,
    /// Platform-bound metadata (Phase 3, D4: "delivery receipts plus
    /// `OutboundMessage.meta`"; the named `tcl_*` fields it replaced are
    /// gone since R5). Whoever shapes the message stamps keys a
    /// platform's plugin understands (a public-feed plugin reads
    /// `hosted.*` and `ledger.entry`); a plugin that does not declare the
    /// platform never sees them because the gateway routes the message to
    /// the platform's own instance. Free-form, but dotted (`<concern>.<key>`)
    /// so two concerns cannot collide.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meta: std::collections::BTreeMap<String, String>,
}

/// The `meta` keys a public-feed plugin and the module shaping a public
/// reply agree on. Two concerns: a HOSTED copy of a long result (the
/// plugin publishes the artifacts, substitutes the link for
/// [`crate::text::URL_PLACEHOLDER`] in the text, and reports the ids in
/// the receipt's `meta`), and a LEDGER entry the post belongs to (the
/// plugin never reads it; the module that stamped it files the receipt).
pub mod meta_keys {
    /// The ledger entry this reply is the post of (outbound).
    pub const LEDGER_ENTRY: &str = "ledger.entry";
    /// The markdown artifact to publish as a hosted copy (outbound).
    pub const HOSTED_MD: &str = "hosted.md";
    /// The HTML artifact to publish as a hosted copy (outbound).
    pub const HOSTED_HTML: &str = "hosted.html";
    /// Receipt: the hosted copy's public link.
    pub const HOSTED_LINK: &str = "hosted.link";
    /// Receipt: the HTML blob's CID.
    pub const HOSTED_BLOB_HTML: &str = "hosted.blob_html";
    /// Receipt: the markdown blob's CID.
    pub const HOSTED_BLOB_MD: &str = "hosted.blob_md";
    /// Receipt: the short-link id (`/t/<id>`).
    pub const HOSTED_SHORT_ID: &str = "hosted.short_id";
    /// Receipt: the owner-pointer record's rkey.
    pub const HOSTED_RECORD_RKEY: &str = "hosted.record_rkey";
    /// The rich parts of a public reply (outbound): what a Bluesky `/tcl`
    /// evaluation recorded with `bsky::link`/`tag`/`mention`/`quote`/
    /// `card`/`image`/`lang`, VALIDATED by the slopdrop module — a JSON
    /// array of flat string objects. The atproto plugin turns it into
    /// facets and embeds (`veles_atproto::rich`).
    pub const BSKY_SPEC: &str = "bsky.spec";
}

impl OutboundMessage {
    /// Plain text reply/progress with no ask payload.
    pub fn plain(chat_id: &str, text: impl Into<String>, kind: MessageKind) -> Self {
        Self {
            chat_id: chat_id.to_string(),
            reply_to: None,
            text: text.into(),
            reasoning: None,
            provider: String::new(),
            model: String::new(),
            kind,
            ask: None,
            meta: std::collections::BTreeMap::new(),
        }
    }
}

/// How the adapter should treat an outbound message.
///
/// `Reply` is the final turn output. `Progress` is mid-turn status
/// (typing indicator, "⚙ tool…" rounds) — adapters render it as an
/// editable status message and drop it when the Reply lands (hermes
/// cleanup_progress semantics). Empty-text Progress means "show the
/// typing indicator only". `Step` is an intermediate assistant message
/// from a multi-round turn: a real, standalone message that the final
/// Reply does not repeat, so a chat surface flushes progress as the
/// model produces it rather than dumping the whole turn at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    #[default]
    Reply,
    Progress,
    /// A question for the human (approval / clarify). Adapters render it
    /// with whatever affordance they have and resolve it via the ask registry (`veles_core::ask`).
    Ask,
    /// An intermediate assistant message from a multi-round turn.
    Step,
}

/// The default route: what a platform with one adapter registers under.
pub const DEFAULT_ROUTE: &str = "";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What a plugin sends: only the required fields. The rest default,
    /// exactly as an adapter that leaves them unset in Rust.
    #[test]
    fn a_minimal_inbound_message_reads() {
        let m: InboundMessage = serde_json::from_value(json!({
            "platform": "echo", "chat_id": "c1", "sender": "someone", "text": "hi"
        }))
        .unwrap();
        assert_eq!(m.route(), DEFAULT_ROUTE);
        assert!(m.msg_id.is_none() && m.sender_id.is_none() && m.context.is_none());
        assert_eq!(m.sender_ctx, crate::SenderContext::default());
    }

    #[test]
    fn an_inbound_message_round_trips_with_its_sender_context() {
        let m = InboundMessage {
            platform: "irc".into(),
            route: Some("synirc".into()),
            chat_id: "#c".into(),
            msg_id: Some("m1".into()),
            dedup_id: None,
            sender: "n".into(),
            sender_id: Some("irc:acct".into()),
            sender_ctx: crate::SenderContext {
                mask: Some("n!u@h".into()),
                network: Some("synirc".into()),
                channel: Some("#c".into()),
                platform: Some("irc".into()),
                chat_id: Some("#c".into()),
                direct: false,
                handle: None,
                post: None,
            },
            text: "t".into(),
            context: Some("ctx".into()),
        };
        let back: InboundMessage =
            serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back.route(), "synirc");
        assert_eq!(back.sender_ctx, m.sender_ctx);
        assert_eq!(back.sender_id, m.sender_id);
        assert_eq!(back.context, m.context);
    }

    /// A message from a build that still wrote the named ledger fields
    /// reads (they are ignored); the `meta` form is what carries them.
    #[test]
    fn the_old_named_ledger_fields_are_ignored_and_meta_reads() {
        let o: OutboundMessage = serde_json::from_value(json!({
            "chat_id": "c", "text": "t", "provider": "", "model": "",
            "tcl_withheld_id": "e1", "tcl_entry_id": "e1",
            "meta": {"ledger.entry": "e2"}
        }))
        .unwrap();
        assert_eq!(
            o.meta.get(meta_keys::LEDGER_ENTRY).map(String::as_str),
            Some("e2")
        );
        assert_eq!(o.kind, MessageKind::Reply);
    }
}
