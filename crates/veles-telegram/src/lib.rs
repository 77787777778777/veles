// An adapter/plugin/types crate: its spawn sites are its OWN work on its
// OWN runtime (a connection task, a test driver), not a module calling
// into core. The M3 hop (clippy.toml) is veles-core/veles-frontends
// business; when THIS crate becomes a module (R4/R6) its spawn sites are
// reviewed and the allow is dropped.
#![allow(clippy::disallowed_methods)]
//! veles-telegram — the reference Rust platform adapter (USER decision:
//! "telegram has much better tooling in rust, make this a rust plugin").
//!
//! Display semantics follow the laptop's hermes config
//! (display.platforms.telegram):
//!   streaming off      → one final message per turn
//!   show_reasoning     → reasoning displayed as a `>` blockquote prefix
//!   cleanup_progress   → no progress edits (single message, no churn)
//!   tool_progress      → tool rounds appended as short status lines
//!   show_route         → the `— provider · model` footer under the reply
//!
//! Security follows platforms.telegram + hermes conventions:
//!   TELEGRAM_BOT_TOKEN  → bot token (env/.env, never config)
//!   TELEGRAM_ALLOWED_USERS → comma-separated user ids; others ignored
//!   require_mention     → in groups, respond only when @mentioned
//!
//! Architecture: the adapter pushes `InboundMessage` into the gateway bus
//! and consumes `OutboundMessage` from its registered channel, sending
//! them back through the Bot. The agent never sees Telegram types.

pub mod markdown;
pub mod module;
pub mod points;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatAction, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Message,
};
use veles_core::ask;
use veles_frontends::gateway::{
    GatewayBus, InboundMessage, MessageKind, OutboundMessage, PlatformAdapter,
};

const TELEGRAM_MSG_LIMIT: usize = 4096;

/// Display semantics from the hermes `display.platforms.telegram` block.
#[derive(Debug, Clone)]
struct DisplayConfig {
    /// Live-edit a status message with partial deltas (hermes: off).
    streaming: bool,
    /// Prefix the reply with the reasoning blockquote (hermes: on).
    show_reasoning: bool,
    /// "off" | "verbose" — mid-turn ⚙ tool round status lines.
    tool_progress: String,
    /// Delete the progress message when the reply lands (hermes: on).
    cleanup_progress: bool,
    /// "blockquote" (default) | "code" — reasoning presentation.
    reasoning_style: String,
    /// Append `— <provider> · <model>` to the reply (hermes: on). The
    /// footer is display only; the route stays on the payload either way.
    show_route: bool,
}

