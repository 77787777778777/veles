//! The OpenAI Responses API — the second wire protocol this build
//! speaks (`api_mode = "responses"`).
//!
//! WHY THIS EXISTS
//!
//! The Model Studio Token Plan's "harness tools" — web_search,
//! code_interpreter, web_extractor, web_search_image, image_search —
//! are SERVER-SIDE tools the endpoint runs for you, billed per
//! successful invocation. They are only reachable on the Responses
//! wire; chat_completions has no shape for them (PENDING 2026-09-01
//! identified this; 2026-09-03's live probes settled the wire shape
//! and it is written up in COVERAGE.md).
//!
//! WHAT THE PROBES SETTLED (trust this over the official docs, which
//! were wrong twice)
//!
//!  * Endpoint is `POST {base_url}/responses` — the config's base_url
//!    already ends in `/compatible-mode/v1` on the token-plan routes.
//!  * Function tools use a FLAT schema: `{type, name, description,
//!    parameters}` — NOT chat_completions' nested `{function: {...}}`.
//!    Calls come back as output items `{type:"function_call", call_id,
//!    name, arguments}`; results go back as `{type:"function_call_output",
//!    call_id, output}` input items.
//!  * The docs' rule that a `function_call_output` must IMMEDIATELY
//!    follow its `function_call` or the API errors: NOT enforced. An
//!    orphan output was silently coerced into context (HTTP 200). veles
//!    still emits them adjacent, because that is what the mapping
//!    produces naturally.
//!  * Streaming function-call argument deltas arrive as
//!    `response.function_call_arguments.delta` — the OpenAI-standard
//!    name, NOT the `response.custom_tool_call_input.delta` the docs
//!    claim. Reasoning deltas are `response.reasoning_text.delta`.
//!  * Built-in tool activity: status events
//!    (`response.web_search_call.in_progress|searching|completed`)
//!    carry only an item_id; the substance arrives in the item on
//!    `response.output_item.done`.
//!  * Usage is inside `response.completed` → `response.usage` with
//!    `input_tokens` / `output_tokens` / `input_tokens_details.
//!    cached_tokens` (+ `x_tools` per-tool invocation counts). There is
//!    no `stream_options` here — undocumented parameters are silently
//!    ignored, per the reference page itself.
//!  * Caching is OPT-IN: the `x-dashscope-session-cache: enable`
//!    header, required on BOTH creation and hit, ≥1024 tokens, 5-minute
//!    ephemeral TTL. There is NO implicit caching on this endpoint.
//!    The hit survives appended turns (the key is anchored on the
//!    stable prefix), which is exactly what a stateless client
//!    re-sending a byte-stable history needs — measured 1831/1858
//!    tokens cached on the second round of a growing conversation.
//!
//! STATELESS BY DESIGN
//!
//! `previous_response_id` / `conversation` exist on this endpoint and
//! save nothing: the doc itself says `input_tokens` still includes the
//! prior-turn context with them — the only discount is a cache hit, and
//! the stateless re-send gets that too via the header above. What they
//! WOULD cost is control: veles compacts, `/undo`s, checkpoints and
//! resumes its own history, none of which can be done to a server-side
//! chain with a 7-day TTL — and a failover hop to a chat_completions
//! route cannot take the chain with it at all. So this client maps the
//! engine's `Vec<ChatMessage>` onto `input` items every round, the same
//! contract as chat_completions, and `run_turn` never knows the
//! difference: function_call items become OpenAI-shaped `tool_calls` on
//! the `Completion` message, which is the shape `run_turn` reads.
//!
//! WHAT THIS CLIENT DELIBERATELY DROPS
//!
//!  * Reasoning replay. Responses can carry `reasoning` items back
//!    across turns, but only by their server-assigned id, and veles
//!    stores reasoning TEXT, not ids. History goes out without them —
//!    accepted input, billed as new context, same as every other client
//!    that keeps its own transcript.
//!  * `store: false`. Responses are stored server-side by default;
//!    nothing here references them afterwards, but nothing here is
//!    harmed by it either, and opting out would forfeit
//!    `previous_response_id` for anyone who ever wants it.

use serde_json::{json, Value};
use std::sync::Arc;

use crate::providers::{
    CancelToken, ChatMessage, ChatRequest, Completion, ProviderError, StreamEvent, UserAgentProfile,
};

#[derive(Debug, Clone)]
pub struct ResponsesClient {
    /// Arc-backed, same pool-sharing rationale as `ProviderClient`.
    http: Arc<reqwest::Client>,
    pub base_url: String,
    pub api_key: String,
    /// Wire identity (UA + X-Stainless-* headers) — same policy as the
    /// chat-completions client.
    pub ua: UserAgentProfile,
    /// Server-side tools declared on every request from this route
    /// (`ProviderEntry.built_in_tools`). Empty on chat_completions
    /// routes, where the concept does not exist.
    built_in_tools: Vec<String>,
}

