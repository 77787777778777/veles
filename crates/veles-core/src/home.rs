//! The owner's home chat, per platform: the one chat the gateway's OWN
//! traffic to the owner goes to — a relayed question (`[notify]
//! ask_relay`, askrelay.rs), the owner's cron deliveries (`deliver =
//! "home"`, cron.rs), the lines a connector says about itself.
//!
//! USER, 2026-10-21: *"the telegram connector seems to be purely a
//! manhole? […] Being a pure chat interface is how it started out but
//! then it kind of drifted"*, then *"Same bot, dedicated ops chat sounds
//! good"* (docs/issues/open-items/telegram-surface.md, shape A). The
//! home chat IS that ops chat: point `[platforms.telegram.home_channel]
//! chat_id` at a private group with the bot and the DM stays a working
//! session. Nothing here knows Telegram; the gateway installs whatever
//! `[platforms.<name>.home_channel] chat_id` names, for every platform,
//! and the consumers ask by platform.
//!
//! One table per process, installed by the gateway at boot
//! (`install`); a standalone `veles cron run` installs it too, from the
//! same config, so `deliver = "home"` resolves the same way everywhere.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

fn table() -> &'static Mutex<BTreeMap<String, String>> {
    static T: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Name `chat_id` as `platform`'s home chat (replacing any earlier one).
pub fn set(platform: &str, chat_id: &str) {
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(platform.to_string(), chat_id.to_string());
}

/// Forget every home chat (a reload installs the new set after this).
pub fn clear_all() {
    table().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// `platform`'s home chat id, if one is configured.
pub fn chat(platform: &str) -> Option<String> {
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(platform)
        .cloned()
}

/// `platform:chat_id` for `platform`'s home chat — the shape a delivery
/// target and an ask's `chat_key` use.
pub fn key(platform: &str) -> Option<String> {
    chat(platform).map(|c| format!("{platform}:{c}"))
}

/// Every `[platforms.<name>.home_channel] chat_id` the config names,
/// as (platform, chat_id). A number is read as its decimal text (hermes
/// quoted the id; a hand-edited toml plausibly will not); a blank is no
/// home chat at all.
pub fn from_config(cfg: &crate::config::VelesConfig) -> Vec<(String, String)> {
    let Some(platforms) = cfg.platforms.as_mapping() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, plat) in platforms {
        let Some(name) = name.as_str() else { continue };
        let chat = plat
            .get("home_channel")
            .and_then(|h| h.get("chat_id"))
            .and_then(|v| match v {
                serde_yaml::Value::String(s) => Some(s.clone()),
                serde_yaml::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(chat) = chat {
            out.push((name.to_string(), chat));
        }
    }
    out.sort();
    out
}

/// Replace the table with what `cfg` names, and return it.
pub fn install(cfg: &crate::config::VelesConfig) -> Vec<(String, String)> {
    let homes = from_config(cfg);
    let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
    t.clear();
    for (p, c) in &homes {
        t.insert(p.clone(), c.clone());
    }
    homes
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_home_chats_come_from_every_platforms_home_channel_block() {
        let cfg: crate::config::VelesConfig = toml::from_str(
            r##"
[platforms.telegram]
enabled = true
[platforms.telegram.home_channel]
platform = "telegram"
chat_id = "-1001234"
[platforms.irc.home_channel]
chat_id = "#ops"
[platforms.matrix.home_channel]
chat_id = "   "
"##,
        )
        .expect("parses");
        assert_eq!(
            super::from_config(&cfg),
            vec![
                ("irc".to_string(), "#ops".to_string()),
                ("telegram".to_string(), "-1001234".to_string()),
            ],
            "a blank chat_id is no home chat"
        );
        // A number is the same destination as its quoted text.
        let numeric: crate::config::VelesConfig =
            toml::from_str("[platforms.telegram.home_channel]\nchat_id = -1001234\n").unwrap();
        assert_eq!(
            super::from_config(&numeric),
            vec![("telegram".to_string(), "-1001234".to_string())]
        );
        let none: crate::config::VelesConfig = toml::from_str("").unwrap();
        assert!(super::from_config(&none).is_empty());
    }

    #[test]
    fn the_table_answers_by_platform_and_as_a_target_key() {
        super::set("home-test-plat", "-42");
        assert_eq!(super::chat("home-test-plat").as_deref(), Some("-42"));
        assert_eq!(
            super::key("home-test-plat").as_deref(),
            Some("home-test-plat:-42")
        );
        assert!(super::chat("home-test-other").is_none());
    }
}