impl DisplayConfig {
    fn from_config(cfg: &veles_core::config::VelesConfig) -> Self {
        let d = cfg.display.get("platforms").and_then(|p| p.get("telegram"));
        let get_bool = |k: &str, default: bool| {
            d.and_then(|v| v.get(k))
                .and_then(|v| v.as_bool())
                .unwrap_or(default)
        };
        let get_str = |k: &str, default: &str| {
            d.and_then(|v| v.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or(default)
                .to_string()
        };
        Self {
            streaming: get_bool("streaming", false),
            show_reasoning: get_bool("show_reasoning", true),
            tool_progress: get_str("tool_progress", "off"),
            cleanup_progress: get_bool("cleanup_progress", true),
            reasoning_style: get_str("reasoning_style", "blockquote"),
            show_route: get_bool("show_route", true),
        }
    }

    /// Should this Progress message be rendered as an editable status
    /// line? Tool rounds need tool_progress=verbose; streaming partials
    /// need streaming=true. Empty text (typing) is handled separately.
    fn shows_progress(&self, text: &str) -> bool {
        let is_tool_round = text.starts_with('⚙');
        if is_tool_round {
            self.tool_progress == "verbose"
        } else {
            self.streaming
        }
    }
}

pub struct TelegramAdapter {
    token: String,
    allowed_users: HashSet<u64>,
    require_mention: bool,
    /// `[platforms.telegram.home_channel] chat_id`: the owner's home chat,
    /// a DM or a private group with the bot (the ops chat, USER
    /// 2026-10-21). No @mention gate there, and said at boot.
    home_chat: Option<i64>,
    display: DisplayConfig,
}

impl TelegramAdapter {
    /// Build from the hermes-shaped config + env. Returns None when the
    /// platform is disabled or unconfigured (gateway skips it).
    pub fn try_from_config(cfg: &veles_core::config::VelesConfig) -> Option<Self> {
        let plat = cfg.platforms.get("telegram")?;
        let enabled = plat
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        // VELES_HOME-only resolution (pitfall 16, extended): a veles
        // gateway without its own token must NOT silently inherit
        // hermes' live bot token — that fights hermes' poller with 409
        // conflicts (reproduced in testing 2026-08-21). No token ⇒ no
        // adapter.
        let token = veles_core::providers::resolve_env_or_dotenv_veles_only(
            "TELEGRAM_BOT_TOKEN",
            &veles_core::config::config_dir(),
        )?;

        let allowed_users = veles_core::providers::resolve_env_or_dotenv_veles_only(
            "TELEGRAM_ALLOWED_USERS",
            &veles_core::config::config_dir(),
        )
        .map(|s| {
            s.split([',', ' ', '\n'])
                .filter_map(|t| t.trim().parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default();

        let require_mention = plat
            .get("extra")
            .and_then(|e| e.get("require_mention"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // VelesConfig models the hermes `display` block now (M5b):
        // streaming/show_reasoning/tool_progress/cleanup_progress/
        // reasoning_style all flow from display.platforms.telegram.
        let display = DisplayConfig::from_config(cfg);
        let home_chat = plat
            .get("home_channel")
            .and_then(|h| h.get("chat_id"))
            .and_then(|v| {
                // A number, or hermes' quoted spelling of one.
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
            });

        Some(Self {
            token,
            allowed_users,
            require_mention,
            home_chat,
            display,
        })
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn run(&self, bus: GatewayBus) -> Result<()> {
        self.run_with_shutdown(bus, None).await
    }
}

/// How the module's `stop` reaches the long poll (R6). The stop sets
/// `stopping` FIRST, so one that comes before the dispatcher exists is
/// not lost: the loop reads it before `get_me` and again, under the same
/// lock as the token, before `dispatch`.
#[derive(Default)]
pub struct StopSlot {
    pub token: Option<teloxide::dispatching::ShutdownToken>,
    pub stopping: bool,
}

impl TelegramAdapter {
    /// `run`, handing the dispatcher's shutdown token to `stop_slot` once
    /// the dispatcher exists (R6: the module's `stop` ends the long poll
    /// through it instead of being torn down from under it). With a slot
    /// the ctrl-c handler is NOT installed: the host's stop is the only
    /// way the module ends. Without one this is the linked adapter's
    /// `run`, ctrl-c included.
    pub async fn run_with_shutdown(
        &self,
        bus: GatewayBus,
        stop_slot: Option<Arc<Mutex<StopSlot>>>,
    ) -> Result<()> {
        let stopping = |slot: &Option<Arc<Mutex<StopSlot>>>| {
            slot.as_ref()
                .is_some_and(|s| s.lock().unwrap_or_else(|e| e.into_inner()).stopping)
        };
        if stopping(&stop_slot) {
            return Ok(());
        }
        let mut bot = Bot::new(&self.token);
        // teloxide's own variable, as its `Bot::from_env` reads it: a
        // local Bot API server, or the stub the pool rehearsal puts where
        // api.telegram.org is (scripts/g-three-nodes.sh). An absolute URL
        // joined onto the default IS that URL.
        if let Ok(api) = std::env::var("TELOXIDE_API_URL") {
            let api = api.trim();
            if !api.is_empty() {
                let url = bot
                    .api_url()
                    .join(api)
                    .map_err(|e| anyhow::anyhow!("TELOXIDE_API_URL {api:?}: {e}"))?;
                tracing::info!("telegram: Bot API at {url} (TELOXIDE_API_URL)");
                bot = bot.set_api_url(url);
            }
        }
        let me = bot
            .get_me()
            .await
            .map_err(|e| anyhow::anyhow!("telegram get_me failed: {e}"))?;
        let bot_username = me.user.username.clone().unwrap_or_default().to_lowercase();
        tracing::info!("telegram adapter online as @{bot_username}");

        // The door, stated once at boot — and stated as the one actually
        // in force. Two gates sit in front of this adapter and they live
        // in different files; an operator should not have to read both
        // to know who can reach their bot.
        match boot_trust_line(&veles_core::acl::live_config(), self.allowed_users.len()) {
            TrustBoot::Info(line) => tracing::info!("{line}"),
            TrustBoot::Warn(line) => tracing::warn!("{line}"),
        }
        if let Some(line) = home_boot_line(self.home_chat) {
            tracing::info!("{line}");
        }

        // The "/" menu in the chat composer, from the SAME generated
        // table every surface's /help prints (RM4.4) — before this the
        // menu was whatever Telegram remembered from older bots, which
        // is how a command can exist and be undiscoverable here.
        // Capped at Telegram's 32-command menu limit, descriptions at
        // its 256-char ceiling.
        {
            use teloxide::types::BotCommand;
            // Descriptions are translated for the active locale: this
            // menu is text a user reads, in the language they set.
            let cmds: Vec<BotCommand> = veles_frontends::commands::help_entries_localized(
                veles_frontends::commands::Surface::Gateway,
            )
            .into_iter()
            .take(32)
            .map(|(name, desc)| {
                BotCommand::new(
                    name.split(' ').next().unwrap_or(&name).to_string(),
                    desc.chars().take(256).collect::<String>(),
                )
            })
            .collect();
            if let Err(e) = bot.set_my_commands(cmds).await {
                // Not fatal: the bot works, only the autocomplete menu
                // is missing.
                tracing::warn!("telegram set_my_commands failed: {e}");
            }
        }

        // Outbound: gateway → telegram. MessageKind splits the traffic:
        // Reply = final turn output; Progress = typing indicator (empty
        // text) or editable status (tool rounds / streaming partials).
        // One progress message per chat, edited in place and deleted
        // when the reply lands (hermes cleanup_progress semantics).
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<OutboundMessage>();
        bus.register_outbound("telegram", out_tx);

        let bot2 = bot.clone();
        let display = self.display.clone();
        tokio::spawn(async move {
            use std::collections::HashMap;
            use teloxide::types::MessageId;
            let mut progress_msgs: HashMap<i64, MessageId> = HashMap::new();

            while let Some(msg) = out_rx.recv().await {
                let chat_id = match msg.chat_id.parse::<i64>() {
                    Ok(c) => ChatId(c),
                    Err(_) => continue,
                };

                match msg.kind {
                    MessageKind::Progress => {
                        if msg.text.is_empty() {
                            // Typing indicator at turn start.
                            let _ = bot2.send_chat_action(chat_id, ChatAction::Typing).await;
                            continue;
                        }
                        if !display.shows_progress(&msg.text) {
                            continue;
                        }
                        // Edit-or-create the status message. Plain text —
                        // status lines don't carry markdown (hermes sends
                        // them unformatted too).
                        let body = format!("… {}", msg.text);
                        let body = if body.len() > 512 {
                            format!("{}…", &body[..body.floor_char_boundary(512)])
                        } else {
                            body
                        };
                        match progress_msgs.get(&chat_id.0) {
                            Some(mid) => {
                                let _ = bot2.edit_message_text(chat_id, *mid, body).await;
                            }
                            None => {
                                if let Ok(sent) = bot2.send_message(chat_id, body).await {
                                    progress_msgs.insert(chat_id.0, sent.id);
                                }
                            }
                        }
                    }
                    MessageKind::Reply => {
                        // cleanup_progress: drop the status message first.
                        if display.cleanup_progress {
                            if let Some(mid) = progress_msgs.remove(&chat_id.0) {
                                let _ = bot2.delete_message(chat_id, mid).await;
                            }
                        }
                        if let Err(e) = deliver(&bot2, &msg, &display).await {
                            tracing::error!("telegram deliver failed: {e}");
                        }
                    }
                    // An intermediate assistant message in a multi-round
                    // turn. Delivery is the point: the user sees each
                    // step as it's produced, not one dump at the end.
                    MessageKind::Step => {
                        if let Err(e) = deliver(&bot2, &msg, &display).await {
                            tracing::error!("telegram deliver failed: {e}");
                        }
                    }
                    // Approval / clarify: buttons, so answering is one
                    // tap and never looks like a new user turn.
                    MessageKind::Ask => {
                        let Some(req) = msg.ask.clone() else { continue };
                        let questions = req.all_questions();
                        let multi_q = questions.len() > 1;
                        // One row per question when there are several:
                        // tap-to-toggle for multi (or multi-question)
                        // asks + a Done row; one button per choice,
                        // answering instantly, otherwise (the legacy
                        // one-tap path).
                        let toggle_mode = multi_q || req.multi;
                        let kb: InlineKeyboardMarkup = if toggle_mode {
                            tg_ask_keyboard(&req, &[])
                        } else {
                            InlineKeyboardMarkup::new(req.choices.iter().map(|c| {
                                vec![InlineKeyboardButton::callback(
                                    c.label.clone(),
                                    format!("{}|a|{}", req.id, c.key),
                                )]
                            }))
                        };
                        // A challenge's page, as a photo above the question
                        // (PLAN-pool-round.md Part E); the relay link in its
                        // detail is where it can be clicked.
                        if let Some(png) = req.image_png() {
                            let photo =
                                teloxide::types::InputFile::memory(png).file_name("challenge.png");
                            if let Err(e) = bot2.send_photo(chat_id, photo).await {
                                tracing::warn!("telegram challenge photo failed: {e}");
                            }
                        }
                        let body = fit_telegram(&tg_ask_body(&req));
                        if let Err(e) = bot2.send_message(chat_id, body).reply_markup(kb).await {
                            tracing::error!("telegram ask send failed: {e}");
                        }
                    }
                }
            }
        });

        // Inbound: telegram → gateway bus.
        let allowed = self.allowed_users.clone();
        let require_mention = self.require_mention;
        let bus2 = bus.clone();
        let bot3 = bot.clone();
        let handler = move |msg: Message| {
            let bus = bus2.clone();
            let allowed = allowed.clone();
            let bot_username = bot_username.clone();
            let bot = bot.clone();
            async move {
                tracing::debug!(
                    "telegram: update id={} from={:?} text={:?}",
                    msg.id.0,
                    msg.from.as_ref().map(|u| u.id.0),
                    msg.text()
                );
                let chat = &msg.chat;
                let Some(user) = msg.from.as_ref() else {
                    return ResponseResult::Ok(());
                };
                if !allowed.is_empty() && !allowed.contains(&user.id.0) {
                    return ResponseResult::Ok(());
                }

                // Media first (photo/document/voice/audio/video/sticker):
                // download to VELES_HOME/media and describe it to the
                // model instead of silently dropping the message.
                let mut text = msg.text().map(str::to_string).unwrap_or_default();
                if text.is_empty() {
                    match describe_and_download_media(&bot, &msg).await {
                        Some(note) => {
                            text = match msg.caption() {
                                Some(cap) => format!("{cap}\n\n{note}"),
                                None => note,
                            };
                        }
                        None => return ResponseResult::Ok(()),
                    }
                } else if msg.photo().is_some()
                    || msg.document().is_some()
                    || msg.voice().is_some()
                    || msg.audio().is_some()
                    || msg.video().is_some()
                    || msg.sticker().is_some()
                {
                    // Text + attached media: describe the attachment too.
                    if let Some(note) = describe_and_download_media(&bot, &msg).await {
                        text = format!("{text}\n\n{note}");
                    }
                }

                // Is this the answer to a question the agent is parked
                // on? Resolve it HERE — routing it through the inbound
                // bus would queue it behind the very turn that's waiting
                // for it (and the buttons are the normal path anyway).
                // BEFORE the group gate: a typed answer is not a message
                // for the model, it never needed an @mention, and in the
                // home chat it is the main thing the owner types (USER
                // 2026-10-21: the ops chat and the chat sessions split
                // cleanly; a plain reply there answers, and is no turn).
                let chat_key = format!("telegram:{}", chat.id.0);
                if let Some(id) = ask::resolve_for_chat(
                    &chat_key,
                    &text,
                    Some(&format!("telegram:{}", user.id.0)),
                ) {
                    tracing::info!("telegram: typed reply answered ask {id}");
                    return ResponseResult::Ok(());
                }

                // Group mention gate (hermes require_mention parity).
                // M2 scope: typed @mentions; entity-offset parsing later.
                // Core's verdict, unless a plugin overrides
                // `telegram.addressed` (PLAN-extension-api.md E4).
                let is_group = !chat.is_private();
                if is_group {
                    let sender_name = user.first_name.clone();
                    let (address, read) = crate::points::decide(crate::points::GroupMessage {
                        chat_id: chat.id.0,
                        sender: &sender_name,
                        sender_id: user.id.0,
                        bot_username: &bot_username,
                        text: &text,
                        require_mention,
                    })
                    .await;
                    if !address {
                        return ResponseResult::Ok(());
                    }
                    text = read;
                }

                // `/todos`, `/checkpoints`, `/restore` used to be
                // intercepted HERE, pre-bus, with private
                // implementations. RM4.3 moved them into the shared
                // dispatcher, which the gateway answers for every
                // adapter — one implementation, one drift surface less.

                let sender = match &user.last_name {
                    Some(l) => format!("{} {}", user.first_name, l),
                    None => user.first_name.clone(),
                };
                bus.send_inbound(InboundMessage {
                    platform: "telegram".into(),
                    route: None,
                    chat_id: chat.id.0.to_string(),
                    // Telegram message id: unique per chat, strictly
                    // increasing. This is the exactly-once key — the
                    // gateway dedup window drops redelivered updates.
                    msg_id: Some(msg.id.0.to_string()),
                    dedup_id: None,
                    sender,
                    // Verified telegram user id is the identity.
                    sender_id: Some(format!("telegram:{}", user.id.0)),
                    // No hostmask exists here, but the rest of the
                    // context does — and it is what lets an `[acl]`
                    // rule or a sandbox assignment scope on "this
                    // connector" or "this chat" without knowing IRC's
                    // spelling of either.
                    sender_ctx: veles_core::acl::SenderContext {
                        platform: Some("telegram".into()),
                        chat_id: Some(chat.id.0.to_string()),
                        direct: chat.is_private(),
                        ..Default::default()
                    },
                    text,
                    context: None,
                });
                ResponseResult::Ok(())
            }
        };

        // Inline-keyboard answers arrive as callback queries, NOT
        // messages. teloxide's `repl` only wires Update::filter_message
        // (everything else hits a no-op default handler), so approvals
        // need a real Dispatcher — this is why the repl is gone.
        let allowed_cq = self.allowed_users.clone();
        let cq_handler = move |bot: Bot, q: CallbackQuery| {
            let allowed = allowed_cq.clone();
            async move {
                if !allowed.is_empty() && !allowed.contains(&q.from.id.0) {
                    return ResponseResult::Ok(());
                }
                let Some(data) = q.data.clone() else {
                    return ResponseResult::Ok(());
                };
                // WHO pressed it, before anything else. The typed-reply
                // path above has always asked `may_answer` (an ask bound
                // to a sender is answerable by that sender or an owner);
                // the BUTTON path once resolved unconditionally, so in any
                // chat where the prompt was visible, whoever tapped first
                // decided the owner's approval. The allowlist above covers
                // a single-user box and nothing in a group. `tap_action`
                // applies the gate to EVERY data shape.
                let presser = q.from.id.0;
                let action =
                    tap_action(&data, ask::is_open, |id| button_answer_allowed(id, presser));
                let (id, verb) = match action {
                    TapAction::Ignore | TapAction::Header => {
                        let _ = bot.answer_callback_query(q.id.clone()).await;
                        return ResponseResult::Ok(());
                    }
                    TapAction::Refuse => {
                        let _ = bot
                            .answer_callback_query(q.id.clone())
                            .text("not yours to answer")
                            .await;
                        return ResponseResult::Ok(());
                    }
                    TapAction::Gone => {
                        // Answered elsewhere, timed out, or its asker
                        // stopped: say so, and take the dead buttons off.
                        let _ = bot
                            .answer_callback_query(q.id.clone())
                            .text("no longer waiting")
                            .await;
                        if let Some(m) = q.message.as_ref() {
                            let _ = bot.edit_message_reply_markup(m.chat().id, m.id()).await;
                        }
                        return ResponseResult::Ok(());
                    }
                    TapAction::Answer { id, choice } => {
                        let ok = ask::resolve(&id, ask::AskAnswer::choice(&choice));
                        let _ = bot
                            .answer_callback_query(q.id.clone())
                            .text(if ok { "recorded" } else { "expired" })
                            .await;
                        if let Some(m) = q.message.as_ref() {
                            let note = if ok {
                                format!("→ {choice}")
                            } else {
                                "→ expired (no longer waiting)".to_string()
                            };
                            let _ = bot.edit_message_reply_markup(m.chat().id, m.id()).await;
                            let _ = bot.send_message(m.chat().id, note).await;
                        }
                        return ResponseResult::Ok(());
                    }
                    TapAction::Toggle { id, qk, ck } => (id, Some((qk, ck))),
                    TapAction::Done { id } => (id, None),
                };
                // Toggle and Done: the selection lives in this adapter's
                // table (presentation, not engine state — the ask itself
                // is still one registry row until Done or a typed reply).
                let Some(open) = ask::open_all().into_iter().find(|r| r.id == id) else {
                    let _ = bot
                        .answer_callback_query(q.id.clone())
                        .text("expired")
                        .await;
                    return ResponseResult::Ok(());
                };
                if let Some((qk, ck)) = verb {
                    let changed = {
                        let mut t = TG_TOGGLES.lock().unwrap_or_else(|e| e.into_inner());
                        // Bounded by the asks still open: one resolved by a
                        // typed reply, a timeout or another surface never
                        // passes through Done, so its row goes here.
                        t.retain(|k, _| ask::is_open(k));
                        let e = t.entry(id.clone()).or_default();
                        apply_toggle(&open, e, &qk, &ck)
                    };
                    if changed {
                        if let Some(m) = q.message.as_ref() {
                            let kb = tg_ask_keyboard(&open, &tg_toggles(&id));
                            let _ = bot
                                .edit_message_reply_markup(m.chat().id, m.id())
                                .reply_markup(kb)
                                .await;
                        }
                    }
                    let _ = bot.answer_callback_query(q.id.clone()).await;
                    return ResponseResult::Ok(());
                }
                let sel = tg_toggles(&id);
                let Some((answer, shown)) = done_answer(&open, &sel) else {
                    // Nothing ticked: an accidental Done must not answer.
                    let _ = bot
                        .answer_callback_query(q.id.clone())
                        .text("nothing selected — tap a choice first, or reply by typing")
                        .await;
                    return ResponseResult::Ok(());
                };
                TG_TOGGLES
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                let ok = ask::resolve(&id, answer);
                let _ = bot
                    .answer_callback_query(q.id.clone())
                    .text(if ok { "recorded" } else { "expired" })
                    .await;
                if let Some(m) = q.message.as_ref() {
                    let note = if ok {
                        format!("→ {shown}")
                    } else {
                        "→ expired (no longer waiting)".to_string()
                    };
                    let _ = bot.edit_message_reply_markup(m.chat().id, m.id()).await;
                    let _ = bot.send_message(m.chat().id, note).await;
                }
                ResponseResult::Ok(())
            }
        };

        // Long-poll until cancelled.
        let builder = Dispatcher::builder(
            bot3,
            dptree::entry()
                .branch(Update::filter_message().endpoint(handler))
                .branch(Update::filter_callback_query().endpoint(cq_handler)),
        );
        let mut dispatcher = match &stop_slot {
            Some(_) => builder.build(),
            None => builder.enable_ctrlc_handler().build(),
        };
        if let Some(slot) = &stop_slot {
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot.stopping {
                return Ok(());
            }
            slot.token = Some(dispatcher.shutdown_token());
        }
        dispatcher.dispatch().await;
        Ok(())
    }
}

/// Inbound media → download + model-facing descriptor.
///
/// Returns a note like `[photo 800×600 saved to /home/.../media/…jpg]`
/// to prepend/append to the message text, or None when the message
/// carries no media (or the download failed — the message still flows
/// as text when a caption exists; silent drops are gone).
async fn describe_and_download_media(bot: &Bot, msg: &Message) -> Option<String> {
    // (kind label, file_id, extension hint, extra descriptor)
    let media: Option<(&str, String, Option<String>, String)> = if let Some(sizes) = msg.photo() {
        // Largest size = last element.
        sizes.last().map(|p| {
            (
                "photo",
                p.file.id.clone(),
                Some("jpg".into()),
                format!("{}x{}", p.width, p.height),
            )
        })
    } else if let Some(d) = msg.document() {
        let ext = d
            .file_name
            .as_ref()
            .and_then(|n| n.rsplit('.').next())
            .map(str::to_string);
        let name = d.file_name.clone().unwrap_or_else(|| "document".into());
        Some(("document", d.file.id.clone(), ext, name))
    } else if let Some(v) = msg.voice() {
        Some((
            "voice",
            v.file.id.clone(),
            Some("ogg".into()),
            format!("{}s", v.duration.seconds()),
        ))
    } else if let Some(a) = msg.audio() {
        let title = a.title.clone().unwrap_or_else(|| "audio".into());
        Some(("audio", a.file.id.clone(), Some("mp3".into()), title))
    } else if let Some(v) = msg.video() {
        Some((
            "video",
            v.file.id.clone(),
            Some("mp4".into()),
            format!("{}x{}", v.width, v.height),
        ))
    } else if let Some(s) = msg.sticker() {
        Some((
            "sticker",
            s.file.id.clone(),
            Some("webp".into()),
            "sticker".into(),
        ))
    } else {
        None
    };

    let (kind, file_id, ext, detail) = media?;

    // Resolve the server-side path, then download.
    let file = match bot.get_file(file_id).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("telegram: get_file failed for {kind}: {e}");
            return Some(format!("[{kind} attached but could not fetch it]"));
        }
    };

    let dir = veles_core::config::media_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("telegram: cannot create media dir: {e}");
        return Some(format!("[{kind} attached but storage unavailable]"));
    }
    let ext = ext
        .or_else(|| file.path.rsplit('.').next().map(str::to_string))
        .unwrap_or_else(|| "bin".into());
    let name = format!(
        "{}_{}_{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        msg.id.0,
        kind,
        ext
    );
    let dest_path = dir.join(&name);
    let mut dest = match tokio::fs::File::create(&dest_path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("telegram: cannot create {}: {e}", dest_path.display());
            return Some(format!("[{kind} attached but storage unavailable]"));
        }
    };
    match bot.download_file(&file.path, &mut dest).await {
        Ok(()) => Some(format!(
            "[{kind} {detail} saved to {}]",
            dest_path.display()
        )),
        Err(e) => {
            tracing::warn!("telegram: download failed for {kind}: {e}");
            let _ = std::fs::remove_file(&dest_path);
            Some(format!("[{kind} attached but download failed]"))
        }
    }
}

/// The `— provider · model` line a reply is signed with, or nothing.
///
/// Nothing when the route cannot be named (an ask, a system line), and
/// nothing when `display.platforms.telegram.show_route` is off — the knob
/// that did not exist until 2026-09-10, when this footer was the one part
/// of a reply no config could suppress. Split out of `deliver` so the
/// gating is unit-tested without a wire; IRC's twin is
/// `client::outbound_body`.
fn route_footer(display: &DisplayConfig, msg: &OutboundMessage) -> String {
    if display.show_route && !msg.provider.is_empty() {
        format!("\n— {} · {}", msg.provider, msg.model)
    } else {
        String::new()
    }
}

/// Deliver one assembled reply with hermes display semantics.
async fn deliver(bot: &Bot, msg: &OutboundMessage, display: &DisplayConfig) -> Result<()> {
    let chat_id = ChatId(msg.chat_id.parse()?);

    // Reasoning prefix (show_reasoning parity). Style from display
    // config: blockquote (`> line`) or code fence (``` ... ```).
    let mut body = String::new();
    if display.show_reasoning {
        if let Some(r) = &msg.reasoning {
            if !r.is_empty() {
                match display.reasoning_style.as_str() {
                    "code" => {
                        body.push_str("```\n");
                        body.push_str(r);
                        body.push_str("\n```\n\n");
                    }
                    _ => {
                        for line in r.lines() {
                            body.push('>');
                            body.push_str(line);
                            body.push('\n');
                        }
                        body.push('\n');
                    }
                }
            }
        }
    }
    body.push_str(&msg.text);

    // Outbound media (hermes MEDIA: convention): extract marker lines
    // before formatting, send the files, deliver remaining text below.
    let (media_paths, remaining) = extract_media(&body);
    send_media_items(bot, chat_id, &media_paths).await;

    // Runtime-metadata footer (hermes /footer parity, minimal).
    body = format!("{remaining}{}", route_footer(display, msg));
    let body = body.trim().to_string();
    if body.is_empty() && !media_paths.is_empty() {
        return Ok(()); // media-only reply
    }

    // Telegram's 4096-char cap → split at line boundaries, THEN format
    // each chunk as MarkdownV2 (formatting must run per-chunk: escaping
    // changes byte counts, and a placeholder split mid-way would corrupt
    // protected regions). Fallback on parse rejection: hermes-style —
    // retry the same chunk as plain text with markers stripped.
    for chunk in split_message(&body, TELEGRAM_MSG_LIMIT) {
        let formatted = markdown::format_message(&chunk);
        let res = bot
            .send_message(chat_id, formatted)
            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
            .await;
        if res.is_err() {
            // MarkdownV2 rejected (malformed from model output) — send
            // clean plain text instead of losing the message entirely.
            let plain = markdown::strip_mdv2(&markdown::format_message(&chunk));
            let _ = bot
                .send_message(chat_id, plain)
                .await
                .map_err(|e| anyhow::anyhow!("send_message failed: {e}"))?;
        }
    }
    Ok(())
}

/// Split `MEDIA:` markers (hermes convention) out of a reply body —
/// shared implementation in veles_core::media (2026-08-10 backport).
pub fn extract_media(text: &str) -> (Vec<String>, String) {
    veles_core::media::extract_media(text)
}

/// Media kind by extension, for choosing the right send method.
fn media_kind(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" => "photo",
        "mp3" | "wav" | "flac" | "m4a" => "audio",
        "ogg" | "opus" => "voice",
        "mp4" | "webm" | "mov" | "mkv" => "video",
        _ => "document",
    }
}

/// Send files to the chat. Missing/unreadable files degrade to a text
/// note rather than failing the whole reply.
///
/// This is telegram's DELIVERY method — the connector half of the `MEDIA:`
/// convention, and the reason an upload is the right answer here and a link is
/// the right answer on IRC (which has no upload). It is deliberately not a
/// gate: which paths may be handed over at all was decided upstream by
/// `veles_core::media::enforce_outbound`, at the one seam that knows the
/// principal. A role check here would be a second ACL to keep in step with the
/// first, and would still be wrong for every caller that is not the gateway.
pub async fn send_media_items(bot: &Bot, chat_id: ChatId, paths: &[String]) {
    for path in paths {
        if !std::path::Path::new(path).is_file() {
            tracing::warn!("telegram: MEDIA path not found: {path}");
            let _ = bot
                .send_message(chat_id, format!("[media not found: {path}]"))
                .await;
            continue;
        }
        let file = teloxide::types::InputFile::file(path.clone());
        let res = match media_kind(path) {
            "photo" => bot.send_photo(chat_id, file).await.map(|_| ()),
            "audio" => bot.send_audio(chat_id, file).await.map(|_| ()),
            "voice" => bot.send_voice(chat_id, file).await.map(|_| ()),
            "video" => bot.send_video(chat_id, file).await.map(|_| ()),
            _ => bot.send_document(chat_id, file).await.map(|_| ()),
        };
        if let Err(e) = res {
            tracing::warn!("telegram: media send failed for {path}: {e}");
            let _ = bot
                .send_message(chat_id, format!("[media send failed: {path}]"))
                .await;
        }
    }
}

pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if cur.len() + line.len() + 1 > limit && !cur.is_empty() {
            out.push(cur);
            cur = String::new();
        }
        // pathological single line: hard-cut
        if line.len() > limit {
            let mut rest = line;
            while rest.len() > limit {
                let cut = rest.floor_char_boundary(limit);
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            cur.push_str(rest);
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push("(empty reply)".into());
    }
    out
}

/// The `[platforms.telegram]` and `[display.platforms.telegram]` keys this
/// adapter reads, with each one's default.
///
/// Hand-listed for the reason the IRC one is: both blocks arrive as
/// `serde_yaml::Value` and are read key by key. `every_key_the_reader_reads_is_listed`
/// and `the_display_defaults_are_the_ones_in_the_reader` grep this file for
/// the readers' literals and their inline defaults, so a key added here
/// without a listing — or a default moved in the reader — fails the suite.
///
/// Documented rather than written: an adapter nobody configured should not
/// be materialized by a fill, and `enabled = false` sitting in a config
/// reads like a decision somebody made.
pub fn config_surface() -> Vec<veles_core::config_surface::Knob> {
    use veles_core::config_surface::Knob;
    vec![
        Knob::blob_documented(
            "platforms.telegram.enabled",
            "false",
            "off until true, and TELEGRAM_BOT_TOKEN must be in .env",
        ),
        Knob::blob_documented(
            "platforms.telegram.extra.require_mention",
            "true",
            "unmentioned group messages are watched, not answered",
        ),
        Knob::blob_documented(
            "display.platforms.telegram.streaming",
            "false",
            "edit one status message with partial deltas",
        ),
        Knob::blob_documented(
            "display.platforms.telegram.show_reasoning",
            "true",
            "prefix the reply with the reasoning block",
        ),
        Knob::blob_documented(
            "display.platforms.telegram.tool_progress",
            "\"off\"",
            "\"off\" | \"verbose\": the ⚙ tool round lines",
        ),
        Knob::blob_documented(
            "display.platforms.telegram.cleanup_progress",
            "true",
            "delete the status message once the reply lands",
        ),
        Knob::blob_documented(
            "display.platforms.telegram.reasoning_style",
            "\"blockquote\"",
            "how the reasoning block is rendered",
        ),
        Knob::blob_documented(
            "display.platforms.telegram.show_route",
            "true",
            "the `— provider · model` footer under the reply",
        ),
    ]
}

/// May this telegram user answer that ask by tapping its button?
///
/// The typed-reply path has asked this since S15 (`resolve_for_chat`
/// takes a responder and refuses one the ask is not bound to). The
/// BUTTON path did not: it called `ask::resolve` on whatever arrived,
/// so in any chat where the prompt was visible, whoever tapped first
/// decided the owner's approval. `TELEGRAM_ALLOWED_USERS` happened to
/// cover that on a single-user box and covers nothing in a group.
fn button_answer_allowed(ask_id: &str, presser: u64) -> bool {
    veles_core::ask::may_answer(
        ask_id,
        veles_core::ask::Decider::sender(Some(&format!("telegram:{presser}"))),
    )
}

/// Toggle state for multi/multi-question asks: ask id → selected
/// (qk, ck) pairs, in tap order. Presentation state, not engine state —
/// the ask itself stays one registry row until Done (or a typed reply)
/// resolves it. Rows for asks that are no longer open are pruned on
/// every toggle, so the table holds at most the open asks.
type TickTable = std::collections::HashMap<String, Vec<(String, String)>>;
static TG_TOGGLES: std::sync::LazyLock<Mutex<TickTable>> =
    std::sync::LazyLock::new(|| Mutex::new(TickTable::new()));

fn tg_toggles(id: &str) -> Vec<(String, String)> {
    TG_TOGGLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
        .unwrap_or_default()
}

/// What a tapped inline button asks for, decided BEFORE any side effect
/// — and with the presser check applied to every shape, so no data
/// format is a way around it.
#[derive(Debug, PartialEq)]
enum TapAction {
    /// Not data this adapter writes.
    Ignore,
    /// The presser may not answer this ask.
    Refuse,
    /// The ask is not open any more: answered, timed out, or its asker
    /// went away. Not a refusal — told as one, the owner tapping a sync
    /// question a stopped phone had asked read "not yours to answer"
    /// (2026-10-13).
    Gone,
    /// A question's header row: not a button.
    Header,
    /// One-tap answer (single-select asks).
    Answer { id: String, choice: String },
    /// Tick/untick one choice of one question.
    Toggle { id: String, qk: String, ck: String },
    /// Commit the ticked set.
    Done { id: String },
}

/// Parse callback data. Shapes: `id|a|choice`, `id|t|qk:ck`, `id|done|`,
/// `id|hdr|qk`, and the two-part `id|choice` from before the toggle
/// protocol, which is read as `id|a|choice` — through the same gate.
fn tap_action(
    data: &str,
    open: impl Fn(&str) -> bool,
    allowed: impl Fn(&str) -> bool,
) -> TapAction {
    let parts: Vec<&str> = data.splitn(3, '|').collect();
    let (id, verb, arg) = match parts.as_slice() {
        [id, choice] => (*id, "a", *choice),
        [id, verb, arg] => (*id, *verb, *arg),
        _ => return TapAction::Ignore,
    };
    if id.is_empty() || !matches!(verb, "a" | "t" | "done" | "hdr") {
        return TapAction::Ignore;
    }
    if !open(id) {
        return TapAction::Gone;
    }
    if !allowed(id) {
        return TapAction::Refuse;
    }
    let id = id.to_string();
    match verb {
        "hdr" => TapAction::Header,
        "a" => TapAction::Answer {
            id,
            choice: arg.to_string(),
        },
        "t" => {
            // A single-question ask's toggle may carry just the choice.
            let (qk, ck) = arg.split_once(':').unwrap_or(("q1", arg));
            TapAction::Toggle {
                id,
                qk: qk.to_string(),
                ck: ck.to_string(),
            }
        }
        _ => TapAction::Done { id },
    }
}

/// Tick or untick (qk, ck) in `sel`. A single-select question keeps one
/// pick (radio). A pair the ask does not offer is refused (false), so
/// made-up callback data cannot plant keys in the answer.
fn apply_toggle(
    req: &ask::AskRequest,
    sel: &mut Vec<(String, String)>,
    qk: &str,
    ck: &str,
) -> bool {
    let questions = req.all_questions();
    let Some(q) = questions.iter().find(|q| q.key == qk) else {
        return false;
    };
    if !q.choices.iter().any(|c| c.key == ck) {
        return false;
    }
    let pair = (qk.to_string(), ck.to_string());
    if let Some(i) = sel.iter().position(|p| *p == pair) {
        sel.remove(i);
    } else {
        if !q.multi {
            sel.retain(|(k, _)| k != qk);
        }
        sel.push(pair);
    }
    true
}

/// The answer Done commits, and the line that records it in the chat
/// (choice LABELS, not keys). None when nothing is ticked: an accidental
/// Done answers nothing and the ask stays open.
fn done_answer(
    req: &ask::AskRequest,
    sel: &[(String, String)],
) -> Option<(ask::AskAnswer, String)> {
    if sel.is_empty() {
        return None;
    }
    let questions = req.all_questions();
    let label = |qk: &str, ck: &str| -> String {
        questions
            .iter()
            .find(|q| q.key == qk)
            .and_then(|q| q.choices.iter().find(|c| c.key == ck))
            .map(|c| c.label.clone())
            .unwrap_or_else(|| ck.to_string())
    };
    let keys_for = |qk: &str| -> Vec<String> {
        sel.iter()
            .filter(|(k, _)| k == qk)
            .map(|(_, c)| c.clone())
            .collect()
    };
    if questions.len() <= 1 {
        let keys = keys_for(&questions[0].key);
        let shown = keys
            .iter()
            .map(|c| label(&questions[0].key, c))
            .collect::<Vec<_>>();
        let answer = ask::AskAnswer {
            selected: keys,
            ..Default::default()
        };
        return Some((answer, shown.join(", ")));
    }
    let mut per = std::collections::BTreeMap::new();
    let mut shown = Vec::new();
    for q in &questions {
        let keys = keys_for(&q.key);
        if keys.is_empty() {
            continue;
        }
        let labels = keys.iter().map(|c| label(&q.key, c)).collect::<Vec<_>>();
        shown.push(format!("{}: {}", q.prompt, labels.join(", ")));
        per.insert(
            q.key.clone(),
            ask::AskAnswer {
                selected: keys,
                ..Default::default()
            },
        );
    }
    Some((ask::AskAnswer::from_per_question(per), shown.join("; ")))
}

/// The text of an ask message. The buttons carry choices; this says
/// what they cannot. A multi-question ask lists every question and, when
/// one of them is free text, says how to TYPE the answers — a button
/// cannot carry text, and the typed form ("1: 2", "3: your answer",
/// one line per question) is the one that names its questions. Only a
/// clarify invites "your own answer": an approval or failover answers
/// with one of its choices, never prose.
/// Telegram refuses a message over 4096 UTF-16 units ("message is too
/// long"), and a refused ASK is a question nobody can answer: a relayed
/// approval was lost exactly that way (2026-10-05 21:18). Keep the head
/// and the tail — the question and its choices sit at the ends — and say
/// where the whole text is.
fn fit_telegram(body: &str) -> String {
    const MAX: usize = 4000; // UTF-16 units, with margin under 4096
    if body.encode_utf16().count() <= MAX {
        return body.to_string();
    }
    let note = "\n…(cut to fit Telegram — the full text is on the desktop)…\n";
    let budget = MAX - note.encode_utf16().count();
    let (head_budget, tail_budget) = (budget * 2 / 3, budget - budget * 2 / 3);
    let mut used = 0;
    let head: String = body
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= head_budget
        })
        .collect();
    let mut used = 0;
    let mut tail: Vec<char> = body
        .chars()
        .rev()
        .take_while(|c| {
            used += c.len_utf16();
            used <= tail_budget
        })
        .collect();
    tail.reverse();
    format!("{head}{note}{}", tail.into_iter().collect::<String>())
}

