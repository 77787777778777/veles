//! Live probe against the LOCAL ergochat (127.0.0.1:6667, plaintext).
//!
//! Exercises the full irc-crate plumbing we depend on:
//!   1. connect + CAP LS/REQ/ACK state machine (driven by us)
//!   2. SASL PLAIN → 903
//!   3. registration (001) + channel join
//!   4. formatted PRIVMSG round trip (echo-message proves delivery)
//!
//! The ergo account is registered on first run via the Ergo service.
//!
//! Run: cargo run -p veles-irc --example irc_live_probe

use futures::StreamExt;
use irc::client::prelude::*;
use irc::proto::command::CapSubCommand;
use std::time::Duration;

const SERVER: &str = "127.0.0.1";
const PORT: u16 = 6667;
const CHANNEL: &str = "#velestest";
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Unique nick/account per run (ergo reserves nicks for accounts and
    // holds them through the quit grace period — reusing a nick from a
    // previous run gets 433).
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        % 100000;
    let nick = format!("velesprobe{suffix}");
    let user = nick.clone();
    let account_pass = format!("veles-probe-pass-{suffix}");

    // ── Phase 1: register the account (plain connection, no SASL) ──
    println!("phase 1: account registration ({nick})");
    {
        let cfg = Config {
            nickname: Some(nick.clone()),
            username: Some(user.clone()),
            server: Some(SERVER.into()),
            port: Some(PORT),
            use_tls: Some(false),
            ..Default::default()
        };
        let mut client = Client::from_config(cfg).await?;
        client.identify()?;
        let mut stream = client.stream()?;
        let mut registered = false;
        let mut account_ok = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while let Some(next) = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()
            .flatten()
        {
            let msg = next?;
            match &msg.command {
                Command::Response(resp, args) => {
                    if resp == &Response::RPL_WELCOME {
                        registered = true;
                        // Ergo registration: PRIVMSG NickServ REGISTER <pass> <email>
                        // (ergo services are unified under NickServ).
                        client.send(Command::PRIVMSG(
                            "NickServ".into(),
                            format!("REGISTER {account_pass} probe@veles.test"),
                        ))?;
                    }
                    if resp == &Response::RPL_SASLSUCCESS {
                        account_ok = true;
                    }
                    println!("  num {:?}: {:?}", resp, args.last());
                }
                Command::NOTICE(_, text) => {
                    println!("  NOTICE: {text}");
                    if text.to_lowercase().contains("account registered")
                        || text.to_lowercase().contains("successfully registered")
                        || text.to_lowercase().contains("now registered")
                    {
                        account_ok = true;
                    }
                }
                Command::PRIVMSG(_, text) => {
                    println!("  PRIVMSG: {text}");
                    if text.to_lowercase().contains("registered") {
                        account_ok = true;
                    }
                }
                _ => {}
            }
            if registered && account_ok {
                break;
            }
        }
        if !account_ok {
            println!("  (registration reply unclear — account may already exist; continuing)");
        }
        let _ = client.send(Command::QUIT(Some("probe done".into())));
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    // ── Phase 2: SASL PLAIN + caps + join + echo round trip ──
    println!("phase 2: SASL PLAIN + caps + channel round trip");
    let cfg = Config {
        nickname: Some(nick.clone()),
        username: Some(user.clone()),
        server: Some(SERVER.into()),
        port: Some(PORT),
        use_tls: Some(false),
        ..Default::default()
    };
    let mut client = Client::from_config(cfg).await?;
    // Drive the handshake ourselves (no identify() — that CAP ENDs).
    client.send_cap_ls(NegotiationVersion::V302)?;
    client.send(Command::NICK(nick.clone()))?;
    client.send(Command::USER(
        user.clone(),
        "0".into(),
        "veles probe".into(),
    ))?;

    let mut stream = client.stream()?;
    let mut caps_acked = false;
    let mut sasl_ok = false;
    let mut welcomed = false;
    let mut joined = false;
    let mut echo_seen = false;
    let mut offered: Vec<String> = Vec::new();
    let mut req_sent = false; // guard: REQ exactly once
    let mut sasl_started = false; // guard: SASL exactly once
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);

    while let Some(next) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .ok()
        .flatten()
    {
        let msg = next?;
        match &msg.command {
            Command::CAP(_, sub, marker, params) => {
                let params: Vec<String> = params.iter().cloned().collect();
                let more = marker.as_deref() == Some("*");
                println!("  CAP {:?} marker={marker:?} params={params:?}", sub);
                match sub {
                    CapSubCommand::LS => {
                        if let Some(list) = params.last() {
                            for cap in list.split_whitespace() {
                                offered.push(cap.split('=').next().unwrap_or(cap).to_string());
                            }
                        }
                        if !more && !req_sent {
                            req_sent = true;
                            println!("  offered: {:?}", offered);
                            client.send_cap_req(&[
                                Capability::Sasl,
                                Capability::MultiPrefix,
                                Capability::AccountTag,
                                Capability::ExtendedJoin,
                                Capability::ServerTime,
                                Capability::EchoMessage,
                            ])?;
                        }
                    }
                    CapSubCommand::ACK => {
                        caps_acked = true;
                        if !sasl_started {
                            sasl_started = true;
                            client.send_sasl_plain()?;
                        }
                    }
                    _ => {}
                }
            }
            Command::AUTHENTICATE(payload) => {
                if payload == "+" {
                    use base64::Engine;
                    let raw = format!("{user}\0{user}\0{account_pass}");
                    let b64 = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                    client.send(Command::AUTHENTICATE(b64))?;
                    println!("  sent SASL PLAIN payload");
                }
            }
            Command::Response(resp, _args) => {
                match resp {
                    Response::RPL_SASLSUCCESS => {
                        sasl_ok = true;
                        println!("  ✓ 903 SASL SUCCESS");
                        client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    }
                    Response::ERR_SASLFAIL => {
                        println!("  ✗ SASL FAILED");
                        client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    }
                    Response::RPL_WELCOME => {
                        welcomed = true;
                        println!("  ✓ 001 WELCOME");
                        client.send(Command::JOIN(CHANNEL.into(), None, None))?;
                        // Ensure the channel exists (ergo: first joiner creates it).
                        client.send(Command::PRIVMSG(
                            CHANNEL.into(),
                            "\x02PROBE_OK\x02 formatted \x1Ditalic\x1D \x11mono\x11".into(),
                        ))?;
                    }
                    _ => {}
                }
            }
            Command::JOIN(channel, account, _) => {
                joined = true;
                println!("  ✓ JOIN {channel} (extended-join account: {account:?})");
            }
            Command::PRIVMSG(target, text) => {
                if target == CHANNEL && text.contains("PROBE_OK") {
                    echo_seen = true;
                    println!("  ✓ echo-message received: {text:?}");
                }
            }
            Command::PING(t, _) => {
                client.send(Command::PONG(t.clone(), None))?;
            }
            _ => {}
        }
        if welcomed && joined && echo_seen {
            break;
        }
    }

    println!();
    println!("caps_acked={caps_acked} sasl_ok={sasl_ok} welcomed={welcomed} joined={joined} echo_seen={echo_seen}");
    if caps_acked && sasl_ok && welcomed && joined && echo_seen {
        println!("IRC LIVE PROBE PASSED");
        Ok(())
    } else {
        anyhow::bail!("probe incomplete")
    }
}
