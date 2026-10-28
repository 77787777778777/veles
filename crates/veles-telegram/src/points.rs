//! The override point the Telegram module declares
//! (PLAN-extension-api.md E4): whether a group message is for the bot,
//! and what the bot reads of it. Core's verdict — the group mention gate
//! (`require_mention`) — is the default, handed in; a plugin in any tier
//! answers instead or wraps it with `next`. Private chats are always for
//! the bot and are not asked about.
//!
//! The point only decides whether the message goes on. Everything after
//! it still applies: the allowed-users door, the ACL, lockdown.

use serde_json::json;
use veles_core::services::{OnError, PointDecl};

/// Is this group message for the bot, and what does it read of it?
pub const ADDRESSED: &str = "telegram.addressed";

/// The points this module declares (its manifest's `points`).
pub fn point_decls() -> Vec<PointDecl> {
    vec![addressed_decl()]
}

pub fn addressed_decl() -> PointDecl {
    PointDecl {
        name: ADDRESSED.into(),
        summary: "whether a group message is for the bot, and the text the bot reads of it; \
                  core's verdict (the mention gate) is handed in"
            .into(),
        input: json!({
            "type": "object",
            "required": ["chat_id", "sender", "sender_id", "bot_username", "text",
                         "mentioned", "require_mention", "core_address"],
            "properties": {
                "chat_id": {"type": "integer"},
                "sender": {"type": "string", "description": "the sender's display name"},
                "sender_id": {"type": "integer"},
                "bot_username": {"type": "string"},
                "text": {"type": "string"},
                "mentioned": {"type": "boolean", "description": "the text names @bot_username"},
                "require_mention": {"type": "boolean",
                    "description": "groups answer only messages that name the bot"},
                "core_address": {"type": "boolean", "description": "core's verdict"}
            }
        }),
        output: json!({
            "type": "object",
            "required": ["address"],
            "properties": {
                "address": {"type": "boolean", "description": "the message goes on to the bot"},
                "text": {"type": "string", "description": "what the bot reads (default: text)"}
            }
        }),
        on_error: OnError::FallThrough,
        deadline_ms: 5_000,
    }
}

/// Declare the point, from the module's `start` (the instance's owner:
/// its unload takes it; a reload declares it again).
pub fn declare() -> Result<(), String> {
    veles_core::services::declare_point(
        addressed_decl(),
        veles_core::services::handler(|input, _cx| async move {
            Ok(json!({ "address": input["core_address"], "text": input["text"] }))
        }),
    )
}

/// One group message, as the gate sees it.
pub struct GroupMessage<'a> {
    pub chat_id: i64,
    pub sender: &'a str,
    pub sender_id: u64,
    pub bot_username: &'a str,
    pub text: &'a str,
    pub require_mention: bool,
}

/// Does the group message go on to the bot, and with what text? Core's
/// verdict, or the override's when a plugin overrides
/// `telegram.addressed` (with no override nothing is built).
pub async fn decide(m: GroupMessage<'_>) -> (bool, String) {
    let mentioned = mentions(m.text, m.bot_username);
    let core_address = !m.require_mention || mentioned;
    if !veles_core::services::overridden(ADDRESSED) {
        return (core_address, m.text.to_string());
    }
    let input = json!({
        "chat_id": m.chat_id,
        "sender": m.sender,
        "sender_id": m.sender_id,
        "bot_username": m.bot_username,
        "text": m.text,
        "mentioned": mentioned,
        "require_mention": m.require_mention,
        "core_address": core_address,
    });
    match veles_core::services::ask(ADDRESSED, input, None).await {
        Ok(v) => (
            v["address"].as_bool().unwrap_or(core_address),
            v["text"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| m.text.to_string()),
        ),
        Err(e) => {
            tracing::warn!("telegram: `{ADDRESSED}` failed ({e}); core's verdict stands");
            (core_address, m.text.to_string())
        }
    }
}