fn tg_ask_body(req: &ask::AskRequest) -> String {
    let questions = req.all_questions();
    if questions.len() > 1 {
        let free: Vec<usize> = questions
            .iter()
            .enumerate()
            .filter(|(_, q)| q.choices.is_empty())
            .map(|(i, _)| i + 1)
            .collect();
        let mut body = format!(
            "❓ {} questions — tap your choices, then Done",
            questions.len()
        );
        for (i, q) in questions.iter().enumerate() {
            body.push_str(&format!("\n\n{}. {}", i + 1, q.prompt));
            for (j, c) in q.choices.iter().enumerate() {
                body.push_str(&format!("\n  {}) {}", j + 1, c.label));
            }
            if q.multi && !q.choices.is_empty() {
                body.push_str("\n  (pick any)");
            }
            if q.choices.is_empty() {
                body.push_str("\n  (free text)");
            }
        }
        if let Some(n) = free.first() {
            body.push_str(&format!(
                "\n\n(buttons cannot carry text: reply by typing, one line per question, \
e.g. \"1: 2\" and \"{n}: your answer\")"
            ));
        } else {
            body.push_str("\n\n(or reply by typing, one line per question, e.g. \"1: 2\")");
        }
        return body;
    }
    let mut body = format!("❓ {}", req.prompt);
    if let Some(d) = &req.detail {
        if !d.trim().is_empty() {
            body.push_str(&format!("\n\n{}", d.trim()));
        }
    }
    if req.choices.is_empty() {
        body.push_str("\n\n(reply with your answer)");
    } else if req.kind == ask::AskKind::Clarify && req.multi {
        body.push_str("\n\n(tap several, then Done — or reply like \"1,3\")");
    } else if req.kind == ask::AskKind::Clarify {
        body.push_str("\n\n(tap a button — or type your own answer)");
    }
    body
}

