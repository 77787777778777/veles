//! CertFP live probe: SASL EXTERNAL over TLS with a client certificate.
//!
//! Two phases:
//!   1. PLAIN login → NickServ CERT ADD (bind the certificate
//!      fingerprint to the account) — skipped once bound.
//!   2. Fresh TLS connection with the client cert → SASL EXTERNAL →
//!      903 proves passwordless, certificate-only authentication.
//!
//! Run: cargo run -p veles-irc --example irc_certfp_probe

use futures::StreamExt;
use irc::client::prelude::*;
use irc::proto::command::CapSubCommand;
use std::time::Duration;

const SERVER: &str = "127.0.0.1";
const PORT: u16 = 6697;
const NICK: &str = "jackma_veles";
const PASS: &str = "veles-bot-irc-local-1";
/// The local test PKI, in the veles data dir (`~/.local/share/veles/certs`).
fn cert(name: &str) -> String {
    veles_core::config::data_dir()
        .join("certs")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn tls_config(use_client_cert: bool) -> Config {
    Config {
        nickname: Some(NICK.into()),
        username: Some(NICK.into()),
        server: Some(SERVER.into()),
        port: Some(PORT),
        use_tls: Some(true),
        cert_path: Some(cert("irc-ca.crt")), // trust the local test CA
        client_cert_path: use_client_cert.then(|| cert("irc-client.crt")),
        // irc-crate quirk: rustls reads the PRIVATE KEY PEM from
        // client_cert_pass (documented in client.rs).
        client_cert_pass: use_client_cert
            .then(|| std::fs::read_to_string(cert("irc-client.key")).unwrap_or_default()),
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Pick rustls' crypto provider explicitly (both ring and aws-lc-rs
    // are in the unified feature graph).
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls provider already installed"))?;

    // ── Phase 1: ensure the cert is bound to the account ──
    println!("phase 1: bind certificate fingerprint (PLAIN + NickServ CERT ADD)");
    {
        let cfg = Config {
            nickname: Some(NICK.into()),
            username: Some(NICK.into()),
            server: Some(SERVER.into()),
            port: Some(PORT),
            use_tls: Some(true),
            cert_path: Some(cert("irc-ca.crt")),
            client_cert_path: Some(cert("irc-client.crt")),
            client_cert_pass: Some(std::fs::read_to_string(cert("irc-client.key"))?),
            ..Default::default()
        };
        let mut client = Client::from_config(cfg).await?;
        client.send_cap_ls(NegotiationVersion::V302)?;
        let mut stream = client.stream()?;
        let mut welcomed = false;
        let mut done = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while let Some(next) = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()
            .flatten()
        {
            let msg = next?;
            match &msg.command {
                Command::CAP(_, sub, marker, params) => {
                    let more = marker.as_deref() == Some("*");
                    let has_list = params.iter().any(|p| !p.is_empty())
                        || marker.as_deref().map(|m| m != "*").unwrap_or(false);
                    match sub {
                        CapSubCommand::LS => {
                            if !more {
                                client.send_cap_req(&[Capability::Sasl])?;
                            }
                        }
                        CapSubCommand::ACK if has_list => {
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
                        client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                        client.send(Command::NICK(NICK.into()))?;
                        client.send(Command::USER(
                            NICK.into(),
                            "0".into(),
                            "certfp setup".into(),
                        ))?;
                    }
                    Response::ERR_SASLFAIL => {
                        println!("  phase1 SASL failed — cert login needed first?");
                        client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                        client.send(Command::NICK(NICK.into()))?;
                        client.send(Command::USER(
                            NICK.into(),
                            "0".into(),
                            "certfp setup".into(),
                        ))?;
                    }
                    Response::RPL_WELCOME => {
                        welcomed = true;
                        // Add the presented certificate to the account's
                        // fingerprint list (idempotent: 'already in use'
                        // is fine).
                        client.send(Command::PRIVMSG("NickServ".into(), "CERT ADD".into()))?;
                    }
                    _ => {}
                },
                Command::NOTICE(_, text) => {
                    println!("  phase1 notice: {text}");
                    let t = text.to_lowercase();
                    if t.contains("added") || t.contains("in use") || t.contains("already") {
                        done = true;
                        break;
                    }
                }
                Command::PRIVMSG(_, text) => {
                    println!("  phase1 privmsg: {text}");
                    let t = text.to_lowercase();
                    if t.contains("added") || t.contains("in use") || t.contains("already") {
                        done = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        println!(
            "  phase1: welcomed={welcomed} cert-bind={}",
            if done { "ok/already" } else { "unclear" }
        );
        let _ = client.send(Command::QUIT(Some("phase 1 done".into())));
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    // ── Phase 2: SASL EXTERNAL — passwordless, cert-only auth ──
    println!("phase 2: SASL EXTERNAL (client cert, NO password)");
    let mut client = Client::from_config(tls_config(true)).await?;
    client.send_cap_ls(NegotiationVersion::V302)?;
    let mut stream = client.stream()?;
    let mut sasl_ok = false;
    let mut welcomed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while let Some(next) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .ok()
        .flatten()
    {
        let msg = next?;
        match &msg.command {
            Command::CAP(_, sub, marker, _params) => {
                let more = marker.as_deref() == Some("*");
                match sub {
                    CapSubCommand::LS => {
                        if !more {
                            client.send_cap_req(&[Capability::Sasl])?;
                        }
                    }
                    CapSubCommand::ACK => {
                        println!("  caps acked → SASL EXTERNAL");
                        client.send_sasl_external()?;
                    }
                    _ => {}
                }
            }
            Command::AUTHENTICATE(p) => {
                if p == "+" {
                    // EXTERNAL with empty payload
                    client.send(Command::AUTHENTICATE("+".into()))?;
                }
            }
            Command::Response(resp, args) => match resp {
                Response::RPL_SASLSUCCESS => {
                    sasl_ok = true;
                    println!("  ✓ 903 SASL EXTERNAL SUCCESS (certificate-only auth!)");
                    client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    client.send(Command::NICK(NICK.into()))?;
                    client.send(Command::USER(
                        NICK.into(),
                        "0".into(),
                        "certfp probe".into(),
                    ))?;
                }
                Response::ERR_SASLFAIL => {
                    println!("  ✗ EXTERNAL failed: {:?}", args.last());
                    client.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                    client.send(Command::NICK(NICK.into()))?;
                    client.send(Command::USER(
                        NICK.into(),
                        "0".into(),
                        "certfp probe".into(),
                    ))?;
                }
                Response::RPL_WELCOME => {
                    welcomed = true;
                    println!("  ✓ 001 WELCOME over TLS+CertFP");
                    break;
                }
                _ => {}
            },
            _ => {}
        }
    }

    println!();
    println!("sasl_external_ok={sasl_ok} welcomed={welcomed}");
    if sasl_ok && welcomed {
        println!("CERTFP PROBE PASSED");
        Ok(())
    } else {
        anyhow::bail!("certfp probe incomplete")
    }
}
