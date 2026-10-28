// An adapter/plugin/types crate: its spawn sites are its OWN work on its
// OWN runtime (a connection task, a test driver), not a module calling
// into core. The M3 hop (clippy.toml) is veles-core/veles-frontends
// business; when THIS crate becomes a module (R4/R6) its spawn sites are
// reviewed and the allow is dropped.
#![allow(clippy::disallowed_methods)]
//! The headless ticket signer — PLAN-pool-round.md Part S.
//!
//! USER 2026-09-29: cross-node calls above the User level need a ticket
//! "signed by a frontend, gateway itself can never self-sign", and "we
//! need to work out a system for irc and telegram to be able to sign a
//! ticket - an independent proxy for "headless" frontend ticket
//! signatures with the emergency option of manually sending one encoded
//! as a plain message".
//!
//! The desktop and the TUI sign with a key they unlock themselves. A chat
//! surface has nothing to unlock a key IN, so this process holds one for
//! it:
//!
//! * its OWN key file (`veles ticket keygen`), unlocked once at start;
//! * its OWN Telegram bot token and IRC nick — never the gateway's, so a
//!   compromised gateway can neither read the owner's answers nor answer
//!   in the owner's name;
//! * a gateway asks it over HTTP (`POST /v1/sign`, bearer-authenticated)
//!   with a ticket REQUEST — the exact call, as the target minted it;
//! * it shows the owner that call, in full, on its own bot or nick, and
//!   signs only on an explicit yes from the owner's Telegram user id or
//!   the owner's services account — whichever node asks, the one it
//!   runs beside included (ticket.rs: where a key lives does not matter).
//!
//! The emergency path needs none of this: the gateway posts the request
//! code on the chat, the owner signs it on any key-holding device and
//! pastes the ticket back as a plain message (remotetool.rs, ask.rs).

pub mod irc_leg;
pub mod telegram;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};
use veles_core::ticket::{SignerKey, TicketRequest};

/// `signer.toml`, in the signer's own directory (`veles ticket keygen`'s
/// key directory unless `--dir` says otherwise).
#[derive(Debug, Clone, Deserialize)]
pub struct SignerConfig {
    /// The key's name: `<dir>/<key>.key`, as `veles ticket keygen` wrote it.
    pub key: String,
    /// Where gateways reach it. Loopback by default; a WireGuard address
    /// when the other node's gateway asks.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Longest a request waits for the owner, in seconds. The request's
    /// own expiry caps it further.
    #[serde(default = "default_wait")]
    pub wait_secs: u64,
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub irc: Option<IrcConfig>,
}

fn default_listen() -> String {
    "127.0.0.1:9911".into()
}

fn default_wait() -> u64 {
    600
}

/// `[telegram]`: the owner's user id. The bot token is a secret
/// (`SIGNER_TELEGRAM_TOKEN` in `signer.env`).
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    /// The owner's numeric Telegram user id — the only account whose
    /// button press or reply signs anything.
    pub owner_id: i64,
    /// Where a request is shown: a chat id, usually a private group with
    /// the owner (the ops chat, USER 2026-10-21). Default: the owner's DM.
    /// Only `owner_id` may answer, wherever the question is shown.
    #[serde(default)]
    pub chat_id: Option<i64>,
    /// The Bot API base. Tests point it at a stub.
    #[serde(default = "default_tg_api")]
    pub api_url: String,
}

fn default_tg_api() -> String {
    "https://api.telegram.org".into()
}

/// `[irc]`: the signer's own connection. SASL password (optional) is
/// `SIGNER_IRC_PASSWORD` in `signer.env`.
#[derive(Debug, Clone, Deserialize)]
pub struct IrcConfig {
    pub server: String,
    #[serde(default = "default_irc_port")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub tls: bool,
    /// The signer's nick — its own, not the gateway's.
    pub nick: String,
    /// Who it asks.
    pub owner_nick: String,
    /// The owner's services account. A reply counts only when the
    /// network's `account` tag names it; without this set, the nick alone
    /// decides, which any user who takes the nick could exploit — the
    /// signer says so at start.
    #[serde(default)]
    pub owner_account: Option<String>,
}

fn default_irc_port() -> u16 {
    6697
}

fn default_true() -> bool {
    true
}