/// The toggle-mode keyboard, reflecting the current selection set: ○
/// unselected / ● selected, one header row per question, Done at the
/// bottom.
fn tg_ask_keyboard(
    req: &ask::AskRequest,
    sel: &[(String, String)],
) -> teloxide::types::InlineKeyboardMarkup {
    use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
    let questions = req.all_questions();
    let multi_q = questions.len() > 1;
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for q in &questions {
        if multi_q {
            rows.push(vec![InlineKeyboardButton::callback(
                format!("❓ {}", q.prompt),
                format!("{}|hdr|{}", req.id, q.key),
            )]);
        }
        for c in &q.choices {
            let on = sel.iter().any(|(qk, ck)| qk == &q.key && ck == &c.key);
            rows.push(vec![InlineKeyboardButton::callback(
                format!("{} {}", if on { "●" } else { "○" }, c.label),
                format!("{}|t|{}:{}", req.id, q.key, c.key),
            )]);
        }
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "✓ Done".to_string(),
        format!("{}|done|", req.id),
    )]);
    InlineKeyboardMarkup::new(rows)
}

/// The boot line about the home chat, if one is configured — said beside
/// the door, for the same reason: one line to know where the gateway's
/// own traffic to the owner goes.
pub fn home_boot_line(home_chat: Option<i64>) -> Option<String> {
    home_chat.map(|id| {
        format!(
            "telegram: the home chat is {id}: relayed questions, cron output with deliver=home \
             and the signer's tickets go there; a plain reply there answers the waiting question and starts no turn"
        )
    })
}

