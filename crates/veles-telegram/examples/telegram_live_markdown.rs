//! Live probe: send a MarkdownV2-formatted test message to the user's
//! telegram chat through the real bot. Proves the full formatting
//! pipeline produces output Telegram ACCEPTS (a parse error here would
//! mean the message is malformed).
//!
//! Run: cargo run --example telegram_live_markdown
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::Requester;
use veles_telegram::markdown::format_message;

#[tokio::main]
async fn main() {
    let env_path = veles_core::config::config_dir().join(".env");
    let raw = std::fs::read_to_string(&env_path).expect("read .env");
    let token = raw
        .lines()
        .find(|l| l.starts_with("TELEGRAM_BOT_TOKEN="))
        .map(|l| l.trim_start_matches("TELEGRAM_BOT_TOKEN=").to_string())
        .expect("token");

    let md = "## M5b live test\n\n\
              Telegram **MarkdownV2** is now *working* in veles:\n\n\
              - headings, **bold**, *italic*\n\
              - `inline code`\n\
              - tables and [links](https://example.com/doc)\n\n\
              | surface | status |\n|---|---|\n| markdown | ✅ |\n| progress | ✅ |\n\n\
              > display config flows from the hermes display block\n\n\
              Sent by the overnight M5b run.";

    let formatted = format_message(md);
    println!("formatted ({} chars):\n{formatted}", formatted.len());

    let bot = teloxide::Bot::new(&token);
    let chat_id = teloxide::types::ChatId(123456789);
    match bot
        .send_message(chat_id, &formatted)
        .parse_mode(teloxide::types::ParseMode::MarkdownV2)
        .await
    {
        Ok(m) => println!("SENT OK: message id {:?}", m.id),
        Err(e) => {
            println!("REJECTED: {e}");
            println!("→ the formatter produced invalid MarkdownV2");
            std::process::exit(1);
        }
    }
}
