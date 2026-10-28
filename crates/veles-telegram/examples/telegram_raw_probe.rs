//! Raw getUpdates probe — bypasses teloxide's repl/Dispatcher entirely.
//! If updates arrive here but not via repl(), the freeze is in the
//! dispatcher's nested-runtime machinery (the M4 pattern), not the
//! token/network.
//!
//! Run: cargo run -p veles-telegram --example telegram_raw_probe
use teloxide::prelude::*;
use teloxide::types::UpdateKind;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(veles_core::config::config_dir().join(".env"))?;
    let token = raw
        .lines()
        .find(|l| l.starts_with("TELEGRAM_BOT_TOKEN="))
        .map(|l| l.trim_start_matches("TELEGRAM_BOT_TOKEN=").to_string())
        .ok_or_else(|| anyhow::anyhow!("no token"))?;

    let bot = Bot::new(&token);
    let me = bot.get_me().await?;
    println!(
        "raw probe online as @{}",
        me.user.username.unwrap_or_default()
    );

    let mut offset: Option<i32> = None;
    println!("raw getUpdates loop for 40s — send a message now");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    while std::time::Instant::now() < deadline {
        let mut req = bot.get_updates().timeout(3u32);
        if let Some(o) = offset {
            req = req.offset(o);
        }
        match req.await {
            Ok(updates) => {
                for u in updates {
                    offset = Some(u.id.0 as i32 + 1);
                    match &u.kind {
                        UpdateKind::Message(m) => {
                            println!(
                                "RAW GOT UPDATE: id={} from={:?} text={:?}",
                                m.id.0,
                                m.from.as_ref().map(|x| x.id.0),
                                m.text()
                            );
                        }
                        other => {
                            println!(
                                "RAW GOT non-message update: {:?}",
                                std::mem::discriminant(other)
                            );
                        }
                    }
                }
            }
            Err(e) => {
                println!("RAW ERROR: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    println!("RAW PROBE WINDOW ELAPSED");
    Ok(())
}