/// The secrets, from `signer.env` (0600) or the environment.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    /// `name:token` pairs a gateway presents as `Authorization: Bearer`.
    pub gateway_tokens: Vec<(String, String)>,
    pub telegram_token: Option<String>,
    pub irc_password: Option<String>,
}

impl Secrets {
    /// `SIGNER_GATEWAY_TOKENS=phone:…,laptop:…`, `SIGNER_TELEGRAM_TOKEN`,
    /// `SIGNER_IRC_PASSWORD` — the process environment first, then
    /// `<dir>/signer.env`.
    pub fn load(dir: &std::path::Path) -> Self {
        let file: Vec<(String, String)> = std::fs::read_to_string(dir.join("signer.env"))
            .map(|raw| veles_core::secrets::parse_env(&raw))
            .unwrap_or_default();
        let get = |k: &str| {
            std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| {
                file.iter()
                    .find(|(n, v)| n == k && !v.is_empty())
                    .map(|(_, v)| v.clone())
            })
        };
        let gateway_tokens = get("SIGNER_GATEWAY_TOKENS")
            .map(|v| {
                v.split(',')
                    .filter_map(|p| p.trim().split_once(':'))
                    .filter(|(n, t)| !n.is_empty() && !t.is_empty())
                    .map(|(n, t)| (n.to_string(), t.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            gateway_tokens,
            telegram_token: get("SIGNER_TELEGRAM_TOKEN"),
            irc_password: get("SIGNER_IRC_PASSWORD"),
        }
    }
}

/// Read `<dir>/signer.toml`.
pub fn load_config(dir: &std::path::Path) -> Result<SignerConfig> {
    let path = dir.join("signer.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} (see `veles signer --help`)", path.display()))?;
    let cfg: SignerConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    anyhow::ensure!(
        cfg.telegram.is_some() || cfg.irc.is_some(),
        "signer.toml: configure [telegram] or [irc] — the signer has to be able to ask someone"
    );
    Ok(cfg)
}

// ── the desk: requests waiting on the owner ─────────────────────────

/// The owner's answer to one request.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Sign { by: String },
    Decline { by: String },
}

/// What the legs are told.
#[derive(Debug, Clone)]
pub enum Event {
    /// Ask the owner about this request.
    Ask { code: String, req: TicketRequest },
    /// It is over: "signed", "declined", or "expired".
    Settled { code: String, outcome: String },
}

struct Pending {
    tx: Option<oneshot::Sender<Decision>>,
}

/// Requests waiting on the owner, by short code.
pub struct Desk {
    pending: Mutex<HashMap<String, Pending>>,
    events: broadcast::Sender<Event>,
}

impl Desk {
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            pending: Mutex::new(HashMap::new()),
            events,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Put `req` in front of the owner. The code is what they answer with
    /// on IRC; it is taken from the request id, which the target minted.
    fn open(&self, req: &TicketRequest) -> Result<(String, oneshot::Receiver<Decision>)> {
        let code: String = req
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(6)
            .collect::<String>()
            .to_ascii_uppercase();
        anyhow::ensure!(code.len() == 6, "the request id is too short to name");
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            anyhow::ensure!(
                !p.contains_key(&code),
                "a request named {code} is already waiting"
            );
            p.insert(code.clone(), Pending { tx: Some(tx) });
        }
        let _ = self.events.send(Event::Ask {
            code: code.clone(),
            req: req.clone(),
        });
        Ok((code, rx))
    }

    /// The owner answered. False when nothing by that code is waiting
    /// (answered already, expired, or never asked).
    pub fn decide(&self, code: &str, d: Decision) -> bool {
        let code = code.trim().to_ascii_uppercase();
        let tx = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&code)
            .and_then(|p| p.tx.take());
        match tx {
            Some(tx) => tx.send(d).is_ok(),
            None => false,
        }
    }

    /// Take `code` off the desk and tell the legs how it ended. Once:
    /// a second close of the same code (the drop guard after an explicit
    /// close) says nothing.
    fn close(&self, code: &str, outcome: &str) {
        let was = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(code);
        if was.is_some() {
            let _ = self.events.send(Event::Settled {
                code: code.to_string(),
                outcome: outcome.to_string(),
            });
        }
    }

    /// Codes still waiting — for the legs' "which one?" answers.
    pub fn waiting(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    }
}

