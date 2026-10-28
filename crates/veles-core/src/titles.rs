//! Session title summarization — one-shot LLM call on the primary route.
//!
//! Untitled sessions get a provisional title immediately (first line,
//! truncated) so the sidebar is never raw ids; this then upgrades it to
//! a real summary of the first prompt. Cheap by construction: fires at
//! most once per session, on the configured default model, no tools.

use crate::config::VelesConfig;
use crate::providers::{build_routes, ChatMessage, ChatRequest};

const TITLE_PROMPT: &str = "Summarize the following user message as a very short \
chat session title. Maximum 6 words. No quotes, no trailing punctuation, no \
leading article. Reply with the title and nothing else.";

/// Returns a short title for `prompt`, or None when the call fails or
/// the model returns nothing usable. Never panics — callers fall back
/// to the truncated provisional title.
pub async fn summarize_title(cfg: &VelesConfig, prompt: &str) -> Option<String> {
    let routes = build_routes(cfg).ok()?;
    // S16: honor failover aux_fallback — titles shouldn't compete with
    // the brain on a throttled scope.
    let route = crate::auxroute::resolve_aux_route(cfg, &routes, None)?;
    if let Some(primary) = routes.first() {
        if route.provider != primary.provider || route.model != primary.model {
            crate::notes::record_aux_route("session titling", &route.provider, &route.model);
        }
    }
    let req = ChatRequest {
        model: route.model.clone(),
        messages: vec![ChatMessage::user(TITLE_PROMPT), ChatMessage::user(prompt)],
        tools: vec![],
        reasoning_effort: None,
        stream: false,
        replay_reasoning: false,
        cache_mode: crate::providers::CacheMode::Auto,
        extra_body: None,
    };
    match route.client.chat(&req, &route.provider).await {
        Ok(c) => {
            let t: String = c.message.content.trim().chars().take(64).collect();
            // Models love quoting their titles; strip the dressing.
            let t = t
                .trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '.')
                .trim()
                .to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Err(e) => {
            tracing::warn!("title summarization failed on {}: {e}", route.provider);
            None
        }
    }
}
