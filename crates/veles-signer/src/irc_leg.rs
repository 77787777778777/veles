//! The IRC leg: the signer's OWN connection and nick, asking the owner.
//!
//! The owner answers `yes CODE` / `no CODE` in a private message. With
//! `owner_account` set (the strong form) a reply counts only when the
//! network's IRCv3 `account` tag names that services account — anyone can
//! take a nick, nobody else can be logged in as the account. A network
//! that will not grant `account-tag` therefore gets no answers counted at
//! all: fail closed, and say so in the log.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use futures::StreamExt;
use irc::client::prelude::*;
use irc::proto::CapSubCommand;

use crate::{parse_reply, Decision, Desk, Event, IrcConfig};

/// The call in two short lines — IRC cannot carry the full detail the
/// Telegram leg shows, so the arguments are cut and the cut is counted
/// with the hash of the whole.
fn announce_lines(code: &str, req: &veles_core::ticket::TicketRequest) -> Vec<String> {
    let args = serde_json::to_string(&req.args).unwrap_or_default();
    let total = args.chars().count();
    let shown: String = args.chars().take(300).collect();
    let more = if total > 300 {
        format!(
            " … +{} chars, sha256 {}",
            total - 300,
            &veles_core::ticket::args_hash(&req.args)[..16]
        )
    } else {
        String::new()
    };
    vec![
        format!(
            "ticket request {code}: {} on node {} asked by node {} (session {}) as {}",
            req.tool, req.target, req.origin, req.session, req.grant
        ),
        format!("{code} args: {shown}{more}"),
        format!(
            "reply `yes {code}` to sign or `no {code}` to decline — only if you asked for this"
        ),
    ]
}

/// Run the leg until the process ends, reconnecting after a drop.
pub async fn run(cfg: IrcConfig, password: Option<String>, desk: Arc<Desk>) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if cfg.owner_account.is_none() {
        tracing::warn!(
            "signer: [irc] has no owner_account — a reply from whoever holds the nick {} counts",
            cfg.owner_nick
        );
    }
    loop {
        if let Err(e) = session(&cfg, password.as_deref(), &desk).await {
            tracing::warn!("signer: irc session ended: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn session(cfg: &IrcConfig, password: Option<&str>, desk: &Arc<Desk>) -> Result<()> {
    let config = Config {
        nickname: Some(cfg.nick.clone()),
        username: Some(cfg.nick.clone()),
        realname: Some("veles ticket signer".into()),
        server: Some(cfg.server.clone()),
        port: Some(cfg.port),
        use_tls: Some(cfg.tls),
        ..Default::default()
    };
    let mut client = Client::from_config(config).await?;
    let mut stream = client.stream()?;
    // Negotiation by hand: `identify()` would send CAP END first.
    let mut caps = vec![Capability::Custom("account-tag")];
    if password.is_some() {
        caps.push(Capability::Sasl);
    }
    client.send_cap_req(&caps)?;
    client.send(Command::NICK(cfg.nick.clone()))?;
    client.send(Command::USER(
        cfg.nick.clone(),
        "0".into(),
        "veles ticket signer".into(),
    ))?;
    let mut tagged = false;
    let mut events = desk.subscribe();
    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(msg) = msg else { anyhow::bail!("connection closed") };
                let msg = msg?;
                match &msg.command {
                    Command::CAP(_, CapSubCommand::ACK, _, list) => {
                        let list = list.clone().unwrap_or_default();
                        tagged |= list.split_whitespace().any(|c| c == "account-tag");
                        if list.split_whitespace().any(|c| c == "sasl") {
                            client.send_sasl_plain()?;
                        } else {
                            client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                        }
                    }
                    Command::CAP(_, CapSubCommand::NAK, _, _) => {
                        client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    }
                    Command::AUTHENTICATE(p) if p == "+" => {
                        let raw = format!("{0}\0{0}\0{1}", cfg.nick, password.unwrap_or_default());
                        client.send_sasl(base64::engine::general_purpose::STANDARD.encode(raw))?;
                    }
                    Command::Response(Response::RPL_SASLSUCCESS, _)
                    | Command::Response(Response::ERR_SASLFAIL, _) => {
                        if matches!(msg.command, Command::Response(Response::ERR_SASLFAIL, _)) {
                            tracing::warn!("signer: irc SASL refused");
                        }
                        client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    }
                    Command::Response(Response::RPL_WELCOME, _) => {
                        if cfg.owner_account.is_some() && !tagged {
                            tracing::warn!(
                                "signer: {} did not grant account-tag — no IRC answer can be \
                                 verified, so none will count",
                                cfg.server
                            );
                        }
                        // Anything that waited while we were away.
                        for code in desk.waiting() {
                            let _ = client.send_privmsg(
                                &cfg.owner_nick,
                                format!("ticket request {code} is waiting — `yes {code}` or `no {code}`"),
                            );
                        }
                    }
                    Command::PRIVMSG(target, text) if target.eq_ignore_ascii_case(&cfg.nick) => {
                        let from = msg.source_nickname().unwrap_or_default();
                        if !from.eq_ignore_ascii_case(&cfg.owner_nick) {
                            continue;
                        }
                        if let Some(want) = &cfg.owner_account {
                            let account = msg
                                .tags
                                .as_ref()
                                .and_then(|t| t.iter().find(|t| t.0 == "account"))
                                .and_then(|t| t.1.clone());
                            if account.as_deref() != Some(want.as_str()) {
                                let _ = client.send_notice(
                                    from,
                                    "not counted: you are not logged in as the owner's account",
                                );
                                continue;
                            }
                        }
                        let Some((yes, code)) = parse_reply(text) else { continue };
                        let by = format!("irc:{}", cfg.owner_account.as_deref().unwrap_or(from));
                        let d = if yes { Decision::Sign { by } } else { Decision::Decline { by } };
                        if !desk.decide(&code, d) {
                            let _ = client.send_notice(
                                from,
                                format!("{code} is not waiting (waiting: {})", desk.waiting().join(", ")),
                            );
                        }
                    }
                    _ => {}
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(Event::Ask { code, req }) => {
                        for line in announce_lines(&code, &req) {
                            client.send_privmsg(&cfg.owner_nick, line)?;
                        }
                    }
                    Ok(Event::Settled { code, outcome }) => {
                        let _ = client.send_notice(
                            &cfg.owner_nick,
                            format!("ticket request {code}: {outcome}"),
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}
