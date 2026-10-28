//! The override point the IRC module declares (PLAN-extension-api.md
//! E4): whether a channel line is for the bot, and what the bot reads of
//! it. Core's verdict — the mention gate (`require_mention`, per channel),
//! the bare `tcl …` convention — is the default, handed in; a plugin in
//! any tier answers instead (respond to a keyword, ignore a nick) or
//! wraps it with `next`.
//!
//! The point only decides whether the line goes on. Everything after it
//! still applies: authorization, the ACL, lockdown, the eval budget. A
//! slash command in a channel still needs a real mention (the multi-bot
//! rule), whatever the point says.

use serde_json::json;
use veles_core::services::{OnError, PointDecl};

/// Is this channel line for the bot, and what does it read of it?
pub const ADDRESSED: &str = "irc.addressed";

/// The points this module declares (its manifest's `points`).
pub fn point_decls() -> Vec<PointDecl> {
    vec![addressed_decl()]
}

pub fn addressed_decl() -> PointDecl {
    PointDecl {
        name: ADDRESSED.into(),
        summary: "whether a channel line is for the bot, and the text the bot reads of it; \
                  core's verdict (the mention gate, the bare `tcl` convention) is handed in"
            .into(),
        input: json!({
            "type": "object",
            "required": ["network", "channel", "sender", "nick", "text", "mentioned",
                         "require_mention", "core_address", "body"],
            "properties": {
                "network": {"type": "string"},
                "channel": {"type": "string"},
                "sender": {"type": "string", "description": "the sender's nick"},
                "nick": {"type": "string", "description": "the bot's nick on this network"},
                "text": {"type": "string", "description": "the line, IRC formatting stripped"},
                "mentioned": {"type": "boolean", "description": "the line names the bot"},
                "require_mention": {"type": "boolean",
                    "description": "this channel answers only lines that name the bot"},
                "core_address": {"type": "boolean", "description": "core's verdict"},
                "body": {"type": "string",
                    "description": "what the bot reads when it answers: the line without the address"}
            }
        }),
        output: json!({
            "type": "object",
            "required": ["address"],
            "properties": {
                "address": {"type": "boolean", "description": "the line goes on to the bot"},
                "text": {"type": "string", "description": "what the bot reads (default: body)"}
            }
        }),
        on_error: OnError::FallThrough,
        deadline_ms: 5_000,
    }
}

/// Declare the point, from the module's `start`: under the instance's
/// owner, so its unload takes it (one per network; the extra defaults
/// are not overrides, `services::overridden` knows its declarers).
pub fn declare() -> Result<(), String> {
    veles_core::services::declare_point(
        addressed_decl(),
        veles_core::services::handler(|input, _cx| async move {
            Ok(json!({ "address": input["core_address"], "text": input["body"] }))
        }),
    )
}

/// One channel line, as the gate sees it.
pub struct Line<'a> {
    pub network: &'a str,
    pub channel: &'a str,
    pub sender: &'a str,
    pub nick: &'a str,
    pub text: &'a str,
    pub mentioned: bool,
    pub require_mention: bool,
    /// Addressed by convention rather than by nick (a bare `tcl …`).
    pub by_convention: bool,
    /// The line without its address, when it named the bot.
    pub body: Option<&'a str>,
}

/// Does the line go on to the bot, and with what text? Core's verdict,
/// or the override's when a plugin overrides `irc.addressed` (with no
/// override nothing is built: no JSON, no call).
pub async fn decide(line: Line<'_>) -> (bool, String) {
    let core_address = !line.require_mention || line.mentioned || line.by_convention;
    let body = line.body.unwrap_or(line.text).to_string();
    if !veles_core::services::overridden(ADDRESSED) {
        return (core_address, body);
    }
    let input = json!({
        "network": line.network,
        "channel": line.channel,
        "sender": line.sender,
        "nick": line.nick,
        "text": line.text,
        "mentioned": line.mentioned,
        "require_mention": line.require_mention,
        "core_address": core_address,
        "body": body,
    });
    match veles_core::services::ask(ADDRESSED, input, None).await {
        Ok(v) => (
            v["address"].as_bool().unwrap_or(core_address),
            v["text"].as_str().map(str::to_string).unwrap_or(body),
        ),
        Err(e) => {
            tracing::warn!("irc: `{ADDRESSED}` failed ({e}); core's verdict stands");
            (core_address, body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veles_core::services::{handler, purge_owner, register_for, ServiceDecl};

    fn line<'a>(text: &'a str, mentioned: bool, body: Option<&'a str>) -> Line<'a> {
        Line {
            network: "pt-net",
            channel: "#pt",
            sender: "someone",
            nick: "veles",
            text,
            mentioned,
            require_mention: true,
            by_convention: false,
            body,
        }
    }

    /// With no override, core's verdict; an override makes the bot answer
    /// a keyword it was not addressed with, ignores a sender it was, and
    /// rewrites what it reads; its unload restores core's verdict. The
    /// override acts on its own network only, so tests beside it pass
    /// through.
    #[tokio::test]
    async fn an_override_decides_which_channel_lines_reach_the_bot() {
        let declarer = veles_core::lifecycle::owner_tag("module:irc:pt-net");
        veles_core::lifecycle::with_thread_owner(declarer, declare).expect("declare");
        assert_eq!(
            decide(line("veles: hi", true, Some("hi"))).await,
            (true, "hi".to_string())
        );
        assert!(!decide(line("weather?", false, None)).await.0);

        let owner = veles_core::lifecycle::owner_tag("module:irc-pt-override");
        register_for(
            owner,
            ServiceDecl {
                name: ADDRESSED.into(),
                description: "a test's override".into(),
                cap: None,
            },
            handler(|input, cx| async move {
                if input["network"] != "pt-net" {
                    return cx.next(input).await;
                }
                let text = input["text"].as_str().unwrap_or("");
                if text.contains("weather") {
                    return Ok(json!({"address": true, "text": format!("[keyword] {text}")}));
                }
                if input["sender"] == "noisy" {
                    return Ok(json!({"address": false}));
                }
                cx.next(input).await
            }),
        )
        .unwrap();
        assert_eq!(
            decide(line("weather?", false, None)).await,
            (true, "[keyword] weather?".to_string())
        );
        let mut noisy = line("veles: hi", true, Some("hi"));
        noisy.sender = "noisy";
        assert!(!decide(noisy).await.0);
        assert_eq!(
            decide(line("veles: hi", true, Some("hi"))).await,
            (true, "hi".to_string()),
            "the rest goes to `next`: core's verdict"
        );
        purge_owner(Some(owner));
        assert!(!decide(line("weather?", false, None)).await.0);
        purge_owner(Some(declarer));
    }
}