/// Does `text` name `@username`? As Telegram reads a mention: case does
/// not matter, and the name is the whole run of letters, digits and `_`,
/// so `@velesbot_fan` and `x@velesbot.org` do not name `velesbot`.
fn mentions(text: &str, username: &str) -> bool {
    if username.is_empty() {
        return false;
    }
    let name_char = |c: char| c.is_alphanumeric() || c == '_';
    let text = text.to_lowercase();
    let needle = format!("@{}", username.to_lowercase());
    text.match_indices(&needle).any(|(at, _)| {
        !matches!(text[..at].chars().next_back(), Some(c) if name_char(c))
            && !matches!(text[at + needle.len()..].chars().next(), Some(c) if name_char(c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use veles_core::services::{handler, purge_owner, register_for, ServiceDecl};

    fn msg(text: &str) -> GroupMessage<'_> {
        GroupMessage {
            chat_id: -42,
            sender: "someone",
            sender_id: 7,
            bot_username: "velesbot",
            text,
            require_mention: true,
        }
    }

    /// A mention is the bot's username as Telegram matches one: in any
    /// case on either side (getMe reports `VelesBot`, people type
    /// `@velesbot`), and only as the whole name.
    #[tokio::test]
    async fn a_mention_ignores_case_and_must_be_the_whole_username() {
        for text in [
            "hi @velesbot",
            "@VELESBOT: hi",
            "(@VelesBot)",
            "@velesbot, hi",
            "@velesbot",
        ] {
            assert!(mentions(text, "VelesBot"), "{text:?} names @VelesBot");
            assert!(mentions(text, "velesbot"), "{text:?} names @velesbot");
        }
        for text in [
            "@velesbot_fan hi",
            "@velesbotx",
            "mail x@velesbot.org",
            "velesbot",
            "@veles",
        ] {
            assert!(
                !mentions(text, "VelesBot"),
                "{text:?} does not name @VelesBot"
            );
        }
        assert!(!mentions("@ hi", ""), "no username, no mention");

        let group = |text| GroupMessage {
            chat_id: -43,
            sender: "someone",
            sender_id: 7,
            bot_username: "VelesBot",
            text,
            require_mention: true,
        };
        assert!(decide(group("hi @velesbot")).await.0);
        assert!(!decide(group("hi @velesbot_fan")).await.0);
    }

    /// Core's verdict is the mention gate; an override answers a keyword
    /// with no mention and rewrites what the bot reads; its unload puts
    /// core's verdict back. It acts on its own chat only.
    #[tokio::test]
    async fn an_override_decides_which_group_messages_reach_the_bot() {
        let declarer = veles_core::lifecycle::owner_tag("module:telegram-pt");
        veles_core::lifecycle::with_thread_owner(declarer, declare).expect("declare");
        assert!(
            decide(msg("hi @VelesBot")).await.0,
            "a mention, the text's case aside"
        );
        assert!(!decide(msg("weather?")).await.0);

        let owner = veles_core::lifecycle::owner_tag("module:telegram-pt-override");
        register_for(
            owner,
            ServiceDecl {
                name: ADDRESSED.into(),
                description: "a test's override".into(),
                cap: None,
            },
            handler(|input, cx| async move {
                if input["chat_id"] != -42 {
                    return cx.next(input).await;
                }
                let text = input["text"].as_str().unwrap_or("");
                if text.contains("weather") {
                    return Ok(json!({"address": true, "text": format!("[keyword] {text}")}));
                }
                cx.next(input).await
            }),
        )
        .unwrap();
        assert_eq!(
            decide(msg("weather?")).await,
            (true, "[keyword] weather?".to_string())
        );
        assert!(
            !decide(msg("chatter")).await.0,
            "the rest is core's verdict"
        );
        purge_owner(Some(owner));
        assert!(!decide(msg("weather?")).await.0);
        purge_owner(Some(declarer));
    }
}
