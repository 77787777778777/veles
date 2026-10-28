//! The Telegram leg: the signer's OWN bot asking the owner.
//!
//! Four Bot API methods, over plain HTTP JSON — `getUpdates`,
//! `sendMessage`, `answerCallbackQuery`, `editMessageText` — so a test
//! can stand a stub server where Telegram is and drive the real client
//! (stub the wire, not the function).
//!
//! Only the owner's user id counts. A button press or a `yes CODE` from
//! anyone else is answered "not yours" and changes nothing; Telegram sets
//! `from.id` itself, and only this process holds the bot token. The owner
//! has to have opened a chat with the bot once (`/start`) before it can
//! write to them — Telegram's rule, not ours.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use crate::{describe, parse_reply, Decision, Desk, Event};

/// A Bot API client for one token.
#[derive(Clone)]
pub struct Bot {
    http: reqwest::Client,
    base: String,
}

impl Bot {
    pub fn new(api_url: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(70))
                .build()
                .unwrap_or_default(),
            base: format!("{}/bot{}", api_url.trim_end_matches('/'), token),
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let r: Value = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        anyhow::ensure!(
            r["ok"].as_bool() == Some(true),
            "telegram {method}: {}",
            r["description"].as_str().unwrap_or("not ok")
        );
        Ok(r["result"].clone())
    }
}

/// Run the leg until the process ends: announce every request to the
/// owner with Sign/Decline buttons, and turn the owner's answers into
/// decisions.
pub async fn run(
    bot: Bot,
    owner_id: i64,
    announce_chat: Option<i64>,
    desk: Arc<Desk>,
) -> Result<()> {
    let asked: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));
    let announce = announce_loop(
        bot.clone(),
        owner_id,
        announce_chat,
        desk.clone(),
        asked.clone(),
    );
    let answers = answer_loop(bot, owner_id, desk);
    tokio::select! {
        r = announce => r,
        r = answers => r,
    }
}

async fn announce_loop(
    bot: Bot,
    owner_id: i64,
    announce_chat: Option<i64>,
    desk: Arc<Desk>,
    asked: Arc<Mutex<HashMap<String, i64>>>,
) -> Result<()> {
    let mut events = desk.subscribe();
    // Where the request is shown: the ops chat when one is named, else
    // the owner's DM. Who may answer is `owner_id`, wherever it is shown.
    let shown_in = announce_chat.unwrap_or(owner_id);
    loop {
        let ev = match events.recv().await {
            Ok(ev) => ev,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        };
        match ev {
            Event::Ask { code, req } => {
                let keyboard = json!({ "inline_keyboard": [[
                    { "text": "Sign", "callback_data": format!("sign:{code}") },
                    { "text": "Decline", "callback_data": format!("decline:{code}") },
                ]] });
                match bot
                    .call(
                        "sendMessage",
                        json!({
                            "chat_id": shown_in,
                            "text": describe(&code, &req),
                            "reply_markup": keyboard,
                        }),
                    )
                    .await
                {
                    Ok(m) => {
                        if let Some(id) = m["message_id"].as_i64() {
                            asked
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(code, id);
                        }
                    }
                    Err(e) => tracing::warn!("signer: telegram could not ask about {code}: {e:#}"),
                }
            }
            Event::Settled { code, outcome } => {
                let mid = asked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&code);
                if let Some(mid) = mid {
                    let _ = bot
                        .call(
                            "editMessageText",
                            json!({
                                "chat_id": shown_in,
                                "message_id": mid,
                                "text": format!("veles ticket request {code}: {outcome}"),
                            }),
                        )
                        .await;
                }
            }
        }
    }
}

async fn answer_loop(bot: Bot, owner_id: i64, desk: Arc<Desk>) -> Result<()> {
    let by = format!("telegram:{owner_id}");
    let mut offset: i64 = 0;
    loop {
        let updates = match bot
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": 25,
                    "allowed_updates": ["message", "callback_query"],
                }),
            )
            .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("signer: telegram getUpdates: {e:#}");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for u in updates.as_array().into_iter().flatten() {
            if let Some(id) = u["update_id"].as_i64() {
                offset = offset.max(id + 1);
            }
            if let Some(q) = u.get("callback_query") {
                let qid = q["id"].as_str().unwrap_or_default().to_string();
                let reply = if q["from"]["id"].as_i64() != Some(owner_id) {
                    "not yours to answer".to_string()
                } else {
                    match q["data"].as_str().and_then(|d| d.split_once(':')) {
                        Some((verb, code)) => {
                            let d = if verb == "sign" {
                                Decision::Sign { by: by.clone() }
                            } else {
                                Decision::Decline { by: by.clone() }
                            };
                            if desk.decide(code, d) {
                                "done".to_string()
                            } else {
                                "that request is no longer waiting".to_string()
                            }
                        }
                        None => "?".to_string(),
                    }
                };
                let _ = bot
                    .call(
                        "answerCallbackQuery",
                        json!({ "callback_query_id": qid, "text": reply }),
                    )
                    .await;
            } else if let Some(m) = u.get("message") {
                if m["from"]["id"].as_i64() != Some(owner_id) {
                    continue;
                }
                let Some((yes, code)) = m["text"].as_str().and_then(parse_reply) else {
                    continue;
                };
                let d = if yes {
                    Decision::Sign { by: by.clone() }
                } else {
                    Decision::Decline { by: by.clone() }
                };
                let text = if desk.decide(&code, d) {
                    format!("{code}: done")
                } else {
                    format!(
                        "{code} is not waiting (waiting: {})",
                        desk.waiting().join(", ")
                    )
                };
                let _ = bot
                    .call("sendMessage", json!({ "chat_id": owner_id, "text": text }))
                    .await;
            }
        }
    }
}
