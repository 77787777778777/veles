//! One-shot: register two ergo accounts on the LOCAL server for the
//! veles-irc live test — the bot account (SASL PLAIN auth) and a test
//! user account (authorized to command the bot).
//!
//! Run: cargo run -p veles-irc --example irc_setup_accounts

use futures::StreamExt;
use irc::client::prelude::*;
use std::time::Duration;

const SERVER: &str = "127.0.0.1";
const PORT: u16 = 6667;

/// (nick, account password) pairs to register.
const ACCOUNTS: &[(&str, &str)] = &[
    ("jackma_veles", "veles-bot-irc-local-1"),
    ("wrathirc", "wrath-test-user-local-1"),
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    for (nick, pass) in ACCOUNTS {
        println!("registering account {nick}…");
        let cfg = Config {
            nickname: Some((*nick).into()),
            username: Some((*nick).into()),
            server: Some(SERVER.into()),
            port: Some(PORT),
            use_tls: Some(false),
            ..Default::default()
        };
        let mut client = Client::from_config(cfg).await?;
        client.identify()?;
        let mut stream = client.stream()?;
        let mut welcomed = false;
        let mut outcome: Option<String> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while let Some(next) = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()
            .flatten()
        {
            let msg = next?;
            match &msg.command {
                Command::Response(resp, _) => {
                    if resp == &Response::RPL_WELCOME && !welcomed {
                        welcomed = true;
                        client.send(Command::PRIVMSG(
                            "NickServ".into(),
                            format!("REGISTER {pass} {nick}@veles.local"),
                        ))?;
                    }
                }
                Command::NOTICE(_, text) => {
                    let t = text.to_lowercase();
                    if t.contains("already registered") {
                        outcome = Some("already registered".into());
                        break;
                    }
                    if t.contains("account created") || t.contains("successfully") {
                        outcome = Some("registered".into());
                        break;
                    }
                    println!("  notice: {text}");
                }
                _ => {}
            }
        }
        println!(
            "  → {}",
            outcome.unwrap_or_else(|| "unclear (continuing)".into())
        );
        let _ = client.send(Command::QUIT(Some("setup done".into())));
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    println!("SETUP DONE");
    Ok(())
}