/// Closes a request whose gateway went away before the owner answered
/// (the connection dropped, so axum dropped the handler): without it the
/// question would stay open on the owner's chat, answerable, and an
/// answer would sign nothing for nobody.
struct Withdraw<'a> {
    desk: &'a Desk,
    code: String,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.desk.close(&self.code, "withdrawn");
    }
}

/// What the owner is shown: the call, in full up to a size a chat
/// message can carry, then counted.
pub fn describe(code: &str, req: &TicketRequest) -> String {
    format!(
        "veles ticket request {code}\n\n{}\n\nSign it? Answer only if you asked for this.",
        req.describe(1500)
    )
}

// ── the HTTP side a gateway talks to ─────────────────────────────────

struct App {
    cfg: SignerConfig,
    tokens: Vec<(String, String)>,
    key: SignerKey,
    desk: Arc<Desk>,
}

/// Serve until the process ends. The legs must already be running on
/// `desk` (see `veles signer run`), or requests wait out their time.
pub async fn serve(
    cfg: SignerConfig,
    gateway_tokens: Vec<(String, String)>,
    key: SignerKey,
    desk: Arc<Desk>,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    let app = Arc::new(App {
        cfg,
        tokens: gateway_tokens,
        key,
        desk,
    });
    let router = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sign", post(sign))
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({ "ok": true, "key_id": app.key.key_id() }))
}

#[derive(Deserialize)]
struct SignBody {
    request: String,
}

/// The gateway whose token this is, compared in constant time.
fn gateway_of<'a>(tokens: &'a [(String, String)], headers: &HeaderMap) -> Option<&'a str> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?
        .trim();
    tokens
        .iter()
        .find(|(_, t)| veles_core::authn::ct_eq(t, presented))
        .map(|(n, _)| n.as_str())
}

async fn sign(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SignBody>,
) -> (StatusCode, Json<Value>) {
    let Some(gateway) = gateway_of(&app.tokens, &headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unknown gateway token" })),
        );
    };
    let req = match TicketRequest::decode(&body.request) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    let now = chrono::Utc::now().timestamp();
    if req.exp <= now {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the request has expired" })),
        );
    }
    let (code, rx) = match app.desk.open(&req) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    let _withdraw = Withdraw {
        desk: &app.desk,
        code: code.clone(),
    };
    tracing::info!(
        "signer: request {code} from gateway {gateway}: {} on {} for {}",
        req.tool,
        req.target,
        req.origin
    );
    let wait = Duration::from_secs(
        app.cfg
            .wait_secs
            .min(u64::try_from(req.exp - now).unwrap_or(0)),
    );
    let answer = tokio::time::timeout(wait, rx).await;
    match answer {
        Ok(Ok(Decision::Sign { by })) => match app.key.sign(&req) {
            Ok(ticket) => {
                app.desk.close(&code, "signed");
                tracing::info!("signer: {code} signed, approved by {by}");
                (
                    StatusCode::OK,
                    Json(json!({ "ticket": ticket, "approved_by": by, "code": code })),
                )
            }
            Err(e) => {
                app.desk.close(&code, "failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("{e:#}") })),
                )
            }
        },
        Ok(Ok(Decision::Decline { by })) => {
            app.desk.close(&code, "declined");
            tracing::info!("signer: {code} declined by {by}");
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "declined": by, "code": code })),
            )
        }
        _ => {
            app.desk.close(&code, "expired");
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(
                    json!({ "error": format!("no answer from the owner in {}s", wait.as_secs()), "code": code }),
                ),
            )
        }
    }
}

/// A reply typed on a chat: `yes CODE` / `no CODE` (also sign/decline,
/// y/n, ok/deny). None for anything else.
pub fn parse_reply(text: &str) -> Option<(bool, String)> {
    let mut words = text.split_whitespace();
    let verb = words.next()?.to_ascii_lowercase();
    let code = words
        .next()?
        .trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if code.len() != 6 || words.next().is_some() {
        return None;
    }
    let yes = match verb.as_str() {
        "yes" | "y" | "sign" | "ok" | "approve" => true,
        "no" | "n" | "decline" | "deny" => false,
        _ => return None,
    };
    Some((yes, code.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests;