impl ResponsesClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        ua: UserAgentProfile,
        built_in_tools: Vec<String>,
    ) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .user_agent(&ua.user_agent);
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &ua.extra_headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                headers.insert(name, value);
            }
        }
        builder = builder.default_headers(headers);
        Ok(Self {
            http: Arc::new(builder.build()?),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            ua,
            built_in_tools,
        })
    }

    /// Test hook, same contract as `ProviderClient::client_id`.
    #[cfg(test)]
    pub fn client_id(&self) -> usize {
        Arc::as_ptr(&self.http) as usize
    }

    fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    // ── request body ─────────────────────────────────────────────────

    pub(crate) fn build_body(&self, req: &ChatRequest) -> Value {
        let input = history_to_input(&req.messages);
        let mut body = json!({
            "model": req.model,
            "input": input,
            "stream": req.stream,
        });

        let tools = tools_for_responses(&req.tools, &self.built_in_tools);
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(re) = &req.reasoning_effort {
            body["reasoning"] = json!({ "effort": re });
        }
        merge_extra_body(&mut body, req.extra_body.as_ref());
        body
    }

    /// The session-cache header rides `cache_mode`: `Off` sends
    /// nothing, anything else asks for the ephemeral session cache.
    fn request_builder(&self, body: &Value, req: &ChatRequest) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(body);
        // Caching on this endpoint is opt-in by header (probed: no
        // implicit cache exists). Auto and Explicit both ask for it —
        // there are no breakpoints to emit on this wire, so the two
        // coincide; Off stays silent.
        if req.cache_mode != crate::providers::CacheMode::Off {
            rb = rb.header("x-dashscope-session-cache", "enable");
        }
        rb
    }

    // ── calls ────────────────────────────────────────────────────────

    /// Non-streaming call (compaction, titles, the Tcl bridge).
    pub async fn chat(
        &self,
        req: &ChatRequest,
        provider_name: &str,
    ) -> Result<Completion, ProviderError> {
        let body = self.build_body(req);
        let resp = self
            .request_builder(&body, req)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let v = classify_and_read(resp).await?;
        let (prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens) =
            responses_usage(&v["usage"]);
        Ok(Completion {
            message: parse_output(&v["output"]),
            model: req.model.clone(),
            provider: provider_name.to_string(),
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
            reasoning_tokens: responses_reasoning_tokens(&v["usage"]),
        })
    }

    /// Streaming call. Same event contract as `ProviderClient::
    /// chat_stream`: `Route` first, then deltas, exactly one `Done` or
    /// `Cancelled`. Built-in tool activity rides the SAME ToolStart /
    /// ToolEnd events a local tool round emits, so every surface
    /// renders "web_search … done" with zero new rendering code.
    pub async fn chat_stream<F>(
        &self,
        req: &ChatRequest,
        provider_name: &str,
        cancel: Option<CancelToken>,
        mut on_event: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamEvent),
    {
        use futures::StreamExt;

        let body = self.build_body(req);
        let resp = self
            .request_builder(&body, req)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 429 {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::RateLimited(body));
        }
        if (500..600).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Server { status, body });
        }
        if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Client { status, body });
        }

        on_event(StreamEvent::Route {
            provider: provider_name.to_string(),
            model: req.model.clone(),
            reasoning_effort: req.reasoning_effort.clone(),
        });

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut cancelled = false;
        // function_call items accumulate keyed by item_id; they land in
        // the tool_calls vec in completion order.
        let mut pending: Vec<PendingCall> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut prompt_tokens: Option<u64> = None;
        let mut completion_tokens: Option<u64> = None;
        let mut cached_tokens: Option<u64> = None;
        let mut cache_write_tokens: Option<u64> = None;
        let mut reasoning_tokens: Option<u64> = None;
        // The canonical final message, rebuilt from response.completed
        // when it arrives (single source of truth); deltas feed the UI
        // meanwhile.
        let mut final_message: Option<ChatMessage> = None;
        let mut finished = false;

        // Tail-safe: a final event hung WITHOUT its terminating blank
        // line is promoted at EOF; eventsource-stream dropped it and
        // every reply lost its tail when the provider closed early.
        let mut stream = crate::sse::tail_safe(resp.bytes_stream());
        while let Some(ev) = stream.next().await {
            if let Some(tok) = &cancel {
                if tok.is_cancelled() {
                    cancelled = true;
                    break;
                }
            }
            let ev = ev.map_err(|e| ProviderError::Network(format!("sse: {e}")))?;
            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                continue;
            };
            let etype = v["type"].as_str().unwrap_or("");
            // A terminal event: the provider said the response is over
            // (`completed` carries the canonical message; `incomplete` and
            // `failed` end it too). A stream that closes without one was
            // CUT — see the check after the loop.
            if matches!(
                etype,
                "response.completed" | "response.incomplete" | "response.failed"
            ) {
                finished = true;
            }
            match etype {
                "response.output_text.delta" => {
                    if let Some(t) = v["delta"].as_str() {
                        if !t.is_empty() {
                            content.push_str(t);
                            on_event(StreamEvent::Delta(t.to_string()));
                        }
                    }
                }
                "response.reasoning_text.delta" => {
                    if let Some(t) = v["delta"].as_str() {
                        if !t.is_empty() {
                            reasoning.push_str(t);
                            on_event(StreamEvent::ReasoningDelta(t.to_string()));
                        }
                    }
                }
                "response.output_item.added" => {
                    let item = &v["item"];
                    match item["type"].as_str().unwrap_or("") {
                        "function_call" => pending.push(PendingCall {
                            item_id: item["id"].as_str().unwrap_or("").to_string(),
                            call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                            name: item["name"].as_str().unwrap_or("").to_string(),
                            arguments: String::new(),
                        }),
                        t if is_builtin_call_type(t) => {
                            let name = builtin_display_name(t, item);
                            on_event(StreamEvent::ToolStart(name, String::new()));
                        }
                        _ => {}
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(d) = v["delta"].as_str() {
                        let item_id = v["item_id"].as_str().unwrap_or("");
                        if let Some(p) = pending.iter_mut().find(|p| p.item_id == item_id) {
                            p.arguments.push_str(d);
                        }
                    }
                }
                "response.output_item.done" => {
                    let item = &v["item"];
                    match item["type"].as_str().unwrap_or("") {
                        "function_call" => {
                            // Prefer the done item's own fields over the
                            // delta accumulation (it is the complete
                            // record); fall back to pending when an
                            // endpoint skips the done payload.
                            let item_id = item["id"].as_str().unwrap_or("");
                            let args =
                                item["arguments"].as_str().map(str::to_string).or_else(|| {
                                    pending
                                        .iter()
                                        .find(|p| p.item_id == item_id)
                                        .map(|p| p.arguments.clone())
                                });
                            let name = item["name"]
                                .as_str()
                                .map(str::to_string)
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    pending
                                        .iter()
                                        .find(|p| p.item_id == item_id)
                                        .map(|p| p.name.clone())
                                })
                                .unwrap_or_default();
                            let call_id = item["call_id"]
                                .as_str()
                                .map(str::to_string)
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    pending
                                        .iter()
                                        .find(|p| p.item_id == item_id)
                                        .map(|p| p.call_id.clone())
                                })
                                .unwrap_or_default();
                            pending.retain(|p| p.item_id != item_id);
                            // The shape run_turn reads: id / function.
                            // name / arguments — call_id IS the id the
                            // next round's function_call_output
                            // correlates on.
                            tool_calls.push(json!({
                                "id": call_id,
                                "type": "function",
                                "function": { "name": name, "arguments": args.unwrap_or_default() }
                            }));
                        }
                        t if is_builtin_call_type(t) => {
                            let name = builtin_display_name(t, item);
                            on_event(StreamEvent::ToolEnd(name, render_builtin_result(t, item)));
                        }
                        _ => {}
                    }
                }
                "response.completed" => {
                    let response = &v["response"];
                    let (p, c, ca, cw) = responses_usage(&response["usage"]);
                    prompt_tokens = p.or(prompt_tokens);
                    completion_tokens = c.or(completion_tokens);
                    cached_tokens = ca.or(cached_tokens);
                    cache_write_tokens = cw.or(cache_write_tokens);
                    reasoning_tokens =
                        responses_reasoning_tokens(&response["usage"]).or(reasoning_tokens);
                    if !cancelled {
                        final_message = Some(parse_output(&response["output"]));
                    }
                }
                // response.created / in_progress / content_part.* /
                // web_search_call status events carry nothing the
                // events above will not deliver in full.
                _ => {}
            }
        }

        // The token is checked per event ARRIVING; a stream that ends
        // in the same instant the user hits Ctrl-C would otherwise slip
        // through as a Done. The turn is being stopped either way.
        if !cancelled {
            if let Some(tok) = &cancel {
                if tok.is_cancelled() {
                    cancelled = true;
                }
            }
        }

        // The chat-completions client's rule (`ProviderClient::chat_stream`):
        // a CUT stream with nothing usable in it is an error the caller
        // retries, not a finished reply with no text; a cut after answer
        // text keeps the text and says so.
        if !cancelled && !finished {
            if content.trim().is_empty() && tool_calls.is_empty() {
                return Err(ProviderError::Truncated(format!(
                    "{provider_name}/{} closed the stream before it finished \
                     ({} chars of reasoning, no answer)",
                    req.model,
                    reasoning.chars().count()
                )));
            }
            tracing::warn!(
                "{provider_name}/{}: the stream closed without a terminal event; \
                 keeping the {} chars that arrived",
                req.model,
                content.chars().count()
            );
        }

        let mut message = final_message.unwrap_or_else(|| {
            let mut m = ChatMessage::assistant(
                &content,
                if reasoning.is_empty() {
                    None
                } else {
                    Some(reasoning)
                },
            );
            if !tool_calls.is_empty() {
                m.tool_calls = Some(json!(tool_calls));
            }
            m
        });
        if cancelled {
            // Drop partial tool_calls, keep the text — same posture as
            // the chat-completions client.
            message.tool_calls = None;
            on_event(StreamEvent::Cancelled);
        } else {
            on_event(StreamEvent::Done(message.clone()));
        }
        Ok(Completion {
            message,
            model: req.model.clone(),
            provider: provider_name.to_string(),
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
            reasoning_tokens,
        })
    }
}