/// How loudly the boot line about the door should be said.
///
/// A variant rather than a `tracing::Level` so the two call sites stay
/// static callsites, and so a test can assert on the SEVERITY without
/// capturing a log. Same shape as veles-irc's `TrustBoot`, for the same
/// reason: the posture an operator assumes and the posture in force
/// have to be the same sentence.
#[derive(Debug, PartialEq, Eq)]
pub enum TrustBoot {
    /// The boundary is in force; naming it is bookkeeping.
    Info(String),
    /// Something is wider than an operator would assume.
    Warn(String),
}

impl TrustBoot {
    pub fn line(&self) -> &str {
        match self {
            TrustBoot::Info(s) | TrustBoot::Warn(s) => s,
        }
    }
}

/// The one line the adapter prints about who may reach it.
///
/// USER, 2026-09-16: *"is it still resolving people as guests on
/// telegram?"* — a question nothing in the logs answered, because this
/// adapter had two overlapping doors (`TELEGRAM_ALLOWED_USERS` in
/// `.env`, `[acl]` in `config.toml`) and announced neither. IRC has
/// said its boundary at every boot since the "OPEN to every account"
/// warning turned out to be a lie on any box with an `[acl]` section;
/// this is the same line for the other connector.
pub fn boot_trust_line(acl: &veles_core::acl::AclConfig, allowlist_entries: usize) -> TrustBoot {
    use veles_core::acl::{Principal, StrangerPolicy};
    let filter = match allowlist_entries {
        0 => "TELEGRAM_ALLOWED_USERS is empty".to_string(),
        1 => "1 TELEGRAM_ALLOWED_USERS entry narrows it further".to_string(),
        n => format!("{n} TELEGRAM_ALLOWED_USERS entries narrow it further"),
    };
    if !acl.is_configured() {
        return TrustBoot::Warn(format!(
            "telegram: no [acl] section — the ACL is dormant, so every sender the allowlist \
             admits resolves as OWNER with full capabilities ({filter}); declare [acl] owner \
             to narrow that"
        ));
    }
    match acl.strangers_on(Some("telegram")) {
        StrangerPolicy::Ignore => TrustBoot::Info(format!(
            "telegram: the door is [acl] — only identities an [acl] rule names may speak \
             here; everyone else is ignored at the gateway, no turn and no reply ({filter})"
        )),
        StrangerPolicy::Role(r) => TrustBoot::Info(format!(
            "telegram: the door is [acl] — a sender no rule names is {} here by \
             [acl.strangers] rather than by default_role ({filter})",
            r.as_str()
        )),
        StrangerPolicy::DefaultRole => {
            // What an unlisted sender actually becomes, asked of the ACL
            // rather than read off default_role: an entry scoped to this
            // platform can answer differently.
            let ctx = veles_core::acl::SenderContext {
                platform: Some("telegram".into()),
                direct: true,
                ..Default::default()
            };
            let role = Principal::resolve_ctx(Some("telegram:0"), &ctx, acl).role;
            let line = format!(
                "telegram: the door is [acl] — a sender no rule names still gets a TURN, as \
                 {} ({filter}); set [acl.strangers] telegram = \"ignore\" to admit only \
                 identities an [acl] rule names",
                role.as_str()
            );
            // An open door AND no allowlist is wider than an operator
            // assumes from "allowed users" living in their .env.
            if allowlist_entries == 0 {
                TrustBoot::Warn(line)
            } else {
                TrustBoot::Info(line)
            }
        }
    }
}

