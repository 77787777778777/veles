//! OpenAI-compatible local proxy — a drop-in /v1/chat/completions endpoint
//! that forwards to veles' configured provider chain (with fallback).
//!
//! #34 in the feature matrix. Runs as axum routes alongside the gateway.
//! Any OpenAI-compatible client (aichat, sgpt, opencode, etc.) can point
//! at http://localhost:8642/v1/chat/completions and get veles' provider
//! routing and fallback chain.
//!
//! This is a thin forwarder: receives an OpenAI-format request, resolves
//! the model+provider via veles config, adds the real API key, and
//! proxies the response (streaming or non-streaming). The proxy
//! preserves the SSE byte stream for streaming requests — no buffering.

use std::net::SocketAddr;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;

use crate::config::VelesConfig;
use crate::providers::{build_routes, Route};

/// Build the proxy router. Mount alongside the existing gateway API.
pub fn proxy_router() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
}

/// `platforms.proxy.enabled` — OFF by default: this endpoint answers
/// with the configured provider's real API key behind it, so it is
/// opt-in the way A2A and the desktop API are.
pub fn proxy_enabled(cfg: &VelesConfig) -> bool {
    cfg.platforms
        .get("proxy")
        .and_then(|a| a.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn proxy_port(cfg: &VelesConfig) -> u16 {
    cfg.platforms
        .get("proxy")
        .and_then(|a| a.get("port"))
        .and_then(|p| p.as_u64())
        .unwrap_or(8642) as u16
}

/// Run the proxy server. Spawned by the gateway when enabled; shape
/// matches a2a_server::serve so failures log without taking the
/// gateway down.
pub async fn serve(cfg: &VelesConfig) -> anyhow::Result<()> {
    let host = crate::a2a::resolve_bind_host(cfg);
    let port = proxy_port(cfg);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    tracing::info!("OpenAI-compatible proxy listening on http://{addr}/v1/chat/completions");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, proxy_router()).await?;
    Ok(())
}

async fn chat_completions(headers: HeaderMap, body: axum::Json<Value>) -> Response {
    let req = body.0;

    // Authenticate: accept any bearer token if VELES_PROXY_TOKEN is set.
    if let Some(expected) = crate::secrets::resolve("VELES_PROXY_TOKEN") {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = auth.strip_prefix("Bearer ").unwrap_or("");
        if token != expected {
            return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    }

    let model = req["model"].as_str().unwrap_or("default").to_string();
    let is_stream = req["stream"].as_bool().unwrap_or(false);

    let cfg = match crate::config::load() {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("config: {e}")).into_response()
        }
    };

    let (resolved_model, route) = match resolve_model_and_route(&cfg, &model) {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "no routes configured — check model.provider in config",
            )
                .into_response()
        }
    };

    // This proxy forwards an incoming chat-completions body VERBATIM —
    // that is only an honest thing to do to a chat-completions route.
    // A responses route wants a different endpoint and a different
    // body shape (input items, flat tools); re-posting the caller's
    // body there would 400 at best and silently mis-translate at
    // worst. The loud refusal is the same posture config::load takes
    // on unimplemented api_modes.
    if matches!(route.client, crate::providers::RouteClient::Responses(_)) {
        return (
            StatusCode::BAD_GATEWAY,
            format!(
                "route \"{model}\" speaks api_mode = \"responses\"; this proxy \
                 forwards chat_completions bodies verbatim and cannot translate \
                 between the two wires"
            ),
        )
            .into_response();
    }
    // A plugin route has no URL to forward TO. The driver owns the
    // transport — it might be a socket, a subprocess, or something with
    // no wire at all — so there is nothing this proxy could re-post the
    // caller's body to. Same loud refusal, same reason: better than
    // inventing an endpoint out of a `base_url` that is documented as
    // opaque.
    if let crate::providers::RouteClient::Plugin(p) = &route.client {
        return (
            StatusCode::BAD_GATEWAY,
            format!(
                "route \"{model}\" speaks api_mode = \"plugin\" (driver \"{}\"); \
                 this proxy forwards chat_completions bodies to an HTTP endpoint, \
                 and a driver owns its own transport — there is nothing to forward to",
                p.driver
            ),
        )
            .into_response();
    }

    let url = format!(
        "{}/chat/completions",
        route.client.base_url().trim_end_matches('/')
    );

    let mut upstream = req.clone();
    upstream["model"] = serde_json::Value::String(resolved_model.clone());
    if let Some(obj) = upstream.as_object_mut() {
        obj.remove("provider");
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .unwrap();

    let resp = match client
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", route.client.api_key()),
        )
        .header("Content-Type", "application/json")
        .json(&upstream)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);

    if is_stream {
        // Stream the SSE bytes directly from upstream to client.
        let stream = resp.bytes_stream();
        let body = Body::from_stream(stream);
        Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .unwrap()
    } else {
        // Non-streaming: read the full body and return it.
        let body = resp.text().await.unwrap_or_default();
        (status, body).into_response()
    }
}

async fn list_models() -> impl IntoResponse {
    let cfg = match crate::config::load() {
        Ok(c) => c,
        Err(_) => {
            return axum::Json(serde_json::json!({
                "object": "list",
                "data": []
            }));
        }
    };

    let mut models = Vec::new();
    for route in build_routes(&cfg).unwrap_or_default() {
        models.push(serde_json::json!({
            "id": route.model,
            "object": "model",
            "owned_by": route.provider,
        }));
    }

    // Warm discovery FIRST, then merge. A PAYG key against a router
    // serves hundreds of models on a catalog that changes weekly, and
    // `model_names()` alone could only ever return what somebody typed
    // by hand. Best-effort: a provider whose catalog will not answer
    // falls back to exactly the curated list it had before.
    crate::discovery::warm_all(&cfg).await;
    for entry in &cfg.custom_providers {
        for m in crate::discovery::merged_cached(entry) {
            models.push(serde_json::json!({
                "id": m.id,
                "object": "model",
                "owned_by": entry.name,
                "context_length": m.context_length,
                "discovered": m.discovered,
            }));
        }
    }

    axum::Json(serde_json::json!({
        "object": "list",
        "data": models
    }))
}

/// Resolve a model name from the request to a veles route. `None` when
/// no routes are configured — a request-time 503, never a panic: the
/// proxy is a long-lived server and one bad config must not kill it.
fn resolve_model_and_route(cfg: &VelesConfig, requested: &str) -> Option<(String, Route)> {
    let routes = build_routes(cfg).unwrap_or_default();
    if routes.is_empty() {
        return None;
    }

    // "default" → primary
    if requested == "default" || requested.is_empty() {
        let primary = &routes[0];
        return Some((primary.model.clone(), primary.clone()));
    }

    // Check if any route already serves this model
    for r in &routes {
        if r.model == requested {
            return Some((requested.to_string(), r.clone()));
        }
    }

    // Model not in routes — use primary provider but pass the model through
    let primary = &routes[0];
    Some((requested.to_string(), primary.clone()))
}