struct PendingCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// Read the body of a non-2xx as a classified `ProviderError`. Shared
/// by both calls; identical status semantics to the chat-completions
/// client so the failover chain treats both wires alike.
async fn classify_and_read(resp: reqwest::Response) -> Result<Value, ProviderError> {
    let status = resp.status().as_u16();
    if status == 429 || (400..600).contains(&status) {
        let body = resp.text().await.unwrap_or_default();
        return Err(if status == 429 {
            ProviderError::RateLimited(body)
        } else if (500..600).contains(&status) {
            ProviderError::Server { status, body }
        } else {
            ProviderError::Client { status, body }
        });
    }
    resp.json()
        .await
        .map_err(|e| ProviderError::Network(format!("bad json: {e}")))
}

/// The engine's history → Responses `input` items.
///
///  * system / user / assistant-without-calls → chat-style
///    `{role, content}` items (the documented EasyInputMessage shape;
///    user attachments become `input_text` / `input_image` parts).
///  * assistant WITH tool_calls → an optional assistant message item
///    for any text, then each call INTERLEAVED with the result that
///    answers it: `function_call`, `function_call_output`, next call,
///    its output. The wire demands each output immediately follow its
///    call; a multi-call round is `call, out, call, out` — not
///    `call, call, out, out`, which is what transcript order naively
///    read would produce. The live endpoint turned out to tolerate
///    even an ORPHAN output, but correctness is not a matter of what
///    you can get away with.
///  * a role="tool" result that matches no pending call still goes out
///    as an output item — dropping history is not ours to decide.
pub(crate) fn history_to_input(messages: &[ChatMessage]) -> Vec<Value> {
    let mut input: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i];
        if m.role == "tool" {
            // Orphan result (no preceding call named it) — emit it
            // anyway; the endpoint coerces rather than rejects.
            if let Some(meta) = &m.tool_meta {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": meta.tool_call_id,
                    "output": m.content,
                }));
            }
            i += 1;
            continue;
        }
        if m.role == "user" && !m.attachments.is_empty() {
            let mut parts: Vec<Value> = Vec::new();
            if !m.content.is_empty() {
                parts.push(json!({ "type": "input_text", "text": m.content }));
            }
            for a in &m.attachments {
                parts.push(json!({ "type": "input_image", "image_url": a.data_url }));
            }
            input.push(json!({ "role": "user", "content": parts }));
            i += 1;
            continue;
        }
        let calls = m
            .tool_calls
            .as_ref()
            .and_then(|t| t.as_array())
            .filter(|a| !a.is_empty());
        if let Some(calls) = calls {
            if !m.content.is_empty() {
                input.push(json!({ "role": "assistant", "content": m.content }));
            }
            // The contiguous run of results answering these calls.
            let mut results: Vec<&ChatMessage> = Vec::new();
            let mut j = i + 1;
            while j < messages.len() && messages[j].role == "tool" {
                results.push(&messages[j]);
                j += 1;
            }
            for call in calls {
                let call_id = call["id"].as_str().unwrap_or("");
                input.push(json!({
                    "type": "function_call",
                    "name": call["function"]["name"].as_str().unwrap_or(""),
                    "arguments": call["function"]["arguments"].as_str().unwrap_or(""),
                    "call_id": call_id,
                }));
                if let Some(pos) = results.iter().position(|r| {
                    r.tool_meta
                        .as_ref()
                        .is_some_and(|t| t.tool_call_id == call_id)
                }) {
                    let r = results.remove(pos);
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": r.content,
                    }));
                }
            }
            // Results that matched no call (a refusal path that kept
            // the id legible, or a corrupted transcript) still ride.
            for r in results {
                if let Some(meta) = &r.tool_meta {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": meta.tool_call_id,
                        "output": r.content,
                    }));
                }
            }
            i = j;
            continue;
        }
        input.push(json!({ "role": m.role, "content": m.content }));
        i += 1;
    }
    input
}

