//! Anthropic Message Threads beta: per-session, append-only wire state.
//!
//! The store is authoritative. A thread is only an optimization for requests
//! made from an unchanged local history; an edit, compaction or route change
//! starts a new create. Nothing here treats an Anthropic response id as a
//! replacement for veles' persisted conversation.

use crate::providers::{ChatMessage, ChatRequest, PluginRoute};

pub const BETA: &str = "message-threads-2026-08-12";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub route: String,
    pub previous_message_id: String,
    /// How much of the local history (which includes the system message)
    /// the thread already holds: everything sent, PLUS the reply stored
    /// under `previous_message_id`. Continue sends only what follows. A
    /// continue that starts with that stored reply is a documented error.
    pub sent: usize,
    /// Wire fingerprint of the history the thread holds, reply included,
    /// not just its length. An in-place edit, a local rewrite of the reply,
    /// or compaction must start a new create.
    pub sent_fingerprint: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub messages: Vec<ChatMessage>,
    pub previous_response_id: Option<String>,
    pub continuing: bool,
    /// The length of the full local request (`req.messages`), including the
    /// stored reply that `messages` leaves out.
    pub full_len: usize,
    pub req: ChatRequest,
}

fn fingerprint(req: &ChatRequest, _route: &PluginRoute) -> String {
    // Fixed fields per the API docs. max_tokens, stream, metadata and the
    // new messages are deliberately absent. Credentials and base_url are not
    // serialized into this fingerprint: the route key already isolates them.
    let fixed = serde_json::json!({
        "model": req.model,
        "reasoning_effort": req.reasoning_effort,
        "extra_body": req.extra_body,
        "tools": req.tools,
        "system": req.messages.first().filter(|m| m.role == "system").map(|m| {
            let mut one = req.clone();
            one.messages = vec![m.clone()];
            crate::providers::wire_body(&one)["messages"][0].clone()
        }),
        "cache_mode": format!("{:?}", req.cache_mode),
    });
    fixed.to_string()
}

fn route_key(route: &PluginRoute) -> String {
    serde_json::json!({
        "driver": route.driver,
        "base_url": route.base_url,
        "api_key": route.api_key,
        "endpoint_path": route.endpoint_path,
        "api_version": route.api_version,
        "auth_style": route.auth_style,
        "built_in_tools": route.built_in_tools,
        "cache_ttl": route.cache_ttl,
    })
    .to_string()
}

fn history_fingerprint(messages: &[ChatMessage], req: &ChatRequest) -> String {
    let mut projection = req.clone();
    projection.messages = messages.to_vec();
    projection.thread = None;
    crate::providers::wire_body(&projection)["messages"].to_string()
}

impl State {
    pub fn begin(req: &ChatRequest, route: &PluginRoute) -> Self {
        Self {
            route: route_key(route),
            previous_message_id: String::new(),
            sent: 0,
            sent_fingerprint: String::new(),
            fingerprint: fingerprint(req, route),
        }
    }

    pub fn prepare(
        &self,
        req: &ChatRequest,
        route: &PluginRoute,
        replayed: bool,
    ) -> Option<Prepared> {
        if self.fingerprint != fingerprint(req, route)
            || self.route != route_key(route)
            || self.previous_message_id.is_empty()
            || self.sent > req.messages.len()
            || self.sent_fingerprint != history_fingerprint(&req.messages[..self.sent], req)
            || replayed
        {
            return None;
        }
        // The new messages follow the stored reply, so they start with a
        // user turn or tool results. Nothing new, or another assistant
        // message first (a halted turn), is not a continuation.
        match req.messages.get(self.sent) {
            Some(first) if first.role != "assistant" => {}
            _ => return None,
        }
        Some(Prepared {
            messages: req.messages[self.sent..].to_vec(),
            previous_response_id: Some(self.previous_message_id.clone()),
            continuing: true,
            full_len: req.messages.len(),
            req: req.clone(),
        })
    }

    pub fn consume(
        &mut self,
        req: &ChatRequest,
        route: &PluginRoute,
        replayed: bool,
    ) -> Option<Prepared> {
        let prepared = self.prepare(req, route, replayed);
        if prepared.is_some() {
            self.previous_message_id.clear();
        }
        prepared
    }
}

