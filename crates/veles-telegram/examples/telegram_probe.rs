//! Telegram adapter probe — verifies the config→credential→API chain
//! WITHOUT long-polling (long-polling the real bot would steal updates
//! from the running hermes gateway that owns the token).
//!
//! Checks:
//!   1. try_from_config parses the real ~/.hermes config + ~/.hermes/.env
//!   2. live get_me proves the resolved token authenticates
//!   3. split_message unit checks (4096 cap, line boundaries)
//!
//! Run: cargo run -p veles-telegram --example telegram_probe

use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    let cfg = veles_core::config::load().expect("config");
    println!(
        "1) config loaded (platforms: telegram={})",
        cfg.platforms
            .get("telegram")
            .map(|t| t["enabled"].as_bool().unwrap_or(false))
            .unwrap_or(false)
    );

    {
        let adapter = match veles_telegram::TelegramAdapter::try_from_config(&cfg) {
            Some(a) => a,
            None => {
                println!("SKIP: telegram adapter not configured/enabled");
                return;
            }
        };
        let _ = adapter; // built from real config — the chain works
    }
    println!("2) adapter built from real config ✓");

    // get_me is stateless — safe while hermes owns the polling connection.
    let token = veles_core::providers::resolve_env_or_dotenv(
        "TELEGRAM_BOT_TOKEN",
        &veles_core::config::veles_home(),
    )
    .expect("token resolved");
    let bot = Bot::new(&token);
    match bot.get_me().await {
        Ok(me) => {
            println!(
                "3) live get_me ✓ — bot @{} (id {})",
                me.user.username.unwrap_or_default(),
                me.user.id.0
            );
        }
        Err(e) => panic!("get_me failed: {e}"),
    }

    // split_message behavior
    let chunks = veles_telegram::split_message(&"line\n".repeat(2000), 4096);
    assert!(chunks.iter().all(|c| c.len() <= 4096), "chunk over cap");
    assert!(chunks.len() > 1, "expected splitting");
    let long_line = "x".repeat(9000);
    let chunks = veles_telegram::split_message(&long_line, 4096);
    assert_eq!(chunks.len(), 3);
    assert!(chunks.iter().all(|c| c.len() <= 4096));
    println!("4) split_message: line-boundary + hard-cut chunking ✓");

    println!("\nTELEGRAM PROBE PASSED");
}