/// The two vars the adapter reads, for the `.env` surface.
///
/// Both resolve VELES_HOME-only, on purpose: a gateway without its own
/// token must not inherit hermes' live bot and fight its poller for
/// updates. Neither has a default to show — one is a secret and the other
/// is a filter that is off until somebody writes ids into it.
pub fn env_surface() -> Vec<veles_core::config_surface::EnvKnob> {
    use veles_core::config_surface::EnvKnob;
    vec![
        EnvKnob::without_default(
            "TELEGRAM_BOT_TOKEN",
            "without it the adapter does not start; never inherited from ~/.hermes/.env",
        ),
        EnvKnob::without_default(
            "TELEGRAM_ALLOWED_USERS",
            "comma or space separated ids; unset answers anyone who can reach the bot",
        ),
    ]
}

#[cfg(test)]
mod tests {
    // ── the home chat as the ops chat (USER 2026-10-21) ───────────────

    /// The owner's home chat may be a private group with the bot. What
    /// the owner says there is always addressed to the bot: no @mention
    /// gate in the ops chat, whatever `require_mention` says for groups.
    /// Said once at boot, like the door: an operator reads one line to
    /// know where the gateway's own traffic to them goes.
    #[test]
    fn the_home_chat_is_said_at_boot() {
        use super::home_boot_line;
        assert!(home_boot_line(None).is_none());
        let line = home_boot_line(Some(-1001234)).expect("a line");
        assert!(line.contains("-1001234"), "{line}");
        assert!(
            line.contains("deliver=home") && line.contains("starts no turn"),
            "{line}"
        );
    }

    /// An over-long ask still reaches Telegram, cut to fit, with both
    /// ends kept (the relayed approval lost on 10-05 was one such).
    #[test]
    fn an_over_long_ask_is_cut_to_fit_telegram() {
        let body = format!(
            "[approval needed] run this?\n{}\nchoices: once / deny",
            "😀x".repeat(5000)
        );
        let fit = super::fit_telegram(&body);
        assert!(
            fit.encode_utf16().count() <= 4096,
            "{}",
            fit.encode_utf16().count()
        );
        assert!(fit.starts_with("[approval needed] run this?"));
        assert!(fit.ends_with("choices: once / deny"));
        assert!(fit.contains("cut to fit Telegram"));
        let short = "[question] ok?";
        assert_eq!(
            super::fit_telegram(short),
            short,
            "a short ask is untouched"
        );
    }
    use super::*;

    // ── the door, said out loud at boot ──────────────────────────────

    fn acl(yaml: &str) -> veles_core::acl::AclConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn a_shut_door_says_only_named_identities_may_speak() {
        let line = boot_trust_line(
            &acl("default_role: guest\nstrangers: {telegram: ignore}\nowner: [\"telegram:1\"]"),
            1,
        );
        assert!(matches!(line, TrustBoot::Info(_)), "{line:?}");
        assert!(
            line.line().contains("only identities an [acl] rule names"),
            "{}",
            line.line()
        );
        assert!(
            line.line().contains("no turn and no reply"),
            "the consequence, not just the rule: {}",
            line.line()
        );
    }

    #[test]
    fn an_open_door_with_no_allowlist_is_a_warning_and_names_the_role() {
        // The posture that prompted all of this: nothing said anywhere
        // that a stranger who found the bot would get a billed turn.
        let line = boot_trust_line(&acl("default_role: guest\nowner: [\"telegram:1\"]"), 0);
        assert!(
            matches!(line, TrustBoot::Warn(_)),
            "wider than an operator assumes: {line:?}"
        );
        assert!(line.line().contains("still gets a TURN"), "{}", line.line());
        assert!(line.line().contains("guest"), "{}", line.line());
        assert!(
            line.line().contains("[acl.strangers]"),
            "and the line says how to shut it: {}",
            line.line()
        );
    }

    #[test]
    fn an_allowlist_narrows_the_same_open_door_back_to_info() {
        let line = boot_trust_line(&acl("default_role: guest\nowner: [\"telegram:1\"]"), 2);
        assert!(matches!(line, TrustBoot::Info(_)), "{line:?}");
        assert!(
            line.line().contains("2 TELEGRAM_ALLOWED_USERS entries"),
            "{}",
            line.line()
        );
    }

    #[test]
    fn a_dormant_acl_is_the_loudest_case() {
        // No [acl] at all: every sender the allowlist admits is OWNER.
        let line = boot_trust_line(&acl("{}"), 1);
        assert!(matches!(line, TrustBoot::Warn(_)), "{line:?}");
        assert!(line.line().contains("OWNER"), "{}", line.line());
    }

    // ── the button is not a way around the ask's bindings ────────────

    #[test]
    fn only_the_sender_an_ask_is_bound_to_may_tap_its_buttons() {
        use veles_core::ask::{AskChoice, AskKind, AskRequest};
        // A CURATED acl, pinned for the duration. Without one this test
        // measures the machine: `may_answer` consults the live section
        // for its owner override, and a DORMANT acl resolves every
        // sender as owner — so it passed on the developer's configured
        // box and failed in the nix sandbox, where there is no config
        // file at all. The dormant answer is correct behaviour; it is
        // just not the behaviour this test is about.
        veles_core::ask::set_acl_snapshot_for_tests(Some(
            serde_yaml::from_str("default_role: guest\nowner: [\"telegram:1\"]").unwrap(),
        ));
        // Ids outside anything the pinned acl names, so the owner
        // override cannot make this pass for the wrong reason.
        let req = AskRequest::new(
            "s-button",
            AskKind::Approval,
            "run this?",
            vec![AskChoice {
                key: "yes".into(),
                label: "Yes".into(),
            }],
        );
        let _leg =
            veles_core::ask::register(&req, Some("telegram:900099"), Some("telegram:900001"))
                .expect("a fresh ask registers");

        assert!(
            button_answer_allowed(&req.id, 900001),
            "the sender it is bound to decides it"
        );
        assert!(
            !button_answer_allowed(&req.id, 900002),
            "anyone else in the chat must not decide the owner's approval by tapping first"
        );
        veles_core::ask::cancel(&req.id);
        veles_core::ask::set_acl_snapshot_for_tests(None);
    }