pub fn prepare(
    state: Option<&State>,
    req: &ChatRequest,
    route: &PluginRoute,
    replayed: bool,
) -> Prepared {
    if let Some(prepared) = state.and_then(|s| s.prepare(req, route, replayed)) {
        return prepared;
    }
    Prepared {
        messages: req.messages.clone(),
        previous_response_id: None,
        continuing: false,
        full_len: req.messages.len(),
        req: req.clone(),
    }
}

/// Record only a complete response. A refusal, a fallback-model response,
/// or a missing id is not a continuation point. A length-limited tool call
/// may be unstorable upstream, so conservatively start a new create next.
/// A paused turn (`pause_turn`) resumes only with an empty continue, but
/// the engine ends its turn there and sends a new user message next, which
/// the API rejects while the turn is paused; so it starts a new create.
///
/// `req` is the FULL local request. The thread then holds it plus the reply
/// in `response`, which local history is expected to carry next.
pub fn complete(
    req: &ChatRequest,
    route: &PluginRoute,
    response: &crate::providers::Completion,
) -> Option<State> {
    if !response.response_complete {
        return None;
    }
    let id = response.response_id.as_deref()?.trim();
    let stop = response.native_stop_reason.as_deref().unwrap_or("");
    if id.is_empty()
        || stop == "refusal"
        || stop == "max_tokens"
        || stop == "pause_turn"
        || response.fallback_message.is_some()
    {
        return None;
    }
    let mut held = req.messages.clone();
    held.push(response.message.clone());
    Some(State {
        sent: held.len(),
        sent_fingerprint: history_fingerprint(&held, req),
        previous_message_id: id.into(),
        ..State::begin(req, route)
    })
}

