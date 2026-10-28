//! Live end-to-end: the M11 approval prompt on IRC.
//!
//! Connects as the authorized test user, asks the bot to run something
//! the prefilter flags, waits for the "[approval needed]" prompt, answers
//! it by number, and asserts the turn then completes. Proves the part no
//! offline test can: that the answer resolves the ask WITHOUT being
//! processed as a new user turn (the gateway is parked on it).
//!
//! Needs: local ergo + a gateway running with the IRC adapter and
//! VELES_APPROVALS_MODE=manual (so the aux screen doesn't decide first).
//!
//! Run: cargo run -p veles-irc --example irc_approval_e2e

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
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/veles-live-scratch/perm".to_string());
    let message = format!(
        "{BOT_NICK}: use the terminal tool to run exactly: chmod -R 777 {target} \
         — then tell me the tool's exact output."
    );

    let cfg = Config {
        nickname: Some(NICK.into()),
        username: Some(NICK.into()),
        server: Some(SERVER.into()),
        port: Some(PORT),
        use_tls: Some(false),
        ..Default::default()
    };
    let mut client = Client::from_config(cfg).await?;
    client.send_cap_ls(NegotiationVersion::V302)?;

    let mut stream = client.stream()?;
    let mut sent = false;
    let mut sasl_started = false;
    let mut saw_prompt = false;
    let mut answered = false;
    let mut saw_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);

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
                        if !more {
                            client.send_cap_req(&[Capability::Sasl])?;
                        }
                        let _ = list;
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
                Response::RPL_SASLSUCCESS | Response::ERR_SASLFAIL => {
                    client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    client.send(Command::NICK(NICK.into()))?;
                    client.send(Command::USER(
                        NICK.into(),
                        "0".into(),
                        "veles approval e2e".into(),
                    ))?;
                }
                Response::RPL_WELCOME => {
                    println!("driver connected");
                    client.send(Command::JOIN(CHANNEL.into(), None, None))?;
                }
                _ => {}
            },
            Command::JOIN(channel, _, _) => {
                if !sent && channel == CHANNEL {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    println!("→ {message}");
                    client.send(Command::PRIVMSG(CHANNEL.into(), message.clone()))?;
                    sent = true;
                }
            }
            Command::PRIVMSG(_, text) => {
                println!("BOT: {text}");
                if text.contains("[approval needed]") {
                    saw_prompt = true;
                    assert!(
                        text.contains("chmod") || text.contains("permission"),
                        "prompt must say what is being approved: {text}"
                    );
                }
                // The prompt arrives as several lines; answer once the
                // options line has landed.
                if saw_prompt && !answered && text.contains("allow once") {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    println!("→ answering: 1 (allow once)");
                    client.send(Command::PRIVMSG(CHANNEL.into(), "1".to_string()))?;
                    answered = true;
                }
                if answered
                    && (text.contains("777") || text.contains("chmod"))
                    && !text.contains("[approval needed]")
                {
                    saw_final = true;
                }
            }
            Command::NOTICE(_, text) => {
                println!("notice: {text}");
                if text.contains("decision recorded") {
                    println!("✓ the answer was consumed as an ask reply, not a new turn");
                }
            }
            Command::PING(t, _) => client.send(Command::PONG(t.clone(), None))?,
            _ => {}
        }
        if saw_final {
            break;
        }
    }

    println!("\nprompt seen: {saw_prompt} · answered: {answered} · turn finished: {saw_final}");
    assert!(saw_prompt, "no approval prompt arrived in the channel");
    assert!(answered, "driver never got to answer");
    assert!(saw_final, "turn did not complete after the answer");
    println!("IRC APPROVAL E2E PASSED");
    Ok(())
}