    /// The two-part callback ("askid|choice", the one-tap format from
    /// before the toggle protocol) goes through the SAME gate as every
    /// other tap. The parity round answered it before asking who
    /// pressed, so in a group anyone could decide the owner's approval
    /// with an old button — or with callback data they made up.
    #[test]
    fn every_tap_shape_is_gated_on_who_pressed_it() {
        let nobody = |_: &str| false;
        let owner = |_: &str| true;
        let open = |_: &str| true;
        let gone = |_: &str| false;
        for data in [
            "ask1-ab|once",
            "ask1-ab|a|once",
            "ask1-ab|t|q1:c1",
            "ask1-ab|done|",
        ] {
            assert_eq!(tap_action(data, open, nobody), TapAction::Refuse, "{data}");
            // A question that is not open any more is said to be so,
            // to anyone: not refused as if it were someone else's.
            assert_eq!(tap_action(data, gone, owner), TapAction::Gone, "{data}");
        }
        assert_eq!(
            tap_action("ask1-ab|once", open, owner),
            TapAction::Answer {
                id: "ask1-ab".into(),
                choice: "once".into()
            }
        );
        assert_eq!(
            tap_action("ask1-ab|t|q2:c3", open, owner),
            TapAction::Toggle {
                id: "ask1-ab".into(),
                qk: "q2".into(),
                ck: "c3".into()
            }
        );
        assert_eq!(
            tap_action("ask1-ab|done|", open, owner),
            TapAction::Done {
                id: "ask1-ab".into()
            }
        );
        assert_eq!(tap_action("garbage", open, owner), TapAction::Ignore);
    }

    fn two_question_ask() -> veles_core::ask::AskRequest {
        use veles_core::ask::{AskChoice, AskKind, AskQuestion, AskRequest};
        AskRequest::multi_question(
            "s-tg",
            AskKind::Clarify,
            vec![
                AskQuestion::new(
                    "q1",
                    "theme?",
                    vec![
                        AskChoice::new("c1", "synthwave"),
                        AskChoice::new("c2", "amber"),
                    ],
                ),
                AskQuestion::new(
                    "q2",
                    "art?",
                    vec![AskChoice::new("c1", "yes"), AskChoice::new("c2", "no")],
                )
                .with_multi(true),
                AskQuestion::new("q3", "name?", vec![]),
            ],
        )
    }

    /// A toggle flips one (question, choice) pair; a single-select
    /// question keeps one pick (radio); a pair the ask does not offer is
    /// ignored rather than recorded.
    #[test]
    fn toggles_follow_the_questions_rules_and_ignore_unknown_keys() {
        let req = two_question_ask();
        let mut sel = Vec::new();
        assert!(apply_toggle(&req, &mut sel, "q1", "c1"));
        assert!(apply_toggle(&req, &mut sel, "q1", "c2"));
        assert_eq!(sel, vec![("q1".to_string(), "c2".to_string())], "radio");
        assert!(apply_toggle(&req, &mut sel, "q2", "c1"));
        assert!(apply_toggle(&req, &mut sel, "q2", "c2"));
        assert_eq!(sel.len(), 3, "multi keeps both: {sel:?}");
        assert!(apply_toggle(&req, &mut sel, "q2", "c1"));
        assert_eq!(sel.len(), 2, "a second tap unticks: {sel:?}");
        assert!(!apply_toggle(&req, &mut sel, "q1", "c9"), "no such choice");
        assert!(
            !apply_toggle(&req, &mut sel, "q7", "c1"),
            "no such question"
        );
        assert_eq!(sel.len(), 2);
    }

    /// Done with nothing ticked answers nothing (the ask stays open);
    /// otherwise every ticked question is answered and the note names
    /// the LABELS, not "q1:c2".
    #[test]
    fn done_answers_what_was_ticked_and_nothing_when_nothing_was() {
        let req = two_question_ask();
        assert!(done_answer(&req, &[]).is_none());
        let sel = vec![
            ("q1".to_string(), "c2".to_string()),
            ("q2".to_string(), "c1".to_string()),
            ("q2".to_string(), "c2".to_string()),
        ];
        let (a, note) = done_answer(&req, &sel).unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c2"]);
        assert_eq!(a.per_question["q2"].selected, vec!["c1", "c2"]);
        assert!(!a.per_question.contains_key("q3"));
        assert!(
            note.contains("amber") && note.contains("yes") && note.contains("no"),
            "{note}"
        );
        assert!(!note.contains("c2"), "{note}");
    }

    /// The message says what the buttons cannot: a free-text question
    /// in a multi-question ask names the typed form, and an approval is
    /// never invited to "type your own answer" (prose is not an answer
    /// to an approval).
    #[test]
    fn the_ask_text_says_what_the_buttons_cannot() {
        let body = tg_ask_body(&two_question_ask());
        assert!(body.contains("3 questions"), "{body}");
        assert!(
            body.contains("3. name?") && body.contains("(free text)"),
            "{body}"
        );
        assert!(body.contains("\"3: your answer\""), "{body}");
        let approval = veles_core::ask::AskRequest::new(
            "s",
            veles_core::ask::AskKind::Approval,
            "run it?",
            veles_core::ask::approval_choices(true),
        );
        let body = tg_ask_body(&approval);
        assert!(!body.contains("your own answer"), "{body}");
    }

    fn display(streaming: bool, tool_progress: &str) -> DisplayConfig {
        DisplayConfig {
            streaming,
            show_reasoning: true,
            tool_progress: tool_progress.to_string(),
            cleanup_progress: true,
            reasoning_style: "blockquote".into(),
            show_route: true,
        }
    }

    #[test]
    fn progress_tool_round_needs_verbose() {
        let d = display(false, "off");
        assert!(!d.shows_progress("⚙ terminal…"));
        let d = display(false, "verbose");
        assert!(d.shows_progress("⚙ terminal…"));
    }

    #[test]
    fn progress_stream_partial_needs_streaming() {
        let d = display(false, "off");
        assert!(!d.shows_progress("partial delta text"));
        let d = display(true, "off");
        assert!(d.shows_progress("partial delta text"));
    }

    #[test]
    fn split_message_line_boundaries() {
        let text = "line one\nline two\nline three";
        let parts = split_message(text, 20);
        assert!(parts.iter().all(|p| p.len() <= 20));
        assert!(parts.join("").contains("line one"));
        assert!(parts.join("").contains("line three"));
    }

    #[test]
    fn split_message_hard_cut_pathological() {
        let text = "a".repeat(100);
        let parts = split_message(&text, 30);
        assert!(parts.iter().all(|p| p.len() <= 30));
        assert_eq!(parts.join("").len(), 100);
    }

    #[test]
    fn split_message_empty() {
        let parts = split_message("", 4096);
        assert_eq!(parts, vec!["(empty reply)".to_string()]);
    }

    #[test]
    fn display_defaults_from_missing_config() {
        let cfg = veles_core::config::VelesConfig::default();
        let d = DisplayConfig::from_config(&cfg);
        assert!(!d.streaming);
        assert!(d.show_reasoning);
        assert_eq!(d.tool_progress, "off");
        assert!(d.cleanup_progress);
        assert_eq!(d.reasoning_style, "blockquote");
        assert!(d.show_route, "the footer is on unless the block says off");
    }

    /// The key has to reach the FIELD, not merely appear in the reader. A
    /// `get_bool("show_route", true)` whose result is dropped still compiles,
    /// and the surface pin would still pass — that pin compares the literal
    /// in the reader with the literal on the surface, and both would be
    /// right about a knob that does nothing.
    ///
    /// Mutation: write `show_route: true` in `from_config` instead of the
    /// read and this fails while every surface test stays green.
    #[test]
    fn show_route_off_in_the_block_turns_the_field_off() {
        let cfg = veles_core::config::VelesConfig {
            display: serde_yaml::from_str(
                "platforms:\n  telegram:\n    show_route: false\n    streaming: true\n",
            )
            .expect("parses"),
            ..veles_core::config::VelesConfig::default()
        };
        let d = DisplayConfig::from_config(&cfg);
        assert!(!d.show_route, "`show_route = false` must reach the field");
        assert!(d.streaming, "…and its neighbours still parse");
    }

    /// The footer itself. `show_route` is the only thing that suppresses it
    /// besides an unnamed route, and off means the reply carries no trailing
    /// line, no em-dash and no blank line where the line was. `deliver`
    /// trims its body, but a footer that is off should not depend on a trim
    /// to disappear.
    ///
    /// Mutation: drop `display.show_route &&` from `route_footer` and the off
    /// half fails.
    #[test]
    fn show_route_gates_the_route_footer() {
        let mut msg = OutboundMessage::plain("42", "the answer", MessageKind::Reply);
        msg.provider = "custom:ali".into();
        msg.model = "qwen3.8-flash".into();

        let on = display(false, "off");
        assert_eq!(
            route_footer(&on, &msg),
            "\n— custom:ali · qwen3.8-flash",
            "on is today's behaviour, unchanged"
        );

        let off = DisplayConfig {
            show_route: false,
            ..display(false, "off")
        };
        assert_eq!(
            route_footer(&off, &msg),
            "",
            "off leaves the reply with nothing appended"
        );
    }

    /// A reply that cannot name its route gets no footer — an ask or a system
    /// line has an empty provider, and a bare em-dash would be a line about
    /// nothing.
    #[test]
    fn no_route_no_footer() {
        let msg = OutboundMessage::plain("42", "the answer", MessageKind::Reply);
        assert_eq!(route_footer(&display(false, "off"), &msg), "");
    }

    #[test]
    fn extract_media_single_marker() {
        let (paths, rest) = extract_media("here is your chart\nMEDIA:/tmp/chart.png\nsee above");
        assert_eq!(paths, vec!["/tmp/chart.png"]);
        assert!(rest.contains("here is your chart"));
        assert!(rest.contains("see above"));
        assert!(!rest.contains("MEDIA:"));
    }