/// [`complete`] for a request made through [`prepare`]: the state is
/// computed from the full local request, never from the suffix sent.
pub fn record(
    route: &PluginRoute,
    response: &crate::providers::Completion,
    prepared: Prepared,
) -> Option<State> {
    complete(&prepared.req, route, response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-5-5".into(),
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    ..ChatMessage::user("s")
                },
                ChatMessage::user("first"),
            ],
            tools: vec![serde_json::json!({"name": "terminal"})],
            reasoning_effort: Some("max".into()),
            stream: true,
            replay_reasoning: false,
            cache_mode: crate::providers::CacheMode::Explicit,
            extra_body: None,
            thread: None,
        }
    }

    fn route() -> PluginRoute {
        PluginRoute {
            driver: "anthropic-messages".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "not-in-fingerprint".into(),
            ..Default::default()
        }
    }

    fn completion(id: Option<&str>, stop: &str, fallback: bool) -> crate::providers::Completion {
        crate::providers::Completion {
            message: ChatMessage::assistant("answer", None),
            model: "claude-sonnet-5-5".into(),
            provider: "anthropic".into(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            response_complete: true,
            response_id: id.map(str::to_string),
            stop_reason: Some(stop.into()),
            native_stop_reason: Some(stop.into()),
            fallback_message: fallback.then(|| "claude-haiku".into()),
        }
    }

    #[test]
    fn create_then_continue_sends_only_the_new_suffix() {
        let first = req();
        let r = route();
        let made = prepare(None, &first, &r, false);
        assert!(!made.continuing);
        assert_eq!(made.messages.len(), 2);
        let state =
            complete(&first, &r, &completion(Some("msg_1"), "end_turn", false)).expect("storable");
        let mut later = first.clone();
        later.messages.push(ChatMessage::assistant("answer", None));
        later.messages.push(ChatMessage::user("second"));
        let next = state.prepare(&later, &r, false).expect("continue");
        // The thread already stored "answer" under msg_1. Sending it again
        // is the documented error ("a continue request that starts with the
        // previous response returns an error"), so only the new user turn
        // goes out.
        assert_eq!(next.messages.len(), 1, "{:?}", next.messages);
        assert_eq!(next.messages[0].role, "user");
        assert_eq!(next.messages[0].content, "second");
        assert_eq!(next.previous_response_id.as_deref(), Some("msg_1"));
    }

    /// The reply the thread stored and the one in local history must be the
    /// same message. A local rewrite of it (a hook, a stripped think tag)
    /// makes the thread's copy wrong, so the store wins with a new create.
    #[test]
    fn a_locally_rewritten_reply_forces_a_new_create() {
        let first = req();
        let r = route();
        let state =
            complete(&first, &r, &completion(Some("msg_1"), "end_turn", false)).expect("state");
        let mut later = first.clone();
        later
            .messages
            .push(ChatMessage::assistant("answer, rewritten locally", None));
        later.messages.push(ChatMessage::user("second"));
        assert!(state.prepare(&later, &r, false).is_none());
        // A history without the reply at all (an interrupted turn that
        // persisted nothing) cannot continue either.
        let mut missing = first.clone();
        missing.messages.push(ChatMessage::user("second"));
        assert!(state.prepare(&missing, &r, false).is_none());
        // Nor can a suffix that is empty or itself starts with an assistant
        // message: the thread already ends on the stored reply.
        let mut nothing_new = first.clone();
        nothing_new
            .messages
            .push(ChatMessage::assistant("answer", None));
        assert!(state.prepare(&nothing_new, &r, false).is_none());
        let mut two_replies = nothing_new.clone();
        two_replies
            .messages
            .push(ChatMessage::assistant("halted", None));
        assert!(state.prepare(&two_replies, &r, false).is_none());
    }

    #[test]
    fn changed_fixed_fields_and_a_replay_create_instead() {
        let first = req();
        let r = route();
        let state =
            complete(&first, &r, &completion(Some("msg_1"), "end_turn", false)).expect("state");
        let mut changed = first.clone();
        changed.reasoning_effort = Some("high".into());
        assert!(state.prepare(&changed, &r, false).is_none());
        assert!(!prepare(Some(&state), &first, &r, true).continuing);
        let mut shorter = first.clone();
        shorter.messages.pop();
        assert!(state.prepare(&shorter, &r, false).is_none());
    }

    #[test]
    fn a_cursor_is_consumed_once_and_records_the_full_request_length() {
        let first = req();
        let r = route();
        let mut state =
            complete(&first, &r, &completion(Some("msg_1"), "end_turn", false)).expect("state");
        let mut later = first.clone();
        later.messages.push(ChatMessage::assistant("answer", None));
        later.messages.push(ChatMessage::user("second"));
        let next = state.consume(&later, &r, false).expect("continue");
        assert_eq!(next.full_len, 4);
        assert!(state.prepare(&later, &r, false).is_none());
        assert!(state.consume(&later, &r, false).is_none());
        let state =
            record(&r, &completion(Some("msg_2"), "end_turn", false), next).expect("next cursor");
        // The four messages of the request plus the reply the thread stored.
        assert_eq!(state.sent, 5);
        assert_eq!(state.previous_message_id, "msg_2");
    }

    #[test]
    fn an_in_place_history_edit_forces_a_new_create() {
        let first = req();
        let r = route();
        let state =
            complete(&first, &r, &completion(Some("msg_1"), "end_turn", false)).expect("state");
        let mut edited = first.clone();
        edited.messages[1].content = "edited after the response".into();
        edited.messages.push(ChatMessage::assistant("answer", None));
        edited.messages.push(ChatMessage::user("second"));
        assert!(state.prepare(&edited, &r, false).is_none());
        assert!(!prepare(Some(&state), &edited, &r, false).continuing);
    }

    #[test]
    fn incomplete_streams_never_become_a_cursor() {
        let mut partial = completion(Some("msg_1"), "end_turn", false);
        partial.response_complete = false;
        assert!(complete(&req(), &route(), &partial).is_none());
    }

    #[test]
    fn unstorable_responses_never_become_a_cursor() {
        let first = req();
        for (response, stop) in [
            (completion(Some("msg_1"), "refusal", false), "refusal"),
            (completion(Some("msg_1"), "max_tokens", false), "max_tokens"),
            (completion(Some("msg_1"), "end_turn", true), "fallback"),
            (completion(None, "end_turn", false), "missing id"),
            // A paused turn resumes only with an EMPTY continue; the engine
            // ends the turn on it and sends a new user message next, which
            // the API rejects while the turn is paused.
            (completion(Some("msg_1"), "pause_turn", false), "pause_turn"),
        ] {
            assert!(
                complete(&first, &route(), &response).is_none(),
                "{stop} must not become a continuation point"
            );
        }
        assert!(complete(
            &first,
            &route(),
            &completion(Some("msg_1"), "end_turn", false)
        )
        .is_some());
    }
}
