//! Review and revoke standing tool-approval grants.
//!
//! This is deliberately separate from pending `/api/asks`: a pending ask
//! is a decision still to make; a standing grant is a decision that keeps
//! applying. Three sources are listed: global rules (the store), the
//! session grants of every live agent in this process — desktop-hosted
//! AND gateway chat sessions (Telegram, IRC, …), which live in a second
//! agent map — and the config's `[approvals] allowlist`, read-only
//! (config.toml is its source of truth; the panel never rewrites it).

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::desktop_api::DesktopServer;
use crate::gateway::GatewayCore;
use veles_core::agent::Agent;
use veles_core::lockutil::LockOk;

/// How much of a global grant's note the panel shows. Notes written
/// before 0.2.0 held the whole call (review finding approvals-9).
const NOTE_CHARS: usize = 120;

pub(super) async fn view(State(srv): State<Arc<DesktopServer>>) -> (StatusCode, Json<Value>) {
    view_with(&srv, crate::gateway::global_core_arc().as_deref())
}

/// Every live agent holding session grants, labelled by surface. The
/// desktop's map and the gateway's are separate caches and may each hold
/// an agent for the same session id; both are listed, since each keeps
/// its own grants.
fn live_agents(srv: &DesktopServer, gateway: Option<&GatewayCore>) -> Vec<(String, Arc<Agent>)> {
    let mut out: Vec<(String, Arc<Agent>)> = srv
        .agents
        .lock_ok()
        .values()
        .map(|agent| ("desktop".to_string(), agent.clone()))
        .collect();
    if let Some(core) = gateway {
        out.extend(core.live_agents());
    }
    out
}

/// A stored note as the panel may show it: credential shapes redacted,
/// one line, bounded. Never null.
fn shown_note(note: Option<String>) -> String {
    let note = note.unwrap_or_default();
    let first = note.lines().next().unwrap_or("").trim();
    let redacted = veles_core::a2a::redact_outbound(first);
    if redacted.chars().count() > NOTE_CHARS {
        let mut cut: String = redacted.chars().take(NOTE_CHARS).collect();
        cut.push('…');
        cut
    } else {
        redacted
    }
}

pub(crate) fn view_with(
    srv: &DesktopServer,
    gateway: Option<&GatewayCore>,
) -> (StatusCode, Json<Value>) {
    let global = match srv.store.lock_ok().approval_rules() {
        Ok(rows) => rows
            .into_iter()
            .map(|(rule, note, created_at)| {
                json!({
                    "scope": "global", "rule_key": rule, "note": shown_note(note),
                    "created_at": created_at,
                })
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    };
    let mut sessions = Vec::new();
    for (surface, agent) in live_agents(srv, gateway) {
        for rule in agent.session_approval_rules() {
            sessions.push(json!({
                "scope": "session", "session_id": agent.session_id,
                "surface": surface, "rule_key": rule,
            }));
        }
    }
    let config: Vec<Value> = srv
        .cfg()
        .approvals
        .allowlist
        .iter()
        .map(|entry| json!({"scope": "config", "rule_key": entry, "read_only": true}))
        .collect();
    (
        StatusCode::OK,
        Json(json!({"global": global, "sessions": sessions, "config": config})),
    )
}

#[derive(Deserialize)]
pub(crate) struct RevokeBody {
    pub(crate) scope: String,
    pub(crate) rule_key: String,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

pub(super) async fn revoke(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<RevokeBody>,
) -> (StatusCode, Json<Value>) {
    revoke_with(&srv, crate::gateway::global_core_arc().as_deref(), body)
}

pub(crate) fn revoke_with(
    srv: &DesktopServer,
    gateway: Option<&GatewayCore>,
    body: RevokeBody,
) -> (StatusCode, Json<Value>) {
    if body.rule_key.is_empty() || body.rule_key.len() > 512 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"a valid exact rule_key is required"})),
        );
    }
    match body.scope.as_str() {
        "global" => match srv.store.lock_ok().approval_rule_remove(&body.rule_key) {
            Ok(1) => (
                StatusCode::OK,
                Json(json!({"revoked":true, "scope":"global", "rule_key":body.rule_key})),
            ),
            Ok(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"global grant not found or already revoked"})),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":e.to_string()})),
            ),
        },
        "session" => {
            let Some(session_id) = body.session_id.as_deref().filter(|id| !id.is_empty()) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"session_id is required for a session grant"})),
                );
            };
            // Every live agent of that session, on any surface: a grant
            // the panel listed must be revocable wherever it is held.
            let mut revoked = false;
            for (_, agent) in live_agents(srv, gateway) {
                if agent.session_id == session_id {
                    revoked |= agent.revoke_session_approval_rule(&body.rule_key);
                }
            }
            if revoked {
                (
                    StatusCode::OK,
                    Json(
                        json!({"revoked":true,"scope":"session","session_id":session_id,"rule_key":body.rule_key}),
                    ),
                )
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({"error":"session grant not found, already revoked, or session not live"}),
                    ),
                )
            }
        }
        "config" => (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error":"[approvals] allowlist entries are read-only here; remove them from config.toml"}),
            ),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"scope must be global or session"})),
        ),
    }
}