    #[test]
    fn extract_media_multiple_and_spaces() {
        let (paths, rest) = extract_media("MEDIA:/tmp/a.jpg\nMEDIA:/tmp/b with spaces.mp4\ntext");
        assert_eq!(paths, vec!["/tmp/a.jpg", "/tmp/b with spaces.mp4"]);
        assert_eq!(rest.trim(), "text");
    }

    #[test]
    fn extract_media_no_markers() {
        let (paths, rest) = extract_media("plain reply\nno media here");
        assert!(paths.is_empty());
        assert_eq!(rest, "plain reply\nno media here");
    }

    #[test]
    fn extract_media_empty_marker_ignored() {
        let (paths, rest) = extract_media("MEDIA:\nreal text");
        assert!(paths.is_empty());
        assert_eq!(rest.trim(), "real text");
    }

    #[test]
    fn media_kind_by_extension() {
        assert_eq!(media_kind("/x/y.png"), "photo");
        assert_eq!(media_kind("/x/y.JPG"), "photo");
        assert_eq!(media_kind("/x/y.mp3"), "audio");
        assert_eq!(media_kind("/x/y.ogg"), "voice");
        assert_eq!(media_kind("/x/y.mp4"), "video");
        assert_eq!(media_kind("/x/y.pdf"), "document");
        assert_eq!(media_kind("/x/noext"), "document");
    }
}

/// The pins on [`config_surface`], both directions, against the readers'
/// own source.
#[cfg(test)]
mod surface_tests {
    use super::*;

    fn fn_body(name: &str) -> String {
        let src = include_str!("lib.rs");
        let start = src
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("`fn {name}` is gone; the surface pin needs it"));
        let body = &src[start..];
        let mut depth = 0usize;
        for (i, ch) in body.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return body[..i + 1].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces in {name}")
    }

    fn call_literals(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(i) = rest.find("(\"") {
            let after = &rest[i + 2..];
            match after.find('"') {
                Some(j) => {
                    out.push(after[..j].to_string());
                    rest = &after[j..];
                }
                None => break,
            }
        }
        out
    }

    /// The `get_bool("k", v)` / `get_str("k", "v")` pairs, as `(key, TOML)`.
    fn reader_defaults() -> Vec<(String, String)> {
        let body = fn_body("from_config");
        let mut out = Vec::new();
        for (call, quoted) in [("get_bool(", false), ("get_str(", true)] {
            let mut rest = body.as_str();
            while let Some(i) = rest.find(call) {
                let after = &rest[i + call.len()..];
                let (key, tail) = after
                    .strip_prefix('"')
                    .and_then(|a| a.split_once('"'))
                    .expect("get_* takes a quoted key");
                let value = if quoted {
                    let v = tail
                        .split_once('"')
                        .and_then(|(_, b)| b.split('"').next())
                        .expect("get_str takes a quoted default");
                    format!("\"{v}\"")
                } else {
                    tail.split_once(',')
                        .map(|(_, b)| b.split(')').next().unwrap_or("").trim().to_string())
                        .expect("get_bool takes a default")
                };
                out.push((key.to_string(), value));
                rest = tail;
            }
        }
        out
    }

    #[test]
    fn a_typed_answer_is_taken_before_the_mention_gate() {
        // USER 2026-10-21, after the first evening in the ops chat: "I
        // thought we'd split it cleanly between ops and chat sessions."
        // A plain reply in the home chat answers the question waiting
        // there; it must not have to pass the group gate first (it would
        // need an @mention there), and it must never become a turn. So
        // the typed-answer check comes BEFORE the gate, for every chat.
        let body = fn_body("run_with_shutdown");
        let answer = body
            .find("ask::resolve_for_chat(")
            .expect("the typed-answer check");
        let gate = body.find("crate::points::decide(").expect("the group gate");
        assert!(answer < gate, "the typed answer is checked after the gate");
    }

    #[test]
    fn every_key_the_reader_reads_is_listed() {
        // The adapter's own surface, plus the `[platforms.telegram]` keys
        // core lists because core reads them too: `home_channel.chat_id`
        // is the ask relay's destination (gateway.rs `owner_home_chat`)
        // and the home table's (`veles_core::home`), and since 2026-10-21
        // the adapter reads it as well, for the @mention gate. One
        // listing, in core; this test takes it from there.
        let own = config_surface();
        let core = veles_core::config_surface::platform_surface();
        let blob: Vec<&str> = own
            .iter()
            .chain(core.iter())
            .filter_map(|k| k.path.strip_prefix("platforms.telegram."))
            .collect();
        // `telegram` is the platform name, `extra` and `home_channel`
        // containers; the uppercase literals are env vars, which live on
        // the env surface.
        let read: Vec<String> = call_literals(&fn_body("try_from_config"))
            .into_iter()
            .filter(|l| !l.contains('_') || l.chars().all(|c| !c.is_uppercase()))
            .filter(|l| l != "telegram" && l != "extra" && l != "home_channel")
            .collect();
        for key in &read {
            assert!(
                blob.iter()
                    .any(|b| b == key || b.ends_with(&format!(".{key}"))),
                "`{key}` is read but not on the surface: {blob:?}"
            );
        }
        for key in &blob {
            let leaf = key.rsplit('.').next().unwrap_or(key);
            assert!(
                read.iter().any(|r| r == leaf),
                "`{key}` is on the surface but nothing reads it"
            );
        }
    }

    #[test]
    fn the_display_defaults_are_the_ones_in_the_reader() {
        let surface = config_surface();
        let pairs = reader_defaults();
        // Pinned so a sixth display key cannot arrive in the reader without
        // a listing: `show_route` was the last one added, and this number is
        // the reason its surface entry exists.
        assert_eq!(pairs.len(), 6, "six display keys: {pairs:?}");
        for (key, rendered) in &pairs {
            let path = format!("display.platforms.telegram.{key}");
            let knob = surface
                .iter()
                .find(|k| k.path == path)
                .unwrap_or_else(|| panic!("{path} is read but not listed"));
            assert_eq!(
                knob.default.as_deref(),
                Some(rendered.as_str()),
                "{path}: the surface and the reader disagree about the default"
            );
        }
    }

    /// The `.env` half: a var read and not documented is a knob nobody can
    /// find, and one documented that nothing reads is a line that does
    /// nothing.
    #[test]
    fn the_env_surface_is_the_vars_the_adapter_reads() {
        // Not `call_literals`: the resolver calls wrap, so the literal is
        // on the line after the `(`.
        fn vars_in(body: &str, prefix: &str) -> Vec<String> {
            let mut out = Vec::new();
            let mut rest = body;
            while let Some(i) = rest.find('"') {
                let after = &rest[i + 1..];
                match after.find('"') {
                    Some(j) => {
                        let lit = &after[..j];
                        if lit.starts_with(prefix)
                            && lit
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                        {
                            out.push(lit.to_string());
                        }
                        rest = &after[j..];
                    }
                    None => break,
                }
            }
            out.sort();
            out.dedup();
            out
        }

        // `try_from_config` is the adapter's: `from_config` is the display
        // settings' own, and reads no vars at all.
        let read = vars_in(&fn_body("try_from_config"), "TELEGRAM_");
        let listed: Vec<String> = env_surface().into_iter().map(|k| k.name).collect();
        for var in &read {
            assert!(listed.contains(var), "{var} is read but not in .env's list");
        }
        for name in &listed {
            assert!(read.contains(name), "{name} is listed but nothing reads it");
        }
        assert_eq!(read.len(), 2, "the adapter reads two vars: {read:?}");
    }

    /// Telegram's command menu is capped at 32 entries and the adapter
    /// takes them POSITIONALLY (`.take(32)`), so a help row added at the
    /// end of the table exists everywhere except the one surface whose
    /// users discover commands by typing `/`.
    ///
    /// Nothing guarded that. This does: the table's length is allowed to
    /// grow past 32, but a command that is meant to be discoverable in
    /// Telegram has to be inside the window, and the day the 32nd row
    /// pushes one out, this says which.
    #[test]
    fn the_commands_telegram_shows_are_inside_its_thirty_two_row_menu() {
        let rows =
            veles_frontends::commands::help_entries(veles_frontends::commands::Surface::Gateway);
        let menu: Vec<&str> = rows
            .iter()
            .take(32)
            .map(|(usage, _)| usage.split(' ').next().unwrap_or(usage))
            .collect();

        // The readers a messaging user reaches for first. `/quota` is
        // here because it is the whole point of the command on a
        // surface with no status bar to put the figure in.
        for want in ["/status", "/usage", "/quota", "/context", "/model"] {
            assert!(
                menu.contains(&want),
                "{want} fell out of Telegram's 32-row menu — it is row {:?} of {}",
                rows.iter()
                    .position(|(u, _)| u.split(' ').next() == Some(want))
                    .map(|i| i + 1),
                rows.len()
            );
        }
    }

    /// Every menu description has to survive Telegram's 256-char
    /// ceiling without being cut mid-thought.
    #[test]
    fn no_menu_description_is_truncated_by_telegrams_ceiling() {
        for (usage, desc) in
            veles_frontends::commands::help_entries(veles_frontends::commands::Surface::Gateway)
        {
            assert!(
                desc.chars().count() <= 256,
                "{usage}'s description is {} chars and Telegram cuts at 256",
                desc.chars().count()
            );
        }
    }
}
