//! Live end-to-end driver: connects as the AUTHORIZED test user
//! (wrathirc, SASL PLAIN), joins the test channel, sends a message to
//! the veles bot, and prints every response it receives for 90s.
//!
//! Usage:
//!   irc_e2e_driver                    # default: mention + markdown ask
//!   irc_e2e_driver "custom text"      # custom message text
//!
//! Run: cargo run -p veles-irc --example irc_e2e_driver -- "text"

use futures::StreamExt;
use irc::client::prelude::*;
use irc::proto::command::CapSubCommand;
use std::time::Duration;

const SERVER: &str = "127.0.0.1";
const PORT: u16 = 6667;
const NICK: &str = "wrathirc";
const PASS: &str = "wrath-test-user-local-1";
const CHANNEL: &str = "#velestest";
const BOT_NICK: &str = "jackma_veles";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let custom = std::env::args().nth(1);
    let message = custom.unwrap_or_else(|| {
        format!(
            "{BOT_NICK}: Reply with a short markdown demo: one ## heading, one **bold** word, one inline `code` span, and a fenced python block containing `def hello_world(): return _name * 2`."
        )
    });

    let cfg = Config {
        nickname: Some(NICK.into()),
        username: Some(NICK.into()),
        server: Some(SERVER.into()),
        port: Some(PORT),
        use_tls: Some(false),
        ..Default::default()
    };
    let mut client = Client::from_config(cfg).await?;
    // SASL-FIRST ordering (same as the adapter): NICK/USER only after
    // CAP END, or ergo's reserved-nick handling rejects the handshake.
    client.send_cap_ls(NegotiationVersion::V302)?;

    let mut stream = client.stream()?;
    let mut welcomed = false;
    let mut sent = false;
    let mut sasl_started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);

    while let Some(next) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .ok()
        .flatten()
    {
        let msg = next?;
        match &msg.command {
            Command::CAP(_, sub, marker, params) => {
                let list: Vec<String> = params.iter().cloned().collect();
                let more = marker.as_deref() == Some("*");
                match sub {
                    CapSubCommand::LS => {
                        let caps: Vec<String> = {
                            let mut all: Vec<String> = Vec::new();
                            if let Some(m) = marker {
                                if m != "*" {
                                    all.extend(m.split_whitespace().map(str::to_string));
                                }
                            }
                            if let Some(l) = list.last() {
                                all.extend(l.split_whitespace().map(str::to_string));
                            }
                            all
                        };
                        if !more {
                            println!("driver caps offered: {} items", caps.len());
                            client.send_cap_req(&[Capability::Sasl])?;
                        }
                    }
                    CapSubCommand::ACK if !sasl_started => {
                        sasl_started = true;
                        client.send_sasl_plain()?;
                    }
                    _ => {}
                }
            }
            Command::AUTHENTICATE(p) => {
                if p == "+" {
                    use base64::Engine;
                    let raw = format!("{NICK}\0{NICK}\0{PASS}");
                    let b64 = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                    client.send(Command::AUTHENTICATE(b64))?;
                }
            }
            Command::Response(resp, _) => match resp {
                Response::RPL_SASLSUCCESS => {
                    // CAP END, THEN register (SASL-first ordering).
                    client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    client.send(Command::NICK(NICK.into()))?;
                    client.send(Command::USER(
                        NICK.into(),
                        "0".into(),
                        "veles e2e driver".into(),
                    ))?;
                }
                Response::ERR_SASLFAIL => {
                    println!("DRIVER SASL FAILED");
                    client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    client.send(Command::NICK(NICK.into()))?;
                    client.send(Command::USER(
                        NICK.into(),
                        "0".into(),
                        "veles e2e driver".into(),
                    ))?;
                }
                Response::RPL_WELCOME => {
                    welcomed = true;
                    println!("driver connected + SASL ok");
                    client.send(Command::JOIN(CHANNEL.into(), None, None))?;
                }
                _ => {}
            },
            Command::JOIN(channel, account, _) => {
                if !sent && channel == CHANNEL {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    println!("driver sending: {message}");
                    client.send(Command::PRIVMSG(CHANNEL.into(), message.clone()))?;
                    sent = true;
                }
                println!("  join: {channel} account={account:?}");
            }
            Command::PRIVMSG(target, text) => {
                println!("BOT→{}: {text}", target);
                println!("--- raw bytes: {:?}", text.as_bytes());
            }
            Command::NOTICE(_, text) => {
                println!("notice: {text}");
            }
            Command::PING(t, _) => {
                client.send(Command::PONG(t.clone(), None))?;
            }
            _ => {}
        }
        if welcomed && sent {
            // keep listening for replies until deadline
        }
    }
    println!("DRIVER DONE");
    Ok(())
}