/// Tool array for the Responses wire: client functions flattened from
/// chat_completions' nested shape to the flat one this endpoint wants,
/// followed by the route's built-in tool declarations.
pub(crate) fn tools_for_responses(openai_schemas: &[Value], built_ins: &[String]) -> Vec<Value> {
    let mut tools: Vec<Value> = openai_schemas
        .iter()
        .filter_map(|t| {
            let f = &t["function"];
            let name = f["name"].as_str()?;
            let mut v = json!({
                "type": "function",
                "name": name,
                "description": f["description"].as_str().unwrap_or(""),
            });
            if !f["parameters"].is_null() {
                v["parameters"] = f["parameters"].clone();
            }
            Some(v)
        })
        .collect();
    for b in built_ins {
        tools.push(json!({ "type": b }));
    }
    tools
}

/// extra_body passthrough with this wire's own denylist — `input`
/// replaces `messages`, and `stream_options` does not exist here
/// (undocumented parameters are silently ignored by the endpoint,
/// which is worse than a 400: it is a silent no-op).
fn merge_extra_body(body: &mut Value, extra: Option<&Value>) {
    let Some(extra) = extra else { return };
    let Value::Object(map) = extra else {
        tracing::warn!("extra_body must be an object; ignoring");
        return;
    };
    const DENYLIST: &[&str] = &["input", "model", "stream", "tools"];
    for (k, v) in map {
        if DENYLIST.contains(&k.as_str()) {
            tracing::warn!("extra_body key '{k}' is transport-managed; dropped");
            continue;
        }
        body[k] = v.clone();
    }
}

/// Assemble the engine's `ChatMessage` from a Responses `output`
/// array. message → content, reasoning → reasoning text (display; the
/// ids needed to REPLAY it are not stored), function_call →
/// OpenAI-shaped tool_calls entries. Built-in call items are already
/// acted on server-side — their results live in the message text.
pub(crate) fn parse_output(output: &Value) -> ChatMessage {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(items) = output.as_array() {
        for item in items {
            match item["type"].as_str().unwrap_or("") {
                "message" => {
                    if let Some(parts) = item["content"].as_array() {
                        for p in parts {
                            if p["type"] == "output_text" {
                                content.push_str(p["text"].as_str().unwrap_or(""));
                            }
                        }
                    }
                }
                "reasoning" => {
                    if let Some(parts) = item["summary"].as_array() {
                        for p in parts {
                            if let Some(t) = p["text"].as_str() {
                                if !reasoning.is_empty() {
                                    reasoning.push('\n');
                                }
                                reasoning.push_str(t);
                            }
                        }
                    }
                }
                "function_call" => {
                    tool_calls.push(json!({
                        "id": item["call_id"].as_str().unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": item["name"].as_str().unwrap_or(""),
                            "arguments": item["arguments"].as_str().unwrap_or(""),
                        }
                    }));
                }
                _ => {}
            }
        }
    }
    let mut m = ChatMessage::assistant(
        &content,
        if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
    );
    if !tool_calls.is_empty() {
        m.tool_calls = Some(json!(tool_calls));
    }
    m
}

/// Responses usage → the Completion's four Option fields (None = the
/// provider said nothing; never invent a zero).
fn responses_usage(u: &Value) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64();
    // Creation accounting lives under usage.plugins when the session
    // cache reports it (shape: cache_creation_input_tokens, ephemeral
    // 5-minute tier). Absent on every probe so far — leave the read in
    // place for when it shows up.
    let write = u["plugins"]["input_tokens_details"]["cache_creation_input_tokens"].as_u64();
    (
        u["input_tokens"].as_u64(),
        u["output_tokens"].as_u64(),
        cached,
        write,
    )
}

/// Reasoning tokens out of a Responses-shaped `usage`: Anthropic splits
/// them (`usage.output_tokens_details.reasoning_tokens`), separate from
/// `output_tokens_details.output_tokens`. `None` = the provider said
/// nothing, the same "unknown ≠ 0" contract as the chat-completions side.
fn responses_reasoning_tokens(u: &Value) -> Option<u64> {
    u["output_tokens_details"]["reasoning_tokens"].as_u64()
}

/// The five server-side tool item types this endpoint emits
/// (2026-09-03 probes produced all five). mcp_call and file_search_call
/// are not here: veles declares neither, and a surprise item type is
/// better ignored than mislabelled.
fn is_builtin_call_type(t: &str) -> bool {
    matches!(
        t,
        "web_search_call"
            | "web_extractor_call"
            | "code_interpreter_call"
            | "web_search_image_call"
            | "image_search_call"
    )
}

/// Surface name for a built-in call: the tool that ran it, not the
/// item type — `web_search`, not `web_search_call`. The two
/// search-image tools carry their own `name` field; trust it when
/// present.
fn builtin_display_name(item_type: &str, item: &Value) -> String {
    if let Some(n) = item["name"].as_str().filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    item_type.trim_end_matches("_call").to_string()
}

/// One human-readable line per built-in invocation — this becomes the
/// ToolEnd result every surface already knows how to render. Truncated
/// hard: a code-interpreter log or an extractor dump can be kilobytes,
/// and the transcript does not need the whole of it twice (the model
/// already worked from the full item server-side).
fn render_builtin_result(item_type: &str, item: &Value) -> String {
    fn clip(s: &str, n: usize) -> String {
        if s.len() <= n {
            return s.to_string();
        }
        // Nearest char boundary at or before n (floor_char_boundary is
        // still unstable).
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
    match item_type {
        "web_search_call" => {
            let action = &item["action"];
            let queries = action["queries"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|q| q.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| action["query"].as_str().unwrap_or("?").to_string());
            let sources = action["sources"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s["url"].as_str())
                        .take(5)
                        .collect::<Vec<_>>()
                        .join("\n  ")
                })
                .unwrap_or_default();
            let n = action["sources"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("searched: {queries} ({n} sources)\n  {sources}")
        }
        "web_extractor_call" => {
            let goal = item["goal"].as_str().unwrap_or("");
            let urls = item["urls"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|u| u.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let out = clip(item["output"].as_str().unwrap_or(""), 400);
            format!("extracted from {urls}: {goal}\n{out}")
        }
        "code_interpreter_call" => {
            let code = clip(item["code"].as_str().unwrap_or(""), 200);
            let logs = item["outputs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o["logs"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            format!("ran code:\n{code}\n{}", clip(&logs, 300))
        }
        // web_search_image_call / image_search_call carry arguments +
        // output as JSON strings; surface them clipped, uninterpreted.
        _ => {
            let name = builtin_display_name(item_type, item);
            let out = clip(item["output"].as_str().unwrap_or(""), 300);
            format!("{name}: {out}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::tests::sse_server;
    use crate::providers::CacheMode;

    fn req(stream: bool) -> ChatRequest {
        ChatRequest {
            model: "qwen3.8-max".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream,
            replay_reasoning: false,
            cache_mode: CacheMode::Auto,
            extra_body: None,
        }
    }

    fn client() -> ResponsesClient {
        ResponsesClient::new(
            "http://127.0.0.1:0/v1",
            "k",
            UserAgentProfile::veles(),
            vec!["web_search".into()],
        )
        .expect("client")
    }

    // ── request body ─────────────────────────────────────────────────

    /// Plain roles map onto chat-style items; the responses wire has no
    /// reason to see anything fancier for a text-only history.
    #[test]
    fn plain_history_maps_to_chat_style_items() {
        let msgs = vec![
            ChatMessage {
                role: "system".into(),
                content: "be brief".into(),
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: None,
                attachments: vec![],
                injection: None,
                msg_id: None,
            },
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi there", None),
        ];
        let input = history_to_input(&msgs);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0], json!({"role":"system","content":"be brief"}));
        assert_eq!(input[1], json!({"role":"user","content":"hello"}));
        assert_eq!(input[2], json!({"role":"assistant","content":"hi there"}));
    }

    /// The round-trip the whole provider exists for: veles' OpenAI-
    /// shaped tool_calls and tool results become typed items, with each
    /// output DIRECTLY AFTER the call it answers — the adjacency the
    /// docs demand (and the live endpoint happened not to enforce).
    #[test]
    fn tool_round_becomes_adjacent_function_call_items() {
        let mut assistant = ChatMessage::assistant("", None);
        assistant.tool_calls = Some(json!([
            {"id": "call_1", "type": "function",
             "function": {"name": "add", "arguments": "{\"a\":1,\"b\":2}"}},
            {"id": "call_2", "type": "function",
             "function": {"name": "mul", "arguments": "{\"a\":3,\"b\":4}"}}
        ]));
        let msgs = vec![
            ChatMessage::user("math please"),
            assistant,
            ChatMessage::tool_result("call_1", "add", "3".into()),
            ChatMessage::tool_result("call_2", "mul", "12".into()),
        ];
        let input = history_to_input(&msgs);
        assert_eq!(input.len(), 5, "user, 2 calls, 2 outputs: {input:?}");
        assert_eq!(
            input[1],
            json!({"type":"function_call","name":"add",
                   "arguments":"{\"a\":1,\"b\":2}","call_id":"call_1"})
        );
        assert_eq!(
            input[2],
            json!({"type":"function_call_output","call_id":"call_1","output":"3"})
        );
        assert_eq!(input[3]["call_id"], json!("call_2"));
        assert_eq!(input[4]["call_id"], json!("call_2"));
    }

    /// An assistant message that has BOTH text and calls keeps the
    /// text as its own message item — dropping it would rewrite the
    /// transcript the model is continuing.
    #[test]
    fn assistant_text_survives_next_to_its_calls() {
        let mut assistant = ChatMessage::assistant("checking the file", None);
        assistant.tool_calls = Some(json!([
            {"id": "c1", "type": "function",
             "function": {"name": "read_file", "arguments": "{}"}}
        ]));
        let input = history_to_input(&[assistant]);
        assert_eq!(input.len(), 2);
        assert_eq!(
            input[0],
            json!({"role":"assistant","content":"checking the file"})
        );
        assert_eq!(input[1]["type"], json!("function_call"));
    }

    /// User attachments become input parts, same content as the
    /// chat-completions mapping but in this wire's part types.
    #[test]
    fn attachments_become_input_image_parts() {
        let mut user = ChatMessage::user("what is this");
        user.attachments.push(crate::media::Attachment {
            source: "x.png".into(),
            mime: "image/png".into(),
            data_url: "data:image/png;base64,AAA".into(),
        });
        let input = history_to_input(&[user]);
        assert_eq!(
            input[0],
            json!({"role":"user","content":[
                {"type":"input_text","text":"what is this"},
                {"type":"input_image","image_url":"data:image/png;base64,AAA"}
            ]})
        );
    }

    /// Function schemas flatten from the nested chat-completions shape
    /// to the flat one this endpoint wants; built-ins follow, as bare
    /// type declarations.
    #[test]
    fn tools_flatten_and_built_ins_follow() {
        let schemas = vec![json!({
            "type": "function",
            "function": {
                "name": "terminal",
                "description": "run a command",
                "parameters": {"type":"object"}
            }
        })];
        let tools = tools_for_responses(&schemas, &["web_search".into(), "mcp_never".into()]);
        assert_eq!(tools.len(), 3);
        assert_eq!(
            tools[0],
            json!({"type":"function","name":"terminal",
                   "description":"run a command","parameters":{"type":"object"}})
        );
        assert_eq!(tools[1], json!({"type":"web_search"}));
        assert_eq!(tools[2], json!({"type":"mcp_never"}));
        assert!(tools_for_responses(&[], &[]).is_empty());
    }

    /// The body the route assembles: flat tools + built-ins, reasoning
    /// effort nested, this wire's extra_body denylist.
    #[test]
    fn the_responses_body_shape() {
        let c = client();
        let mut r = req(false);
        r.tools = vec![json!({
            "type":"function",
            "function":{"name":"f","description":"d","parameters":{"type":"object"}}
        })];
        r.reasoning_effort = Some("high".into());
        r.extra_body = Some(json!({
            "input": ["hijack"], "model": "hijack", "stream": true,
            "tools": [{"type":"hijack"}], "temperature": 0.2
        }));
        let body = c.build_body(&r);
        assert_eq!(body["model"], json!("qwen3.8-max"));
        assert_eq!(body["stream"], json!(false), "denylist kept stream");
        assert_eq!(body["input"], json!([{"role":"user","content":"hi"}]));
        assert_eq!(body["reasoning"], json!({"effort":"high"}));
        assert_eq!(body["temperature"], json!(0.2), "non-denylisted rides");
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2, "one flattened function + one built-in");
        assert_eq!(tools[0]["name"], json!("f"));
        assert_eq!(tools[1], json!({"type":"web_search"}));
    }

    /// Cache posture decides the session-cache header — the only cache
    /// this endpoint has (probed: no implicit caching exists).
    #[tokio::test]
    async fn cache_off_sends_no_session_cache_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let cap = captured.clone();
        crate::rt::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                // One read is enough: the body below is tiny.
                let n = sock.read(&mut buf).await.unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                if let Ok(mut c) = cap.lock() {
                    *c = String::from_utf8_lossy(&raw).to_string();
                }
                let body = r#"{"output":[],"usage":{"input_tokens":1,"output_tokens":1}}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        let c = ResponsesClient::new(
            &format!("http://{addr}/v1"),
            "k",
            UserAgentProfile::veles(),
            vec![],
        )
        .expect("client");
        let mut r = req(false);
        r.cache_mode = CacheMode::Off;
        let _ = c.chat(&r, "mock").await.expect("call");
        let seen = captured.lock().unwrap().clone();
        assert!(
            !seen.to_lowercase().contains("x-dashscope-session-cache"),
            "Off must send nothing cache-related: {seen}"
        );
        // The Auto/Explicit header IS sent — asserted end-to-end by the
        // agent-level responses turn test, which captures the wire.
    }

    // ── response parsing ─────────────────────────────────────────────

    /// The three item types a turn cares about, assembled the way
    /// run_turn reads them: content string, reasoning text, tool_calls
    /// with id/function nesting.
    #[test]
    fn parse_output_assembles_message_reasoning_and_calls() {
        let output = json!([
            {"type":"reasoning","id":"r1",
             "summary":[{"type":"summary_text","text":"thinking hard"}]},
            {"type":"function_call","id":"m1","call_id":"call_9",
             "name":"add","arguments":"{\"a\":1}","status":"completed"},
            {"type":"message","id":"m2","role":"assistant","status":"completed",
             "content":[{"type":"output_text","text":"done","annotations":[]}]},
            {"type":"web_search_call","id":"m3","status":"completed",
             "action":{"type":"search","query":"q","sources":[]}}
        ]);
        let m = parse_output(&output);
        assert_eq!(m.content, "done");
        assert_eq!(m.reasoning.as_deref(), Some("thinking hard"));
        let calls = m.tool_calls.expect("calls").as_array().cloned().unwrap();
        assert_eq!(calls.len(), 1, "built-in calls are not engine tool_calls");
        assert_eq!(calls[0]["id"], json!("call_9"));
        assert_eq!(calls[0]["function"]["name"], json!("add"));
    }

    #[test]
    fn usage_reads_the_responses_shape() {
        let u = json!({
            "input_tokens": 1837, "output_tokens": 12,
            "input_tokens_details": {"cached_tokens": 1831},
            "plugins": {"input_tokens_details": {"cache_creation_input_tokens": 77}}
        });
        assert_eq!(
            responses_usage(&u),
            (Some(1837), Some(12), Some(1831), Some(77))
        );
        // Nothing said → nothing invented.
        assert_eq!(responses_usage(&json!({})), (None, None, None, None));
    }

    /// Non-streaming call against a canned JSON server: the same
    /// Completion contract the chat-completions client gives.
    #[tokio::test]
    async fn chat_parses_a_nonstreaming_response() {
        async fn json_server(body: &'static str) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            crate::rt::spawn(async move {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                }
            });
            format!("http://{addr}/v1")
        }

        let base = json_server(
            r#"{"id":"resp_x","status":"completed","output":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"ok"}]},
                {"type":"function_call","call_id":"call_7","name":"add",
                 "arguments":"{\"a\":2,\"b\":3}"},
                {"type":"message","role":"assistant","status":"completed",
                 "content":[{"type":"output_text","text":"pong"}]}],
               "usage":{"input_tokens":100,"output_tokens":5,
                        "input_tokens_details":{"cached_tokens":42}}}"#,
        )
        .await;
        let c =
            ResponsesClient::new(&base, "k", UserAgentProfile::veles(), vec![]).expect("client");
        let comp = c.chat(&req(false), "mock").await.expect("call");
        assert_eq!(comp.message.content, "pong");
        assert_eq!(comp.message.reasoning.as_deref(), Some("ok"));
        let calls = comp.message.tool_calls.expect("calls");
        assert_eq!(calls[0]["id"], json!("call_7"));
        assert_eq!(comp.prompt_tokens, Some(100));
        assert_eq!(comp.cached_tokens, Some(42));
        assert_eq!(comp.provider, "mock");
    }

    /// The full streaming grammar, as the live endpoint sent it:
    /// reasoning deltas, text deltas, function-call argument deltas,
    /// a built-in call surfacing as ToolStart/ToolEnd, usage in
    /// response.completed — and exactly one Done.
    #[tokio::test]
    async fn chat_stream_maps_the_event_grammar() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"sequence_number\":0,",
            "\"response\":{\"id\":\"resp_1\",\"status\":\"queued\"}}\n\n",
            "data: {\"type\":\"response.in_progress\",\"sequence_number\":1,",
            "\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":2,",
            "\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"r1\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":3,",
            "\"item_id\":\"r1\",\"output_index\":0,\"delta\":\"thinking \"}\n\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":4,",
            "\"item_id\":\"r1\",\"output_index\":0,\"delta\":\"more\"}\n\n",
            "data: {\"type\":\"response.reasoning_text.done\",\"sequence_number\":5,",
            "\"item_id\":\"r1\",\"output_index\":0,\"text\":\"thinking more\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":6,",
            "\"output_index\":1,\"item\":{\"type\":\"web_search_call\",\"id\":\"w1\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.web_search_call.searching\",\"sequence_number\":7,",
            "\"item_id\":\"w1\",\"output_index\":1}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"sequence_number\":8,",
            "\"output_index\":1,\"item\":{\"type\":\"web_search_call\",\"id\":\"w1\",\"status\":\"completed\",",
            "\"action\":{\"type\":\"search\",\"query\":\"rust\",\"queries\":[\"rust\"],",
            "\"sources\":[{\"type\":\"url\",\"url\":\"https://rust-lang.org\"}]}}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":9,",
            "\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"f1\",",
            "\"call_id\":\"call_3\",\"name\":\"add\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":10,",
            "\"item_id\":\"f1\",\"output_index\":2,\"delta\":\"{\\\"a\\\": \"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":11,",
            "\"item_id\":\"f1\",\"output_index\":2,\"delta\":\"1}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"sequence_number\":12,",
            "\"item_id\":\"f1\",\"output_index\":2,\"name\":\"add\",\"arguments\":\"{\\\"a\\\": 1}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"sequence_number\":13,",
            "\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"f1\",",
            "\"call_id\":\"call_3\",\"name\":\"add\",\"arguments\":\"{\\\"a\\\": 1}\",\"status\":\"completed\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":14,",
            "\"output_index\":3,\"item\":{\"type\":\"message\",\"id\":\"m1\",\"content\":[],",
            "\"role\":\"assistant\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":15,",
            "\"item_id\":\"m1\",\"output_index\":3,\"content_index\":0,\"delta\":\"the \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":16,",
            "\"item_id\":\"m1\",\"output_index\":3,\"content_index\":0,\"delta\":\"answer\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"sequence_number\":17,",
            "\"item_id\":\"m1\",\"output_index\":3,\"content_index\":0,\"text\":\"the answer\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"sequence_number\":18,",
            "\"output_index\":3,\"item\":{\"type\":\"message\",\"id\":\"m1\",\"role\":\"assistant\",",
            "\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"the answer\",\"annotations\":[]}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"sequence_number\":19,",
            "\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[",
            "{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thinking more\"}]},",
            "{\"type\":\"function_call\",\"call_id\":\"call_3\",\"name\":\"add\",\"arguments\":\"{\\\"a\\\": 1}\"},",
            "{\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"the answer\",\"annotations\":[]}]}],",
            "\"usage\":{\"input_tokens\":50,\"output_tokens\":9,",
            "\"input_tokens_details\":{\"cached_tokens\":44}}}}\n\n"
        );
        let base = sse_server(sse).await;
        let c = ResponsesClient::new(
            &base,
            "k",
            UserAgentProfile::veles(),
            vec!["web_search".into()],
        )
        .expect("client");

        let mut events: Vec<StreamEvent> = Vec::new();
        let comp = c
            .chat_stream(&req(true), "mock", None, |e| events.push(e))
            .await
            .expect("stream");

        // Route first, Done exactly once at the end.
        assert!(matches!(events.first(), Some(StreamEvent::Route { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::Done(_)))
                .count(),
            1,
            "one Done per call"
        );
        let deltas: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, "the answer");
        let reasoning: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "thinking more");
        // The built-in call surfaced through the events every surface
        // already renders.
        let tool_events: Vec<&StreamEvent> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolStart(..) | StreamEvent::ToolEnd(..)))
            .collect();
        assert_eq!(tool_events.len(), 2);
        match tool_events[1] {
            StreamEvent::ToolEnd(name, result) => {
                assert_eq!(name, "web_search");
                assert!(result.contains("rust-lang.org"), "{result}");
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
        // Final message from response.completed's canonical output.
        assert_eq!(comp.message.content, "the answer");
        assert_eq!(comp.message.reasoning.as_deref(), Some("thinking more"));
        let calls = comp.message.tool_calls.expect("tool_calls");
        assert_eq!(calls[0]["id"], json!("call_3"));
        assert_eq!(calls[0]["function"]["name"], json!("add"));
        assert_eq!(calls[0]["function"]["arguments"], json!("{\"a\": 1}"));
        assert_eq!(comp.prompt_tokens, Some(50));
        assert_eq!(comp.cached_tokens, Some(44));
    }

    /// THE 2026-09-08 CLIP, responses wire: a final delta hung at EOF
    /// without its blank line must survive — eventsource-stream dropped
    /// it, which is how live transcripts lost their tails mid-word.
    #[tokio::test]
    async fn a_responses_stream_keeps_the_final_delta_at_eof() {
        let sse = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,",
            "\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"head\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,",
            "\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"tail\"}\n"
        );
        let base = sse_server(sse).await;
        let c =
            ResponsesClient::new(&base, "k", UserAgentProfile::veles(), vec![]).expect("client");
        let mut deltas: String = String::new();
        let comp = c
            .chat_stream(&req(true), "mock", None, |e| {
                if let StreamEvent::Delta(t) = e {
                    deltas.push_str(&t);
                }
            })
            .await
            .expect("stream");
        assert_eq!(deltas, "headtail");
        assert_eq!(
            comp.message.content, "headtail",
            "the hung final delta must survive EOF"
        );
    }

    /// A responses stream CUT mid-thought — no `response.completed` (or
    /// incomplete/failed), only reasoning — is an error the caller retries,
    /// the same rule as the chat-completions reader (c1aaac's aborted
    /// turns were the empty-answer reading of exactly this).
    #[tokio::test]
    async fn a_responses_stream_cut_with_only_reasoning_is_truncated() {
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":1,",
            "\"item_id\":\"r1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hmm\"}\n\n"
        );
        let base = sse_server(sse).await;
        let c =
            ResponsesClient::new(&base, "k", UserAgentProfile::veles(), vec![]).expect("client");
        let err = c
            .chat_stream(&req(true), "mock", None, |_| {})
            .await
            .expect_err("a cut stream is an error");
        assert!(matches!(err, ProviderError::Truncated(_)), "{err:?}");
    }

    /// Cancel mid-stream: an ACCUMULATED tool call is dropped, text
    /// kept, Cancelled instead of Done — same posture as the
    /// chat-completions client. The cancel lands on the event after
    /// the call completed, so the drop is of a call the stream had
    /// already assembled, which is exactly the race Ctrl-C produces.
    #[tokio::test]
    async fn a_cancelled_responses_stream_drops_accumulated_calls() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,",
            "\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"f1\",",
            "\"call_id\":\"call_x\",\"name\":\"terminal\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":2,",
            "\"item_id\":\"f1\",\"output_index\":0,\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"sequence_number\":3,",
            "\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"f1\",",
            "\"call_id\":\"call_x\",\"name\":\"terminal\",",
            "\"arguments\":\"{\\\"command\\\": \\\"ls\\\"}\",\"status\":\"completed\"}}\n\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":4,",
            "\"item_id\":\"r1\",\"output_index\":1,\"delta\":\"next\"}\n\n"
        );
        let base = sse_server(sse).await;
        let c =
            ResponsesClient::new(&base, "k", UserAgentProfile::veles(), vec![]).expect("client");
        let tok = CancelToken::new();
        let t2 = tok.clone();
        let mut saw_cancelled = false;
        let comp = c
            .chat_stream(&req(true), "mock", Some(tok), |e| {
                if matches!(e, StreamEvent::ReasoningDelta(_)) {
                    // The call is already in the accumulation; cancel
                    // now and it must not survive to a Done.
                    t2.cancel();
                }
                if matches!(e, StreamEvent::Cancelled) {
                    saw_cancelled = true;
                }
                assert!(!matches!(e, StreamEvent::Done(_)), "no Done after cancel");
            })
            .await
            .expect("stream");
        assert!(saw_cancelled);
        assert!(
            comp.message.tool_calls.is_none(),
            "the accumulated call must be dropped on cancel"
        );
    }

    /// A 429 on the responses wire classifies exactly as it does on
    /// chat_completions, because the failover chain reads the same
    /// enum off both.
    #[tokio::test]
    async fn a_429_is_rate_limited_on_both_wires() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        crate::rt::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"error":{"code":"THROTTLING","message":"quota"}}"#;
                let head = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let c = ResponsesClient::new(
            &format!("http://{addr}/v1"),
            "k",
            UserAgentProfile::veles(),
            vec![],
        )
        .expect("client");
        let err = c.chat(&req(false), "mock").await.expect_err("429");
        assert!(err.should_fallback(), "a 429 must walk the chain");
        assert!(matches!(err, ProviderError::RateLimited(_)));
    }
}
