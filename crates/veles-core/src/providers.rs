//! LLM provider layer.
//!
//! Parity targets (hermes `providers.py` / `runtime_provider.py` /
//! `conversation_loop._should_fallback`):
//!   - api_mode: chat_completions and responses — the two modes this
//!     build speaks. `responses` (the OpenAI Responses API, client in
//!     `responses.rs`) landed 2026-09-03 because the Token Plan's
//!     server-side tools live on that wire and nowhere else. hermes
//!     also names anthropic_messages; it is not implemented here, and
//!     `config::load` rejects unimplemented modes by name rather than
//!     silently treating them as chat_completions (which is how
//!     `api_mode = ""` ended up sprinkled through a live config nobody
//!     could have debugged from its behavior).
//!   - custom_providers with base_url + key_env + per-model context_length
//!   - **fallback_providers chain** — a 429/5xx/network failure walks
//!     down the chain. The chain itself is stateless; `agent.rs` layers
//!     within-turn stickiness on top (once a round lands on route N>0,
//!     the rest of that turn starts there), resetting at the turn
//!     boundary — the laptop behavior: personal quota exhausted →
//!     zai → team, then reverts when the 5-hour window resets.
//!   - reasoning fields passthrough (reasoning_content for replay)

use anyhow::Result;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

// ── User-Agent impersonation ("approved harness" profiles) ───────────
//
// Some providers gate features behind "approved" agent harnesses. veles
// can present an approved wire profile instead of its honest UA. The
// hermes profile is byte-faithful to hermes 0.20.0's runtime stack
// (openai-python 2.24.0 on CPython 3.12.13/x86_64 — verified against
// the pinned nix store env on this laptop 2026-08-10); the claw profile
// matches OpenClaw's documented gateway UA.

/// Resolved outbound identity for inference API calls.
#[derive(Debug, Clone)]
pub struct UserAgentProfile {
    pub user_agent: String,
    /// Extra identifying headers (X-Stainless-* for SDK-faithful
    /// profiles).
    pub extra_headers: Vec<(String, String)>,
}

impl UserAgentProfile {
    /// The honest default.
    pub fn veles() -> Self {
        Self {
            user_agent: format!("veles-agent/{}", env!("CARGO_PKG_VERSION")),
            extra_headers: Vec::new(),
        }
    }

    /// Hermes 0.20.0 wire profile — exactly what hermes' openai-python
    /// 2.24.0 client sends (verified 2026-08-10 against the laptop's
    /// pinned runtime: Python 3.12.13, x86_64, CPython, async).
    pub fn hermes() -> Self {
        Self {
            user_agent: "OpenAI/Python 2.24.0".into(),
            extra_headers: vec![
                ("X-Stainless-Lang".into(), "python".into()),
                ("X-Stainless-Package-Version".into(), "2.24.0".into()),
                ("X-Stainless-OS".into(), "Linux".into()),
                ("X-Stainless-Arch".into(), "x64".into()),
                ("X-Stainless-Runtime".into(), "CPython".into()),
                ("X-Stainless-Runtime-Version".into(), "3.12.13".into()),
                ("X-Stainless-Async".into(), "true".into()),
            ],
        }
    }

    /// OpenClaw gateway profile (documented wire identity).
    pub fn claw() -> Self {
        Self {
            user_agent: "OpenClaw-Gateway/1.0".into(),
            extra_headers: Vec::new(),
        }
    }
}

/// Resolve the User-Agent policy: VELES_USER_AGENT env > config
/// agent.user_agent > honest default. Presets "hermes"/"claw"; anything
/// else is a literal custom UA string.
pub fn resolve_user_agent(config_value: Option<&str>, home: &std::path::Path) -> UserAgentProfile {
    let raw = resolve_env_or_dotenv("VELES_USER_AGENT", home)
        .or_else(|| config_value.map(str::to_string));
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => UserAgentProfile::veles(),
        Some("hermes") => UserAgentProfile::hermes(),
        Some("claw") | Some("openclaw") => UserAgentProfile::claw(),
        Some(custom) => UserAgentProfile {
            user_agent: custom.to_string(),
            extra_headers: Vec::new(),
        },
    }
}

/// Cooperative cancellation — the REPL drops this flag on Ctrl-C while a
/// turn is streaming; the stream loop checks it per SSE event and aborts.
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    /// Same underlying flag? Used to drop one registration out of many
    /// without touching the others (the turn-cancel registry).
    pub fn ptr_eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// reasoning_content / reasoning_details for replay (DeepSeek/Kimi/Qwen).
    /// NEVER rewritten by display plugins — API replay integrity.
    pub reasoning: Option<String>,
    /// Display-side reasoning copy. Set by post_llm_call hooks (e.g. the
    /// reasoning-summary plugin) when the raw chain-of-thought should not
    /// be shown. Surfaces render this when present. Mirrors hermes'
    /// msg["reasoning"] (display) vs reasoning_content (replay) split.
    pub display_reasoning: Option<String>,
    /// Raw tool_calls JSON on an assistant message (for replay fidelity).
    pub tool_calls: Option<Value>,
    /// tool_call_id + name on role="tool" results.
    pub tool_meta: Option<ToolMeta>,
    /// Files the USER attached, resolved to data URLs just before the
    /// request goes out (see media::attach_user_media). Empty for every
    /// other role — a system or tool message with an image part is a
    /// malformed request, not a feature.
    pub attachments: Vec<crate::media::Attachment>,
    /// The CLASS of engine injection this user-role message is
    /// ("steer" | "background" | "compaction" | "todo" | "hook" |
    /// "verification"), when it is one. The model cannot tell these
    /// from the human's own words; the transcript CAN and must — every
    /// injection renders as a labelled block, never as speech.
    pub injection: Option<String>,
    /// The persisted row id (`messages.msg_id`), when this message came
    /// from the store or has been written to it. `None` for a message
    /// that has never been persisted.
    ///
    /// Compaction's tail pruning REWRITES persisted rows (shrinking an
    /// old tool result to a stub) and addresses them by THIS id, never
    /// by position: a positional index would silently corrupt the wrong
    /// row the moment in-memory history and the live rows diverge. The
    /// id is what makes "shrink this message" mean the message it read.
    pub msg_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub tool_call_id: String,
    pub name: String,
}

impl ChatMessage {
    /// Tag this message as an engine injection of `class` (RM3-S4/A4).
    pub fn with_injection(mut self, class: &str) -> Self {
        self.injection = Some(class.to_string());
        self
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: None,
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        }
    }
    pub fn assistant(content: &str, reasoning: Option<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            reasoning,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: None,
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        }
    }
    pub fn tool_result(tool_call_id: &str, name: &str, content: String) -> Self {
        Self {
            role: "tool".into(),
            content,
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: Some(ToolMeta {
                tool_call_id: tool_call_id.into(),
                name: name.into(),
            }),
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        }
    }
}

/// How this route asks the provider to cache the prefix.
///
/// Every provider configured on this machine today caches automatically
/// and needs nothing on the wire — which is exactly why `Auto` (emit
/// nothing) is the default and the strict endpoints stay safe. `Explicit`
/// exists because OpenRouter→Anthropic/Gemini is one config line away,
/// and those want the breakpoints stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Send no cache fields; the provider caches on its own.
    #[default]
    Auto,
    /// Emit `cache_control: {"type": "ephemeral"}` breakpoints.
    Explicit,
    /// Never emit, even if something else asks — for an endpoint that
    /// 400s on unknown fields.
    Off,
}

impl CacheMode {
    /// Unknown spellings fall back to `Auto`, the do-nothing mode: a
    /// typo in config.toml must not silently start decorating the wire.
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "explicit" => Self::Explicit,
            "off" => Self::Off,
            _ => Self::Auto,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Explicit => "explicit",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<Value>,
    pub reasoning_effort: Option<String>,
    pub stream: bool,
    /// Whether assistant messages carry their `reasoning_content` back
    /// on the wire (Step 3.3). Resolved per ROUTE from
    /// `[[custom_providers]] replay_reasoning`, default false: DeepSeek's
    /// reasoner rejects it on input and everyone else bills for it, and
    /// it is replayed cost nobody chose. The reasoning stays in the
    /// store either way — this is only what goes out.
    pub replay_reasoning: bool,
    /// Prompt-cache posture for the route this request goes out on
    /// (Step 1.5). Resolved from `[[custom_providers]] cache_mode`,
    /// falling back to `[cache] mode`.
    pub cache_mode: CacheMode,
    /// Arbitrary fields merged verbatim into the wire body (provider
    /// server-side options/tools). Provider-route values merge OVER the
    /// request-level one (which a session override sets).
    pub extra_body: Option<Value>,
}

/// A single streaming event, surfaced to the UI layer.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Incremental content text.
    Delta(String),
    /// Incremental reasoning text.
    ReasoningDelta(String),
    /// Which route is producing this stream.
    ///
    /// Carries the MODEL as well as the provider. It used to be the
    /// provider name alone, so every surface could only ever say
    /// "thinking… (openrouter)" — USER: "during the turn the status bar
    /// only seems to show provider, should be full provider->model
    /// chain". The model is what actually varies between routes.
    /// Also carries the effective REASONING EFFORT so the status line
    /// can show it next to provider/model (USER: "show effort level in
    /// the status next to provider/model name").
    Route {
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    },
    /// Tool round starting: (name, args_json) — for progress display.
    ToolStart(String, String),
    /// Tool round finished: (name, result).
    ToolEnd(String, String),
    /// The turn is parked on a question for the human (approval /
    /// clarify). Surfaces that can render it do; the answer travels
    /// back through `crate::ask`, not through this stream.
    Ask(Box<crate::ask::AskRequest>),
    /// Stream complete — final assembled message.
    Done(ChatMessage),
    /// Turn was interrupted by the user (Ctrl-C / /stop parity).
    Cancelled,
    /// The engine did something the user did not ask for — a guard
    /// fired, history was compacted, a different route answered.
    ///
    /// Deliberately a StreamEvent rather than a log line: every surface
    /// matches this enum exhaustively, so the compiler forces each one
    /// to handle it. See `crate::notes`.
    Note(crate::notes::EngineNote),
}

/// The fully-assembled completion (after streaming or non-streaming call).
#[derive(Debug, Clone)]
pub struct Completion {
    pub message: ChatMessage,
    pub model: String,
    pub provider: String,
    /// Token usage when the provider reports it (OpenAI-compatible
    /// `usage` object; our token-plan routes include it).
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Prompt tokens served from the provider's prefix cache.
    /// None = the provider said nothing (not "zero") — surfaces must
    /// render "unknown", not "0% cached".
    pub cached_tokens: Option<u64>,
    /// Tokens written INTO the cache this call, where the provider
    /// distinguishes it (Anthropic-shaped: cache_creation_input_tokens).
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens the provider billed this call, where it splits
    /// them out (`usage.completion_tokens_details.reasoning_tokens`).
    /// None = the provider said nothing, which is a DIFFERENT fact from
    /// "reported reasoning, and it was zero" — that is the same
    /// "unknown ≠ 0" doctrine `cached_tokens` carries. A route that
    /// folds thinking into `completion_tokens` leaves this unset, and
    /// the surplus then shows up as answer output, which is exactly the
    /// lie the separate count exists to expose.
    pub reasoning_tokens: Option<u64>,
}

/// Cache accounting, normalized across the shapes providers use.
/// Order matters: the first present field wins.
///   OpenAI / z.ai / OpenRouter: usage.prompt_tokens_details.cached_tokens
///   DeepSeek (incl. Aliyun compatible-mode): usage.prompt_cache_hit_tokens
///   Anthropic-shaped: usage.cache_read_input_tokens
///
/// Returns (cached, cache_write). None means the provider SAID NOTHING —
/// that is "unknown", not "0", and the distinction is what lets a surface
/// report a hit rate honestly.
fn cache_usage(u: &Value) -> (Option<u64>, Option<u64>) {
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
        .or_else(|| u["cache_read_input_tokens"].as_u64());
    let write = u["cache_creation_input_tokens"].as_u64();
    (cached, write)
}

/// Reasoning tokens, where the provider distinguishes them, out of an
/// OpenAI-compatible `usage` object.
///
/// OpenRouter/z.ai/DeepSeek put them at
/// `usage.completion_tokens_details.reasoning_tokens`; Anthropic-shaped
/// routes do not carry the field, and their thinking is tool-call
/// content rather than a separate bucket anyway. `None` means "the
/// provider said nothing about reasoning", which is a different fact
/// from "it said zero" — exactly `cache_usage`'s contract.
fn reasoning_usage(u: &Value) -> Option<u64> {
    u["completion_tokens_details"]["reasoning_tokens"].as_u64()
}

/// Errors classified for fallback decisions.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("rate limited (429): {0}")]
    RateLimited(String),
    #[error("server error ({status}): {body}")]
    Server { status: u16, body: String },
    #[error("network: {0}")]
    Network(String),
    #[error("client error ({status}): {body}")]
    Client { status: u16, body: String },
    /// The stream closed before the model finished — no `[DONE]`, no
    /// `finish_reason`, no usage — and nothing usable arrived (reasoning
    /// at most). Aliyun compatible-mode does this mid-thought: 18 turns
    /// from 2026-09-20 to 2026-10-06 ended as silent empty answers
    /// because the cut was read as a finished reply with no text.
    #[error("stream cut: {0}")]
    Truncated(String),
}

impl ProviderError {
    /// Should the caller fall back to the next route?
    /// Mirrors hermes' _should_fallback: 429 (any), 5xx, network, timeout.
    /// NOTE the known hermes quirk (memory, 2026-08-04): ANY rate-limit
    /// 429 triggers fallback even at 3% quota (transient
    /// Throttling.ResourceExhausted), not only true AllocationQuota
    /// exhaustion. veles keeps hermes behavior for parity.
    pub fn should_fallback(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited(_)
                | ProviderError::Server { .. }
                | ProviderError::Network(_)
                | ProviderError::Truncated(_)
        )
    }
}

/// Attach a cache breakpoint to one message, in the shape the
/// OpenAI-compatible pass-through wants: `cache_control` sits on a
/// CONTENT PART, never on the message object, so a plain string content
/// is promoted to a one-element parts array first. A message that
/// already has parts (a user turn with an image) gets the marker on its
/// last part, which is where the prefix actually ends.
fn mark_cache_breakpoint(msg: &mut Value) {
    let cc = json!({ "type": "ephemeral" });
    match &mut msg["content"] {
        Value::Array(parts) => {
            if let Some(last) = parts.last_mut() {
                last["cache_control"] = cc;
            }
        }
        other => {
            let text = other.as_str().unwrap_or_default().to_string();
            // An empty content part is rejected by several endpoints;
            // there is also nothing to cache, so leave it alone.
            if text.is_empty() {
                return;
            }
            *other = json!([{ "type": "text", "text": text, "cache_control": cc }]);
        }
    }
}

/// The wires a route can ride, behind one call surface.
///
/// There is deliberately no trait here, and adding a plugin driver did
/// not change that: an enum makes "which wire does this route speak" a
/// value the rest of the engine can branch on (proxy.rs refuses to
/// forward what it cannot translate) instead of a dynamic dispatch it
/// can never see. A plugin route is exactly the kind of thing something
/// downstream must be able to refuse, so it is a third VARIANT rather
/// than a reason to erase the distinction.
#[derive(Debug, Clone)]
pub enum RouteClient {
    ChatCompletions(ProviderClient),
    Responses(crate::responses::ResponsesClient),
    /// A wire supplied by a plugin (`api_mode = "plugin"`, `driver =
    /// "..."`). See the provider-driver section further down.
    Plugin(PluginRoute),
}

impl RouteClient {
    pub async fn chat(
        &self,
        req: &ChatRequest,
        provider_name: &str,
    ) -> Result<Completion, ProviderError> {
        match self {
            Self::ChatCompletions(c) => c.chat(req, provider_name).await,
            Self::Responses(c) => c.chat(req, provider_name).await,
            // A driver is SYNC (it is a plugin handler), so it runs on a
            // blocking thread — which is also what lets it call back
            // into the engine through the bridge.
            Self::Plugin(p) => {
                let (p, req, name) = (p.clone(), req.clone(), provider_name.to_string());
                crate::rt::spawn_blocking(move || p.run(&req, &name, |_| {}))
                    .await
                    .map_err(|e| {
                        ProviderError::Network(format!("provider driver task failed: {e}"))
                    })?
            }
        }
    }
    pub async fn chat_stream<F>(
        &self,
        req: &ChatRequest,
        provider_name: &str,
        cancel: Option<CancelToken>,
        on_event: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamEvent),
    {
        match self {
            Self::ChatCompletions(c) => c.chat_stream(req, provider_name, cancel, on_event).await,
            Self::Responses(c) => c.chat_stream(req, provider_name, cancel, on_event).await,
            // The driver runs on a blocking thread and its chunks come
            // back over a channel, which this side drains as they
            // arrive. `on_event` is `FnMut` and stays on THIS thread —
            // it belongs to the caller's surface, not to the plugin.
            Self::Plugin(p) => {
                let (route, r, name) = (p.clone(), req.clone(), provider_name.to_string());
                let mut on_event = on_event;
                on_event(StreamEvent::Route {
                    provider: name.clone(),
                    model: r.model.clone(),
                    reasoning_effort: r.reasoning_effort.clone(),
                });
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
                let mut task = crate::rt::spawn_blocking(move || {
                    route.run(&r, &name, move |chunk| {
                        let _ = tx.send(chunk.clone());
                    })
                });
                let mut on_event = on_event;
                loop {
                    tokio::select! {
                        chunk = rx.recv() => match chunk {
                            Some(c) => emit_wire_chunk(&c, &mut on_event),
                            // The driver finished and dropped the
                            // sender; the join below has the answer.
                            None => break,
                        },
                        joined = &mut task => {
                            let out = joined.map_err(|e| {
                                ProviderError::Network(format!(
                                    "provider driver task failed: {e}"
                                ))
                            })?;
                            // Drain anything the driver emitted just
                            // before returning, or a final delta is
                            // lost to the race.
                            while let Ok(c) = rx.try_recv() {
                                emit_wire_chunk(&c, &mut on_event);
                            }
                            let completion = out?;
                            on_event(StreamEvent::Done(completion.message.clone()));
                            return Ok(completion);
                        }
                    }
                }
                let out = (&mut task).await.map_err(|e| {
                    ProviderError::Network(format!("provider driver task failed: {e}"))
                })?;
                let completion = out?;
                on_event(StreamEvent::Done(completion.message.clone()));
                Ok(completion)
            }
        }
    }
    pub fn base_url(&self) -> &str {
        match self {
            Self::ChatCompletions(c) => &c.base_url,
            Self::Responses(c) => &c.base_url,
            Self::Plugin(p) => &p.base_url,
        }
    }
    pub fn api_key(&self) -> &str {
        match self {
            Self::ChatCompletions(c) => &c.api_key,
            Self::Responses(c) => &c.api_key,
            Self::Plugin(p) => &p.api_key,
        }
    }
    /// Same pool-sharing identity test hook both clients carry.
    /// A plugin route has no connection pool of its own — the driver
    /// owns its transport — so it reports 0.
    #[cfg(test)]
    pub fn client_id(&self) -> usize {
        match self {
            Self::ChatCompletions(c) => c.client_id(),
            Self::Responses(c) => c.client_id(),
            Self::Plugin(_) => 0,
        }
    }
}

/// A resolved route: provider + model + credentials, ready to call.
#[derive(Debug, Clone)]
pub struct Route {
    pub provider: String, // display/lookup name, e.g. "ali-token-plan-personal"
    pub model: String,
    pub client: RouteClient,
    /// Declared context window in tokens, when the config knows it
    /// (per-model context_length in the provider's models map, or the
    /// provider-level context_length). Drives context compression's
    /// percent-of-window trigger (M10).
    pub context_tokens: Option<u64>,
    /// Per-provider reasoning-effort override (ProviderEntry.reasoning_effort).
    /// None = use the request-level/global effort.
    pub reasoning_effort: Option<String>,
    /// Per-provider extra_body (ProviderEntry.extra_body), merged over
    /// the request-level value at call time.
    pub extra_body: Option<Value>,
    /// Whether to replay `reasoning_content` to THIS provider
    /// (ProviderEntry.replay_reasoning). Resolved here rather than at
    /// request-build time because the route is only chosen inside the
    /// fallback loop — and the answer differs per endpoint.
    pub replay_reasoning: bool,
    /// Prompt-cache posture for this endpoint (ProviderEntry.cache_mode,
    /// else `[cache] mode`; forced to Off when `[cache] enabled = false`).
    pub cache_mode: CacheMode,
}

#[derive(Debug, Clone)]
pub struct ProviderClient {
    /// Arc-backed so clones share one connection pool / TLS state —
    /// `effective_routes` memoizes clients by (base_url, key, UA) and
    /// every Route clone must keep riding the SAME pool, or the memo
    /// buys nothing.
    http: Arc<reqwest::Client>,
    pub base_url: String,
    pub api_key: String,
    /// Wire identity (User-Agent + SDK-faithful extra headers).
    pub ua: UserAgentProfile,
}

impl ProviderClient {
    pub fn new(base_url: &str, api_key: &str, ua: UserAgentProfile) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .user_agent(&ua.user_agent);
        // SDK-faithful profiles carry extra identifying headers; bake
        // them in as defaults so every request (streaming and not)
        // presents the full wire profile.
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
        })
    }

    /// Test hook: identity of the underlying reqwest client. Two
    /// `ProviderClient`s with the same id share a connection pool and
    /// TLS state — which is what `effective_routes`' memoization must
    /// actually deliver.
    #[cfg(test)]
    pub fn client_id(&self) -> usize {
        Arc::as_ptr(&self.http) as usize
    }

    /// Test/diagnostic hook: POST a minimal chat-completions body and
    /// return the raw response body. Used by ua_wire_probe to verify the
    /// wire identity against a local echo server (zero inference tokens).
    pub async fn http_probe(&self) -> Result<String> {
        let body = json!({
            "model": "probe",
            "messages": [{ "role": "user", "content": "probe" }],
            "stream": false,
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        Ok(resp.text().await?)
    }

    /// The OpenAI chat-completions request body for one `ChatRequest`.
    ///
    /// Reachable at module level as [`wire_body`], because a PROVIDER
    /// DRIVER needs it: a plugin that speaks some other wire is handed
    /// this body and translates it, which means a driver author writes a
    /// translation and not a request builder. Attachment parts, cache
    /// breakpoints, tool serialization, reasoning_effort and the
    /// extra_body denylist all come for free and stay in one place.
    fn build_body(&self, req: &ChatRequest) -> Value {
        Self::build_body_for(req)
    }

    pub(crate) fn build_body_for(req: &ChatRequest) -> Value {
        // Sanitize: providers reject an assistant with empty content and
        // no tool_calls, and an empty tool result, as "the message at
        // position N with role 'assistant' must not be empty" (400).
        // A crash mid-persist or an interrupted tool round can leave a
        // stored row in that shape; repair it on the way out rather than
        // failing the whole request. Content is NEVER dropped — only
        // replaced with a placeholder when the row is provably invalid.
        let sanitized: Vec<ChatMessage> = req
            .messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                match m.role.as_str() {
                    "assistant" if m.content.trim().is_empty() && m.tool_calls.is_none() => {
                        tracing::warn!(
                            "build_body: assistant message with empty content and no \
                             tool_calls (msg_id={:?}) — substituting placeholder",
                            m.msg_id
                        );
                        m.content = "[interrupted tool round]".to_string();
                    }
                    "tool" if m.content.trim().is_empty() => {
                        tracing::warn!(
                            "build_body: empty tool result (msg_id={:?}, tool={:?}) — \
                             substituting placeholder",
                            m.msg_id,
                            m.tool_meta.as_ref().map(|t| t.name.as_str())
                        );
                        m.content = format!(
                            "[{} produced no output]",
                            m.tool_meta
                                .as_ref()
                                .map(|t| t.name.as_str())
                                .unwrap_or("tool")
                        );
                    }
                    _ => {}
                }
                m
            })
            .collect();

        let messages: Vec<Value> = sanitized
            .iter()
            .map(|m| {
                // A user message with attachments becomes an OpenAI
                // PARTS ARRAY. Everything else keeps a plain string:
                // several providers reject an array on a system or tool
                // message, and there is nothing to put in one.
                let mut v = if m.role == "user" && !m.attachments.is_empty() {
                    let mut parts: Vec<Value> = Vec::new();
                    if !m.content.is_empty() {
                        parts.push(json!({ "type": "text", "text": m.content }));
                    }
                    for a in &m.attachments {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": a.data_url },
                        }));
                    }
                    json!({ "role": m.role, "content": parts })
                } else {
                    json!({ "role": m.role, "content": m.content })
                };
                // Replayed reasoning is opt-in per provider: it is the
                // one part of the body we send back purely because we
                // happened to receive it. See ChatRequest.
                if req.replay_reasoning {
                    if let Some(r) = &m.reasoning {
                        v["reasoning_content"] = json!(r);
                    }
                }
                if let Some(tc) = &m.tool_calls {
                    v["tool_calls"] = tc.clone();
                }
                if let Some(meta) = &m.tool_meta {
                    v["tool_call_id"] = json!(meta.tool_call_id);
                    v["name"] = json!(meta.name);
                }
                v
            })
            .collect();

        let mut messages = messages;
        // Explicit cache breakpoints, in prefix order (Step 1.5). At
        // most three, because that is what the wire formats that want
        // them allow: the system message, the end of the tools array,
        // and the last message of the STABLE history — everything
        // before the current user turn, so the third breakpoint rolls
        // forward one turn at a time instead of being invalidated by it.
        if req.cache_mode == CacheMode::Explicit {
            if let Some(first) = messages.first_mut() {
                if first["role"] == "system" {
                    mark_cache_breakpoint(first);
                }
            }
            // The last message BEFORE the current user turn. Mid-turn
            // (the last message is a tool result) this still names the
            // same boundary, so the breakpoint does not move between
            // the rounds of one turn — which is the whole point.
            let current_turn = messages
                .iter()
                .rposition(|m| m["role"] == "user")
                .unwrap_or(0);
            if current_turn > 1 {
                mark_cache_breakpoint(&mut messages[current_turn - 1]);
            }
        }

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": req.stream,
        });
        if req.stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if !req.tools.is_empty() {
            let mut tools = req.tools.clone();
            if req.cache_mode == CacheMode::Explicit {
                if let Some(last) = tools.last_mut() {
                    last["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
            body["tools"] = json!(tools);
        }
        if let Some(re) = &req.reasoning_effort {
            body["reasoning_effort"] = json!(re);
        }
        Self::merge_extra_body(&mut body, req.extra_body.as_ref());
        body
    }

    /// The endpoint's own options/tools land here verbatim. A small
    /// denylist guards the transport contract the client itself owns
    /// — `cache_control` rides that same contract for free, since it
    /// only ever lives inside `messages` and `tools`, both denylisted
    /// (those keys are computed by build_body and a config must not be
    /// able to silently rewrite them); everything else passes through
    /// untouched — the endpoint is the validator, the same verdict
    /// reasoning_effort gets.
    fn merge_extra_body(body: &mut Value, extra: Option<&Value>) {
        let Some(extra) = extra else { return };
        let Value::Object(map) = extra else {
            tracing::warn!("extra_body must be an object; ignoring");
            return;
        };
        const DENYLIST: &[&str] = &["messages", "model", "stream", "stream_options", "tools"];
        for (k, v) in map {
            if DENYLIST.contains(&k.as_str()) {
                tracing::warn!("extra_body key '{k}' is transport-managed; dropped");
                continue;
            }
            body[k] = v.clone();
        }
    }

    /// Non-streaming chat completion.
    pub async fn chat(
        &self,
        req: &ChatRequest,
        provider_name: &str,
    ) -> Result<Completion, ProviderError> {
        let body = self.build_body(req);
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
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

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Network(format!("bad json: {e}")))?;
        Ok(completion_from_wire(&v, &req.model, provider_name))
    }

    /// Streaming chat completion. Calls `on_event` for each delta; returns
    /// the assembled completion.
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
        let body = self.build_body(req);
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
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
        // Orphan `</think>` terminators, out of the DISPLAY stream only
        // (see the content arm below). Some providers stream the thought
        // into `reasoning_content` and the terminator into `content`
        // regardless, and a chunk boundary can land inside the tag.
        let mut think_filter = crate::chatml::ThinkTagStream::new();
        let mut cancelled = false;
        // tool_calls accumulate across chunks, keyed by index.
        let mut tool_calls: Vec<Value> = Vec::new();
        // usage arrives on the final chunk (stream_options.include_usage).
        let mut prompt_tokens: Option<u64> = None;
        let mut completion_tokens: Option<u64> = None;
        let mut cached_tokens: Option<u64> = None;
        let mut cache_write_tokens: Option<u64> = None;
        let mut reasoning_tokens: Option<u64> = None;
        // Did the provider say it was done? `[DONE]`, a `finish_reason`,
        // or the usage chunk (`include_usage` sends it last). A stream
        // that ends with none of the three was CUT, not finished.
        let mut finished = false;

        // Tail-safe: an event the provider hung WITHOUT its terminating
        // blank line is promoted at EOF; eventsource-stream dropped it
        // and every reply lost its tail when the edge closed early
        // (the 2026-09-08 mid-word clips).
        let mut stream = crate::sse::tail_safe(resp.bytes_stream());
        while let Some(ev) = stream.next().await {
            if let Some(tok) = &cancel {
                if tok.is_cancelled() {
                    cancelled = true;
                    break;
                }
            }
            let ev = ev.map_err(|e| ProviderError::Network(format!("sse: {e}")))?;
            if ev.data == "[DONE]" {
                finished = true;
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                continue;
            };
            if v["choices"][0]["finish_reason"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
            {
                finished = true;
            }
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                finished = true;
                prompt_tokens = u["prompt_tokens"].as_u64().or(prompt_tokens);
                completion_tokens = u["completion_tokens"].as_u64().or(completion_tokens);
                let (c, w) = cache_usage(u);
                cached_tokens = c.or(cached_tokens);
                cache_write_tokens = w.or(cache_write_tokens);
                reasoning_tokens = reasoning_usage(u).or(reasoning_tokens);
            }
            let delta = &v["choices"][0]["delta"];
            if delta.is_null() {
                continue;
            }
            // REASONING BEFORE CONTENT, within one chunk. A chunk that
            // carries both is the transition chunk, and emitting the
            // answer's first token ahead of the thought's last one
            // interleaves them on every client — see the long note on
            // `emit_wire_chunk`, which had the same two arms in the
            // same wrong order.
            if let Some(t) = delta
                .get("reasoning_content")
                .and_then(|x| x.as_str())
                .or_else(|| delta.get("reasoning").and_then(|x| x.as_str()))
            {
                if !t.is_empty() {
                    reasoning.push_str(t);
                    on_event(StreamEvent::ReasoningDelta(t.to_string()));
                }
            }
            if let Some(t) = delta["content"].as_str() {
                if !t.is_empty() {
                    // The ACCUMULATED content stays raw: `run_turn` is
                    // the one place that decides what the message IS,
                    // and it strips there (and announces). What is
                    // FILTERED is what gets painted — a `</think>` the
                    // engine is about to remove anyway must not be
                    // watched appearing and then vanishing.
                    content.push_str(t);
                    let shown = think_filter.push(t);
                    if !shown.is_empty() {
                        on_event(StreamEvent::Delta(shown));
                    }
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let idx = call["index"].as_u64().unwrap_or(0) as usize;
                    while tool_calls.len() <= idx {
                        tool_calls.push(json!({
                            "id": "", "type": "function",
                            "function": { "name": "", "arguments": "" }
                        }));
                    }
                    let slot = &mut tool_calls[idx];
                    if let Some(id) = call["id"].as_str() {
                        if !id.is_empty() {
                            slot["id"] = json!(id);
                        }
                    }
                    if let Some(n) = call["function"]["name"].as_str() {
                        if !n.is_empty() {
                            let prev = slot["function"]["name"].as_str().unwrap_or("").to_string();
                            slot["function"]["name"] = json!(format!("{prev}{n}"));
                        }
                    }
                    if let Some(a) = call["function"]["arguments"].as_str() {
                        let prev = slot["function"]["arguments"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        slot["function"]["arguments"] = json!(format!("{prev}{a}"));
                    }
                }
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

        // A CUT stream (see `finished`) with nothing usable in it — no
        // answer text, no tool call, reasoning at most — is not an empty
        // answer: it is an error the caller retries. Read as a finished
        // reply it ended the turn silently with nothing said (c1aaac,
        // 2026-10-06: three turns of 29k–103k chars of thought, then
        // nothing). A cut stream that DID carry an answer keeps it — a
        // half answer beats a lost one — and says so in the log.
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
                "{provider_name}/{}: the stream closed without [DONE], finish_reason or \
                 usage; keeping the {} chars that arrived",
                req.model,
                content.chars().count()
            );
        }

        // Whatever the filter was still holding back was ordinary text.
        let tail = think_filter.finish();
        if !tail.is_empty() {
            on_event(StreamEvent::Delta(tail));
        }
        let mut message = ChatMessage::assistant(
            &content,
            if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
        );
        // Cancelled mid-stream: drop partial tool_calls (they'd be invalid
        // JSON fragments) — keep the text so the user can see what arrived.
        if !cancelled && !tool_calls.is_empty() {
            message.tool_calls = Some(json!(tool_calls));
        }
        if cancelled {
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

fn parse_message(v: &Value) -> ChatMessage {
    let mut m = ChatMessage::assistant(
        v["content"].as_str().unwrap_or_default(),
        // BOTH spellings, like the two stream readers. This used to take
        // `reasoning_content` only, while `chat_stream` and
        // `emit_wire_chunk` have always accepted `reasoning` as well —
        // so on any provider using the second spelling, a non-streaming
        // call (and EVERY plugin-driver route, which assembles its final
        // message here) streamed the thinking live and persisted none of
        // it. The session you watched had reasoning; the one you
        // reopened did not, and the driver contract above promises that
        // "the reasoning/content split … come[s] for free".
        v.get("reasoning_content")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("reasoning").and_then(|x| x.as_str()))
            .map(|s| s.to_string()),
    );
    if let Some(tc) = v.get("tool_calls") {
        if !tc.is_null() {
            m.tool_calls = Some(tc.clone());
        }
    }
    m
}

/// Resolve an api key through the one resolver (`secrets::resolve_in`:
/// the operator's environment, the vault, `secrets.env`, `.env`). Keys
/// never live in config.toml.
pub fn resolve_api_key(entry: &crate::config::ProviderEntry, home: &std::path::Path) -> String {
    let env_name = entry
        .key_env
        .clone()
        .unwrap_or_else(|| "VELES_API_KEY".into());
    crate::secrets::resolve_in(&env_name, home).unwrap_or_default()
}

/// Build the ordered route chain: primary + fallback_providers.
/// The chain is stateless — the caller decides where each call starts
/// (`agent.rs` applies within-turn stickiness on top of it).
/// The cache posture for one route: the provider's own `cache_mode`
/// wins, else the global `[cache] mode`. `[cache] enabled = false` is a
/// master switch and forces `Off` whatever either of them says — one
/// line that guarantees nothing cache-related reaches the wire.
pub(crate) fn resolve_cache_mode(
    cfg: &crate::config::VelesConfig,
    entry: &crate::config::ProviderEntry,
) -> CacheMode {
    if !cfg.cache.enabled {
        return CacheMode::Off;
    }
    entry
        .cache_mode
        .as_deref()
        .map(CacheMode::parse)
        .unwrap_or_else(|| CacheMode::parse(&cfg.cache.mode))
}

/// Build the wire client an entry asks for. `api_mode` validation
/// happened at config load; anything that reaches here is a known
/// spelling. `built_in_tools` on a chat_completions entry is dropped
/// with a warning: the concept does not exist on that wire, and a
/// config line that silently does nothing is the exact failure this
/// crate's api_mode story exists to prevent.
pub fn build_client(
    entry: &crate::config::ProviderEntry,
    api_key: &str,
    ua: &UserAgentProfile,
) -> Result<RouteClient> {
    // Reject an unusable base_url NOW, at route-build, not at first
    // request: a bare string ("definitely not a url") used to be
    // accepted here and only explode deep inside a turn.
    //
    // Not for a plugin route. `base_url` is opaque there — the DRIVER
    // owns the transport, and a driver that talks to a local socket, a
    // subprocess or nothing at all has no http(s) URL to give. It is
    // carried and reported for diagnostics, not parsed.
    let url = entry.base_url.trim();
    if entry.api_mode.trim() != "plugin" {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"));
        match rest {
            None => {
                return Err(anyhow::anyhow!(
                    "provider \"{}\": base_url must start with http:// or https://, got {url:?}",
                    entry.name
                ))
            }
            Some(r) if r.split(['/', '?', '#']).next().unwrap_or("").is_empty() => {
                return Err(anyhow::anyhow!(
                    "provider \"{}\": base_url {url:?} has no host",
                    entry.name
                ))
            }
            Some(_) => {}
        }
    }
    match entry.api_mode.trim() {
        // A wire a plugin speaks. The driver is looked up per CALL
        // (PluginRoute::resolve), not here: routes are built at boot
        // and plugins register afterwards, so requiring the driver to
        // exist now would make ordering the thing that decides whether
        // a config is valid. What is checked here is that the config
        // NAMED one — an api_mode = "plugin" entry with no `driver` is
        // a line that cannot ever do anything.
        "plugin" => {
            let name = entry
                .driver
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider \"{}\": api_mode = \"plugin\" needs `driver = \"<name>\"` \
                         naming a driver some plugin registers. `veles doctor --providers` \
                         lists the ones that are loaded.",
                        entry.name
                    )
                })?;
            Ok(RouteClient::Plugin(PluginRoute {
                driver: name.to_string(),
                base_url: entry.base_url.trim().to_string(),
                api_key: api_key.to_string(),
                endpoint_path: entry
                    .endpoint_path
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                auth_style: entry.auth_style.as_deref().unwrap_or("").trim().to_string(),
                api_version: entry
                    .api_version
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                max_output_tokens: entry.max_output_tokens,
                built_in_tools: entry.built_in_tools.clone(),
            }))
        }
        "responses" => Ok(RouteClient::Responses(
            crate::responses::ResponsesClient::new(
                &entry.base_url,
                api_key,
                ua.clone(),
                entry.built_in_tools.clone(),
            )?,
        )),
        _ => {
            if !entry.built_in_tools.is_empty() {
                tracing::warn!(
                    "provider \"{}\": built_in_tools requires api_mode = \"responses\"; \
                     dropped on chat_completions",
                    entry.name
                );
            }
            Ok(RouteClient::ChatCompletions(ProviderClient::new(
                &entry.base_url,
                api_key,
                ua.clone(),
            )?))
        }
    }
}

// ── provider drivers: a wire a PLUGIN speaks ─────────────────────────
//
// `build_client` below is a match on `api_mode` with two arms, both of
// them implemented in this crate. Until 2026-09-04 that was the whole
// story: there was no extension point at all, in either plugin
// language, so "write a provider driver" was not a thing a plugin could
// do however much of the bridge it could reach.
//
// A driver is now a third `RouteClient` variant. Deliberately a variant
// and not a trait-ification of the enum: the enum's own docstring gives
// the reason ("which wire does this route speak" must stay a value the
// rest of the engine can branch on — `proxy.rs` refuses to forward what
// it cannot translate), and a plugin route is exactly the kind of thing
// something downstream needs to be able to refuse.
//
// # The contract, and why it is small enough to write in Tcl
//
// A driver TRANSLATES; it does not parse. It receives the OpenAI-shaped
// request body [`wire_body`] already built — attachment parts, cache
// breakpoints, tool serialization, reasoning_effort, extra_body — and
// answers with an OpenAI-shaped response, which [`completion_from_wire`]
// turns into a `Completion`. So tool-call reassembly, the
// reasoning/content split and all four spellings of cache usage (see
// `cache_usage`) come for free, and what a driver author writes is the
// transport and the field mapping. That is the difference between a
// hundred lines of Tcl and a thousand.
//
// Streaming: a driver may emit zero or more events while it works, each
// an OpenAI-shaped `chat.completion.chunk` object, and the engine feeds
// them through the same delta handling a native stream uses.

/// The OpenAI chat-completions request body for one `ChatRequest`.
/// Handed to a provider driver as its input.
pub fn wire_body(req: &ChatRequest) -> Value {
    ProviderClient::build_body_for(req)
}

/// Build a `Completion` from an OpenAI-shaped response object.
///
/// The other half of a driver's contract: answer in this shape and the
/// engine does the rest. Split out of `ProviderClient::chat`, which is
/// now one of its two callers.
pub fn completion_from_wire(v: &Value, model: &str, provider: &str) -> Completion {
    let (cached_tokens, cache_write_tokens) = cache_usage(&v["usage"]);
    Completion {
        message: parse_message(&v["choices"][0]["message"]),
        model: model.to_string(),
        provider: provider.to_string(),
        prompt_tokens: v["usage"]["prompt_tokens"].as_u64(),
        completion_tokens: v["usage"]["completion_tokens"].as_u64(),
        cached_tokens,
        cache_write_tokens,
        reasoning_tokens: reasoning_usage(&v["usage"]),
    }
}

// ── streaming out of a driver that lives on another thread ───────────
//
// A `.so` driver is handed an `emit` function pointer and calls it
// directly — nothing below is needed for one. A TCL driver cannot be:
// the proc runs on the interpreter's own thread, reached through a
// blocking request/reply hop, and `veles::provider_emit` fires from
// there with no channel back to the call that started it.
//
// So the sink is parked where the interpreter thread can find it, for
// exactly the duration of one driver call. `DRIVER_TURN` makes that
// "exactly one": two concurrent Tcl driver calls would fight over the
// slot, and they would serialize on the interpreter thread anyway, so
// serializing here is the same cost stated honestly. The lock is never
// held while the driver runs — install, run, remove — or the emitting
// thread would block on the thread waiting for it.

type EmitSink = Box<dyn FnMut(&Value) + Send>;

static EMIT_SINK: std::sync::Mutex<Option<EmitSink>> = std::sync::Mutex::new(None);
static DRIVER_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with `sink` reachable from `emit_chunk`, one caller at a
/// time. The sink is removed before returning, whatever `f` did.
pub fn with_emit_sink<R>(sink: EmitSink, f: impl FnOnce() -> R) -> R {
    let _turn = DRIVER_TURN.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut slot) = EMIT_SINK.lock() {
        *slot = Some(sink);
    }
    let out = f();
    if let Ok(mut slot) = EMIT_SINK.lock() {
        *slot = None;
    }
    out
}

/// Emit one chunk to whichever driver call is in flight. False when
/// there is none — a plugin called `provider_emit` outside a driver
/// proc, which is a mistake worth reporting rather than swallowing.
pub fn emit_chunk(v: &Value) -> bool {
    match EMIT_SINK.lock() {
        Ok(mut slot) => match slot.as_mut() {
            Some(sink) => {
                sink(v);
                true
            }
            None => false,
        },
        Err(_) => false,
    }
}

/// Turn one OpenAI-shaped `chat.completion.chunk` into stream events.
///
/// A provider driver emits chunks in this shape while it works and
/// returns the assembled response at the end, so this only has to feed
/// the UI — no accumulation, unlike `ProviderClient::chat_stream`, which
/// builds its final message out of the deltas because that is all it
/// gets.
fn emit_wire_chunk<F: FnMut(StreamEvent)>(v: &Value, on_event: &mut F) {
    let delta = &v["choices"][0]["delta"];
    // REASONING FIRST, AND THE ORDER IS THE WHOLE POINT.
    //
    // A chunk carrying BOTH fields is the transition chunk: the tail
    // of the thought and the first token of the answer. Emitted
    // content-first, the client is told answer-then-thought, and every
    // client renders strictly in event order — the desktop flushes
    // each pending region before opening the other, and the SSE
    // coalescer stops at the first different kind — so there is
    // nowhere downstream to put it right.
    //
    // USER 2026-09-18: "thinking is followed by one token of output,
    // then one token of thinking again (the tail of the previous
    // thinking block) and then the rest of the regular output."
    // Reproduced exactly by the test below, which recorded
    // [reasoning, delta, reasoning, delta] for one such chunk.
    //
    // Both spellings, same as the native stream reader: providers
    // disagree about `reasoning_content` vs `reasoning`.
    if let Some(t) = delta
        .get("reasoning_content")
        .and_then(|x| x.as_str())
        .or_else(|| delta.get("reasoning").and_then(|x| x.as_str()))
    {
        if !t.is_empty() {
            on_event(StreamEvent::ReasoningDelta(t.to_string()));
        }
    }
    if let Some(t) = delta["content"].as_str() {
        if !t.is_empty() {
            on_event(StreamEvent::Delta(t.to_string()));
        }
    }
}

/// What a driver does: request body in, response body out, with an
/// `emit` callback for streaming chunks along the way.
///
/// SYNC, like every other plugin handler, and called from a blocking
/// thread for the same reason — a driver's whole job is I/O, and in Tcl
/// it is a proc on the interpreter thread.
pub type ProviderDriverFn =
    Arc<dyn Fn(&Value, &mut dyn FnMut(&Value)) -> Result<Value, String> + Send + Sync>;

/// A registered driver: the name a `[[custom_providers]]` entry asks
/// for by `driver = "..."`, plus which plugin supplied it, so `veles
/// doctor --providers` can say where a wire came from.
#[derive(Clone)]
pub struct ProviderDriver {
    pub name: String,
    pub plugin: String,
    pub call: ProviderDriverFn,
}

/// The provider-driver registry. OWNER-TRACKED (M4/R2): each driver
/// records the registering thread's owner, so a module's drivers can be
/// purged on reload without touching core's. Core/plugin registrations on
/// untagged threads → owner `None`, never purged.
static DRIVERS: std::sync::OnceLock<crate::owned::OwnedRegistry<String, ProviderDriver>> =
    std::sync::OnceLock::new();

fn drivers_cell() -> &'static crate::owned::OwnedRegistry<String, ProviderDriver> {
    DRIVERS.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register a provider driver. Replace-by-name, like every other plugin
/// registry here: a reload must not stack two copies of one wire.
/// The owner is read from the calling thread (M4).
pub fn register_driver(driver: ProviderDriver) -> Result<(), String> {
    if driver.name.trim().is_empty() {
        return Err("a provider driver needs a name".to_string());
    }
    drivers_cell().insert(driver.name.clone(), driver);
    Ok(())
}

/// Remove every driver `owner` registered (R3 teardown on a module
/// reload). `None` (core) removes nothing. Returns the count removed.
pub fn purge_owner_drivers(owner: Option<&'static str>) -> usize {
    drivers_cell().purge(owner)
}

/// Drop one provider driver `owner` registered (an ABI 4 plugin's
/// `unregister`); other owners' same-named drivers stay.
pub fn remove_owned_driver(name: &str, owner: Option<&'static str>) -> bool {
    drivers_cell().remove_owned(&name.to_string(), owner)
}

pub fn driver(name: &str) -> Option<ProviderDriver> {
    drivers_cell().with(&name.to_string(), |d| d.clone())
}

/// Every registered driver, for diagnostics.
pub fn drivers() -> Vec<ProviderDriver> {
    drivers_cell().values_cloned()
}

/// A route whose wire is a plugin's.
///
/// Holds the driver NAME rather than the driver, resolved per call.
/// A route outlives a plugin reload, and a stale `Arc` would keep
/// calling the old code after the plugin that owned it was replaced.
///
/// `Default` exists for construction ergonomics — the four endpoint knobs
/// are all optional and all mean "let the driver decide" when empty, so a
/// default-built route is a coherent value. `driver` is NOT optional in
/// practice: `build_client` refuses an entry that does not name one, and
/// `resolve` reports the empty name loudly rather than guessing.
#[derive(Debug, Clone, Default)]
pub struct PluginRoute {
    pub driver: String,
    /// Reported by `base_url()` so diagnostics have something to print;
    /// what a driver does with it is the driver's business.
    pub base_url: String,
    pub api_key: String,
    /// The endpoint knobs a driver needs but cannot know on its own,
    /// because they are facts about the GATEWAY and not about the wire.
    /// All four are optional and all four are carried verbatim to the
    /// driver under the reserved `veles` key; a driver applies its own
    /// default when one is absent. See `ProviderEntry` for why each is a
    /// typed field rather than an `extra_body` key.
    pub endpoint_path: String,
    pub auth_style: String,
    pub api_version: String,
    pub max_output_tokens: Option<u64>,
    /// Server-side tools (`ProviderEntry.built_in_tools`), carried to the
    /// driver under the same reserved key.
    ///
    /// This wire has real server tools — Anthropic runs `web_search` and
    /// `code_execution` itself and returns their results as content blocks
    /// — so the field is not the Responses-API-only concept its
    /// `ProviderEntry` doc describes. On a plugin route it is the driver's
    /// job to translate the names into whatever the wire wants; the engine
    /// only guarantees the list arrives.
    pub built_in_tools: Vec<String>,
}

impl PluginRoute {
    fn resolve(&self) -> Result<ProviderDriver, ProviderError> {
        driver(&self.driver).ok_or_else(|| {
            let known = drivers()
                .iter()
                .map(|d| format!("{} (from {})", d.name, d.plugin))
                .collect::<Vec<_>>()
                .join(", ");
            ProviderError::Network(format!(
                "no provider driver named `{}` is registered{}",
                self.driver,
                if known.is_empty() {
                    " — no plugin has registered one".to_string()
                } else {
                    format!(" — registered: {known}")
                }
            ))
        })
    }

    /// Run the driver and assemble the answer. `on_chunk` receives each
    /// streamed chunk object; a non-streaming call simply never emits.
    /// `on_chunk` is `Send + 'static` because a Tcl driver's chunks
    /// arrive from the interpreter thread and the sink has to be
    /// reachable from there (see `with_emit_sink`).
    fn run(
        &self,
        req: &ChatRequest,
        provider_name: &str,
        on_chunk: impl FnMut(&Value) + Send + 'static,
    ) -> Result<Completion, ProviderError> {
        let d = self.resolve()?;
        let mut body = wire_body(req);
        // The route's own identity, under a reserved key.
        //
        // A driver needs the endpoint and the credential for THIS
        // route, and it has no other channel to them: `config_get`
        // could only ever reach a global, which would make one driver
        // unable to serve two provider entries with different keys —
        // the ordinary case for a work account and a personal one.
        // These are the fields `[[custom_providers]] base_url` and
        // `key_env` already mean; this is what carries them across.
        //
        // Engine-supplied and not part of the wire: a driver reads
        // `veles` and does not forward it.
        body["veles"] = serde_json::json!({
            "base_url": self.base_url,
            "api_key": self.api_key,
            "provider": provider_name,
            "driver": self.driver,
            // The endpoint knobs, carried verbatim. A driver applies its
            // own default for each one it finds empty, which is why they
            // are always present: "absent" and "empty" have to mean the
            // same thing to a driver, or one of the two becomes a silent
            // misconfiguration.
            "endpoint_path": self.endpoint_path,
            "auth_style": self.auth_style,
            "api_version": self.api_version,
            "max_output_tokens": self.max_output_tokens,
            "built_in_tools": self.built_in_tools,
        });
        // The sink is installed for the whole call, so a driver on
        // ANOTHER thread can emit into it; a driver holding the
        // callback directly (the C ABI shape) ignores it.
        let mut on_chunk = on_chunk;
        let out = with_emit_sink(Box::new(move |c: &Value| on_chunk(c)), || {
            let mut noop = |_: &Value| {};
            (d.call)(&body, &mut noop)
        })
        .map_err(|e| ProviderError::Network(format!("driver `{}`: {e}", self.driver)))?;
        // A driver that answers an error object gets the same treatment
        // an endpoint would: the message, not a silent empty turn.
        //
        // And the same CLASSIFICATION, which is the part that used to be
        // missing. Every driver error became `Client` regardless of the
        // status it reported, so `should_fallback` — 429, 5xx, network —
        // could never fire on a plugin route. An Anthropic-compatible
        // gateway answering 429 would fail the turn outright instead of
        // falling through to the next route in the chain, which is the one
        // thing the chain exists to do. The status the driver reports now
        // decides the variant exactly as a real HTTP status does.
        if let Some(msg) = out["error"].as_str() {
            return Err(provider_error_from_status(
                out["status"].as_u64().unwrap_or(502) as u16,
                msg.to_string(),
            ));
        }
        Ok(completion_from_wire(&out, &req.model, provider_name))
    }
}

/// Classify an HTTP status the way the native clients do.
///
/// Split out of `PluginRoute::run` rather than left inline there, because
/// "which variant does a 429 become" is exactly the sort of question that
/// drifts when every caller answers it for itself — and the drift is
/// invisible until a failover chain silently stops failing over. The two
/// native clients (`ProviderClient::chat` and `chat_stream`) still spell
/// theirs out inline against a real status code; this is the same table
/// for a status a DRIVER reported.
pub(crate) fn provider_error_from_status(status: u16, body: String) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited(body),
        500..=599 => ProviderError::Server { status, body },
        _ => ProviderError::Client { status, body },
    }
}

/// Routes whose credential never resolved, as (provider, env var name).
///
/// Keys live in `.env`, never in config, by design (the hermes
/// pattern) — so `veles migrate` carries a config across a machine and
/// leaves every SECRET behind. Nothing said so: `veles doctor` printed
/// the route chain out of that config and exited 0, and the first turn
/// was where you found out. This is what lets doctor say it instead.
pub fn routes_missing_keys(
    cfg: &crate::config::VelesConfig,
    routes: &[Route],
) -> Vec<(String, String)> {
    routes
        .iter()
        .filter(|r| r.client.api_key().is_empty())
        .map(|r| {
            // Name the variable to SET, not just the route that is
            // broken — the whole fix is one line in one file.
            let env = cfg
                .custom_providers
                .iter()
                .find(|p| p.name == r.provider)
                .and_then(|p| p.key_env.clone())
                .unwrap_or_else(|| "VELES_API_KEY".into());
            (r.provider.clone(), env)
        })
        .collect()
}

pub fn build_routes(cfg: &crate::config::VelesConfig) -> Result<Vec<Route>> {
    let home = crate::config::config_dir(); // .env lives here since the XDG split
    let mut routes = Vec::new();
    let ua = resolve_user_agent(cfg.agent.user_agent.as_deref(), &home);
    tracing::debug!("inference User-Agent: {}", ua.user_agent);

    // Primary.
    let primary_key = cfg.model.provider.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no model.provider in config — run `veles setup` to choose a provider and model"
        )
    })?;
    let primary_model = cfg.model.default.clone().ok_or_else(|| {
        anyhow::anyhow!("no model.default in config — run `veles setup` to choose a model")
    })?;
    let entry = cfg
        .provider(&primary_key)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {primary_key}"))?;
    routes.push(Route {
        provider: entry.name.clone(),
        model: primary_model.clone(),
        client: build_client(entry, &resolve_api_key(entry, &home), &ua)?,
        context_tokens: entry
            .models
            .context_length(&primary_model)
            .or(entry.context_length),
        reasoning_effort: entry.reasoning_effort.clone(),
        extra_body: entry.extra_body.clone(),
        replay_reasoning: entry.replay_reasoning.unwrap_or(false),
        cache_mode: resolve_cache_mode(cfg, entry),
    });

    // Fallbacks (config order).
    for fb in &cfg.fallback_providers {
        let Some(entry) = cfg.provider(&fb.provider) else {
            tracing::warn!("fallback provider not found: {}", fb.provider);
            continue;
        };
        let model = fb
            .model
            .clone()
            .or_else(|| entry.model.clone())
            .unwrap_or_default();
        routes.push(Route {
            provider: entry.name.clone(),
            context_tokens: entry.models.context_length(&model).or(entry.context_length),
            model,
            client: build_client(entry, &resolve_api_key(entry, &home), &ua)?,
            reasoning_effort: entry.reasoning_effort.clone(),
            extra_body: entry.extra_body.clone(),
            replay_reasoning: entry.replay_reasoning.unwrap_or(false),
            cache_mode: resolve_cache_mode(cfg, entry),
        });
    }
    Ok(routes)
}

/// Resolve a value through `secrets::resolve_in` (the operator's
/// environment, the vault, `secrets.env`, the home dir's `.env`; keys
/// never live in config). Migration compat: if still unset, also checks
/// ~/.hermes/.env (read-only, same policy as the config fallback).
/// Returns None when unset — adapters use this for optional settings.
pub fn resolve_env_or_dotenv(env_name: &str, home: &std::path::Path) -> Option<String> {
    if let Some(v) = crate::secrets::resolve_in(env_name, home) {
        return Some(v);
    }
    // Migration compat: hermes' .env while veles adopts it.
    if let Some(user_home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
        if let Some(v) = dotenv_lookup(env_name, &user_home.join(".hermes")) {
            return Some(v);
        }
    }
    None
}

/// Strict variant (pitfall 16, extended 2026-08-21): veles' own sources
/// ONLY (environment, vault, its own env files), never the hermes .env
/// fallback. For secrets where inheriting
/// hermes' value is an ACTIVE HAZARD rather than migration compat —
/// chiefly TELEGRAM_BOT_TOKEN: a scratch/test gateway without its own
/// token silently grabs hermes' live bot and fights its poller with
/// 409 conflicts (happened in testing 2026-08-21). The provider keys
/// stay on the fallback variant — they are the migration-compat case.
pub fn resolve_env_or_dotenv_veles_only(env_name: &str, home: &std::path::Path) -> Option<String> {
    crate::secrets::resolve_in(env_name, home)
}

fn dotenv_lookup(env_name: &str, dir: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(".env")).ok()?;
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == env_name && !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// NDJSON event printer. No LLM in the loop — pure serialization.
/// Returns — callers emit the final `result` line separately. The
/// headless `veles chat -qj` surface (veles-frontends' cli.rs) prints these;
/// it lives beside `StreamEvent` so the engine can test what it emits.
pub fn json_event(ev: &StreamEvent) -> String {
    let obj = match ev {
        StreamEvent::Route {
            provider,
            model,
            reasoning_effort,
        } => {
            serde_json::json!({"event": "route", "provider": provider, "model": model, "reasoning_effort": reasoning_effort})
        }
        StreamEvent::Delta(t) => serde_json::json!({"event": "delta", "text": t}),
        StreamEvent::ReasoningDelta(t) => serde_json::json!({"event": "reasoning", "text": t}),
        StreamEvent::ToolStart(name, args) => {
            serde_json::json!({"event": "tool_start", "tool": name, "args": args})
        }
        StreamEvent::ToolEnd(name, result) => {
            serde_json::json!({"event": "tool_end", "tool": name, "result": result})
        }
        StreamEvent::Ask(req) => serde_json::json!({
            "event": "ask",
            "id": req.id,
            "kind": match req.kind {
                crate::ask::AskKind::Approval => "approval",
                crate::ask::AskKind::Clarify => "clarify",
                crate::ask::AskKind::Failover => "failover",
                crate::ask::AskKind::Secret => "secret",
                crate::ask::AskKind::Ticket => "ticket",
                crate::ask::AskKind::Challenge => "challenge",
            },
            "prompt": req.prompt,
            "detail": req.detail,
            "choices": req.choices,
            "multi": req.multi,
            "questions": req.questions,
        }),
        StreamEvent::Done(m) => {
            serde_json::json!({"event": "done", "role": m.role, "text": m.content})
        }
        StreamEvent::Cancelled => serde_json::json!({"event": "cancelled"}),
        StreamEvent::Note(n) => serde_json::json!({
            "event": "note",
            "kind": n.kind.as_str(),
            "summary": n.summary,
            "detail": n.detail,
            "toggle": n.toggle,
        }),
    };
    obj.to_string()
}

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code))]
pub mod tests {
    use super::*;

    /// A garbage base_url used to be accepted here and only explode
    /// deep inside a turn; it now fails at route-build.
    #[test]
    fn build_client_rejects_an_unusable_base_url() {
        let ua = UserAgentProfile {
            user_agent: "ua".into(),
            extra_headers: vec![],
        };
        for bad in [
            "definitely not a url",
            "example.com/v1",
            "ftp://x",
            "https://",
        ] {
            let entry = crate::config::ProviderEntry {
                name: "p".into(),
                base_url: bad.into(),
                ..Default::default()
            };
            assert!(
                build_client(&entry, "k", &ua).is_err(),
                "must reject {bad:?}"
            );
        }
        let ok = crate::config::ProviderEntry {
            name: "p".into(),
            base_url: "https://api.example.com/v1".into(),
            ..Default::default()
        };
        assert!(build_client(&ok, "k", &ua).is_ok());
    }

    #[test]
    fn extra_body_merges_verbatim_and_drops_the_denylist() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            reasoning_effort: None,
            stream: true,
            replay_reasoning: false,
            cache_mode: CacheMode::Auto,
            extra_body: Some(serde_json::json!({
                "enable_search": true,
                "extra_options": {"a": 1},
                "model": "SHOULD-NOT-WIN"
            })),
        };
        let client = ProviderClient::new(
            "https://example.invalid/v1",
            "k",
            UserAgentProfile {
                user_agent: "ua".into(),
                extra_headers: vec![],
            },
        )
        .unwrap();
        let body = client.build_body(&req);
        assert_eq!(body["enable_search"], true);
        assert_eq!(body["extra_options"]["a"], 1);
        assert_eq!(
            body["model"], "m",
            "denylisted transport keys must not be overridable"
        );
    }

    /// A scratch home with no .env so resolve_user_agent never picks up
    /// ambient VELES_USER_AGENT from the real home.
    fn scratch() -> std::path::PathBuf {
        crate::testenv::kept_dir()
    }

    /// An attachment only becomes a parts array on a USER message.
    ///
    /// USER: "Is image/file support implemented at all?" — `image_url`
    /// appeared nowhere in this file, so whatever the frontends did,
    /// the model never received a picture. This is the wire format that
    /// was missing.
    #[test]
    fn a_user_attachment_becomes_an_image_part() {
        let home = scratch();
        let ua = resolve_user_agent(None, &home);
        let client = ProviderClient::new("http://127.0.0.1:0/v1", "k", ua).unwrap();
        let mut m = ChatMessage::user("what is this");
        m.attachments.push(crate::media::Attachment {
            source: "/tmp/a.png".into(),
            mime: "image/png".into(),
            data_url: "data:image/png;base64,AAAA".into(),
        });
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![m],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Auto,
            extra_body: None,
        };
        let body = client.build_body(&req);
        let parts = body["messages"][0]["content"]
            .as_array()
            .expect("content must be a parts array when there is an attachment");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    /// Everything without an attachment keeps a plain string. Several
    /// providers reject a parts array on a system or tool message, so
    /// "always send parts" would break every ordinary turn.
    #[test]
    fn a_message_without_attachments_keeps_a_plain_string_content() {
        let home = scratch();
        let ua = resolve_user_agent(None, &home);
        let client = ProviderClient::new("http://127.0.0.1:0/v1", "k", ua).unwrap();
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "be helpful".into(),
                    reasoning: None,
                    display_reasoning: None,
                    tool_calls: None,
                    tool_meta: None,
                    attachments: Vec::new(),
                    injection: None,
                    msg_id: None,
                },
                ChatMessage::user("plain question"),
            ],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Auto,
            extra_body: None,
        };
        let body = client.build_body(&req);
        assert_eq!(body["messages"][0]["content"], "be helpful");
        assert_eq!(body["messages"][1]["content"], "plain question");
    }

    /// A crash mid-persist can leave an assistant row with empty content
    /// and no tool_calls, or a tool row with empty content — both are a
    /// hard 400 from the provider ("the message at position N with role
    /// 'assistant' must not be empty"). The wire builder must repair them
    /// rather than fail the request. Content is never dropped, only
    /// replaced with a placeholder when the row is provably invalid.
    #[test]
    fn an_empty_assistant_or_tool_message_is_repaired_not_rejected() {
        let home = scratch();
        let ua = resolve_user_agent(None, &home);
        let client = ProviderClient::new("http://127.0.0.1:0/v1", "k", ua).unwrap();

        let mut empty_assistant = ChatMessage::assistant("", None);
        empty_assistant.msg_id = Some(42);

        let empty_tool = ChatMessage {
            role: "tool".into(),
            content: String::new(),
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: Some(ToolMeta {
                tool_call_id: "call_1".into(),
                name: "terminal".into(),
            }),
            attachments: Vec::new(),
            injection: None,
            msg_id: Some(43),
        };

        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::user("hi"),
                empty_assistant,
                ChatMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    reasoning: None,
                    display_reasoning: None,
                    tool_calls: Some(
                        serde_json::json!([{"id":"call_1","type":"function","function":{"name":"terminal","arguments":"{}"}}]),
                    ),
                    tool_meta: None,
                    attachments: Vec::new(),
                    injection: None,
                    msg_id: Some(44),
                },
                empty_tool,
                ChatMessage::user("go on"),
            ],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Auto,
            extra_body: None,
        };
        let body = client.build_body(&req);
        let msgs = body["messages"].as_array().unwrap();

        // Empty assistant with no tool_calls got a placeholder, not "".
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "[interrupted tool round]");

        // Assistant with tool_calls keeps empty content (that's legal).
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "");
        assert!(msgs[2]["tool_calls"].is_array());

        // Empty tool result got a named placeholder.
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["content"], "[terminal produced no output]");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["name"], "terminal");
    }

    #[test]
    fn default_is_honest_veles() {
        let home = scratch();
        let p = resolve_user_agent(None, &home);
        assert!(
            p.user_agent.starts_with("veles-agent/"),
            "got: {}",
            p.user_agent
        );
        assert!(p.extra_headers.is_empty());
    }

    #[test]
    fn hermes_preset_is_sdk_faithful() {
        let home = scratch();
        let p = resolve_user_agent(Some("hermes"), &home);
        assert_eq!(p.user_agent, "OpenAI/Python 2.24.0");
        // The stainless headers are what openai-python sends; a partial
        // profile would be MORE detectable than none.
        let keys: Vec<&str> = p.extra_headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"X-Stainless-Lang"));
        assert!(keys.contains(&"X-Stainless-Package-Version"));
        assert!(keys.contains(&"X-Stainless-Runtime-Version"));
        assert!(p
            .extra_headers
            .iter()
            .any(|(k, v)| k == "X-Stainless-Package-Version" && v == "2.24.0"));
    }

    #[test]
    fn claw_preset_matches_openclaw() {
        let home = scratch();
        let p = resolve_user_agent(Some("claw"), &home);
        assert_eq!(p.user_agent, "OpenClaw-Gateway/1.0");
        let p2 = resolve_user_agent(Some("openclaw"), &home);
        assert_eq!(p2.user_agent, "OpenClaw-Gateway/1.0");
    }

    #[test]
    fn custom_string_passes_through() {
        let home = scratch();
        let p = resolve_user_agent(Some("my-corp-agent/9.9"), &home);
        assert_eq!(p.user_agent, "my-corp-agent/9.9");
        assert!(p.extra_headers.is_empty());
    }

    #[test]
    fn empty_or_whitespace_falls_back_to_default() {
        let home = scratch();
        assert!(resolve_user_agent(Some(""), &home)
            .user_agent
            .starts_with("veles-agent/"));
        assert!(resolve_user_agent(Some("   "), &home)
            .user_agent
            .starts_with("veles-agent/"));
    }

    #[test]
    fn client_carries_the_profile() {
        let home = scratch();
        let p = resolve_user_agent(Some("hermes"), &home);
        let c = ProviderClient::new("http://localhost:1", "key", p.clone()).unwrap();
        assert_eq!(c.ua.user_agent, "OpenAI/Python 2.24.0");
        assert_eq!(c.ua.extra_headers.len(), p.extra_headers.len());
    }
    // ── the streaming parse loop ────────────────────────────────────
    //
    // `chat_stream` had NO test at all: the crate's dev-dependencies are
    // `tempfile` alone, so nothing exercised the SSE loop that splits
    // content from reasoning, accumulates tool calls across chunks,
    // reads usage, and emits the events every surface renders. A whole
    // class of bug — including `StreamEvent::Route` carrying the wrong
    // model — was reachable only by running a real turn.
    //
    // No new dependency: tokio is already here, so a one-shot listener
    // on 127.0.0.1:0 is enough. Hermetic and offline.
    /// Serve a SEQUENCE of canned bodies, one per connection, so a
    /// multi-round turn (tool call, then answer) can be driven.
    /// `pub(crate)` because agent.rs's tests use it too.
    ///
    /// Two details that a naive fixture gets wrong, and that made a
    /// correct engine look broken:
    ///
    ///  * READ THE WHOLE REQUEST first. One `read()` returns whatever a
    ///    single TCP segment carried, and an agent request is ~36 KB of
    ///    system prompt and tool schemas.
    ///  * Half-close and drain before dropping. Dropping a socket with
    ///    unread data in its receive buffer makes the kernel send RST
    ///    instead of FIN, and the reply already written is discarded.
    pub async fn sse_server_seq(bodies: Vec<String>) -> String {
        sse_server_seq_capture(bodies).await.0
    }

    /// Like `sse_server_seq`, but keeping every request it served —
    /// headers and body, raw — for the tests that assert on what went
    /// OUT, not just on what the engine made of what came back (the
    /// responses turn test: round two's `input` must carry the
    /// `function_call_output` item).
    pub async fn sse_server_seq_capture(
        bodies: Vec<String>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        crate::rt::spawn(async move {
            for body in bodies {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut raw = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut need: Option<usize> = None;
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&chunk[..n]);
                    if need.is_none() {
                        if let Some(hdr_end) =
                            raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                        {
                            let head = String::from_utf8_lossy(&raw[..hdr_end]).to_lowercase();
                            let len = head
                                .split("content-length:")
                                .nth(1)
                                .and_then(|t| t.split(['\r', '\n']).next())
                                .and_then(|t| t.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(hdr_end + len);
                        }
                    }
                    if matches!(need, Some(total) if raw.len() >= total) {
                        break;
                    }
                }
                if let Ok(mut cap) = cap.lock() {
                    cap.push(String::from_utf8_lossy(&raw).to_string());
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/event-stream\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
                let mut sink = [0u8; 4096];
                while matches!(sock.read(&mut sink).await, Ok(n) if n > 0) {}
            }
        });
        (format!("http://{addr}/v1"), captured)
    }

    pub(crate) async fn sse_server(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        crate::rt::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain whatever the client sends; we only need to have
                // read it before replying.
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/event-stream\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{addr}/v1")
    }

    fn stream_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream: true,
            replay_reasoning: false,
            cache_mode: CacheMode::Auto,
            extra_body: None,
        }
    }

    /// The live shape from the Alibaba token-plan endpoints, byte for
    /// byte: the thought goes into `reasoning_content` correctly, and
    /// the TERMINATOR is put into `content` anyway — split across
    /// chunks, because that is where the chunk boundary actually fell.
    ///
    /// USER 2026-09-16: *"there's a verbatim `</think>` printed
    /// sometimes in the live transcript."* Nothing painted it on
    /// purpose; nothing removed it either.
    ///
    /// What this pins is the split of duty. The DISPLAY stream is
    /// filtered here, because a tag the engine is about to remove must
    /// not be watched appearing and then vanishing. The ACCUMULATED
    /// content is left raw, because `run_turn` is the one place that
    /// decides what a message is — it strips there and announces it,
    /// and two strippers disagreeing about one message would be worse
    /// than the tag.
    #[tokio::test]
    async fn an_orphan_think_terminator_never_reaches_the_display_stream() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"weighing it up\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\\n</thi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk>\\n\\nthe \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");

        let mut events = Vec::new();
        let c = client
            .chat_stream(&stream_req("m1"), "prov", None, |e| events.push(e))
            .await
            .expect("stream");

        let painted: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            painted, "the answer",
            "the transcript shows the answer and nothing else — not the \
             tag, and not the blank lines it was padded with"
        );
        assert_eq!(
            c.message.reasoning.as_deref(),
            Some("weighing it up"),
            "the thought itself was always classified correctly; it is \
             untouched"
        );
        assert!(
            c.message.content.contains("</think>"),
            "and the ASSEMBLED content is deliberately still raw — \
             run_turn strips it there, where the removal can be announced"
        );
    }

    /// A stream that closes with neither `[DONE]`, a `finish_reason` nor
    /// usage, and carried only reasoning, was CUT: an error to retry, not
    /// a finished reply with no text (the c1aaac aborted turns).
    #[tokio::test]
    async fn a_stream_cut_with_only_reasoning_is_truncated() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"still thinking\"}}]}\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let err = client
            .chat_stream(&stream_req("m1"), "prov", None, |_| {})
            .await
            .expect_err("a cut stream is an error");
        assert!(matches!(err, ProviderError::Truncated(_)), "{err:?}");
        assert!(err.should_fallback(), "the caller may retry or fall back");
    }

    /// The same cut AFTER answer text arrived keeps the text (a half
    /// answer beats a lost one), and a stream that ended with a
    /// `finish_reason` but no `[DONE]` is a finished one.
    #[tokio::test]
    async fn a_cut_after_answer_text_keeps_it_and_finish_reason_counts_as_done() {
        const CUT: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"half an ans\"}}]}\n\n",
        );
        let url = sse_server(CUT).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let c = client
            .chat_stream(&stream_req("m1"), "prov", None, |_| {})
            .await
            .expect("kept");
        assert_eq!(c.message.content, "half an ans");

        const FINISHED: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let url = sse_server(FINISHED).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let c = client
            .chat_stream(&stream_req("m1"), "prov", None, |_| {})
            .await
            .expect("a finish_reason is a finish, even with no answer text");
        assert!(c.message.content.is_empty());
    }

    /// A driver's final message goes through `completion_from_wire`,
    /// and the reasoning field has two spellings in the wild. The two
    /// STREAM readers have always accepted both; this one took
    /// `reasoning_content` alone — so on a provider using the other
    /// name, the thinking was streamed to the transcript live and then
    /// persisted nowhere. Reopen the session and it is gone, which is
    /// exactly the shape of "the reopened transcript lost my thinking".
    ///
    /// The driver contract's own doc promises that "the
    /// reasoning/content split … come[s] for free", so this is the
    /// promise being kept rather than a new feature.
    #[test]
    fn a_driver_response_keeps_its_reasoning_under_either_spelling() {
        let named = serde_json::json!({
            "choices": [{ "message": { "content": "hi", "reasoning_content": "the long way" } }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 },
        });
        assert_eq!(
            completion_from_wire(&named, "m", "p")
                .message
                .reasoning
                .as_deref(),
            Some("the long way"),
        );

        let other = serde_json::json!({
            "choices": [{ "message": { "content": "hi", "reasoning": "the long way" } }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 },
        });
        assert_eq!(
            completion_from_wire(&other, "m", "p")
                .message
                .reasoning
                .as_deref(),
            Some("the long way"),
            "the spelling the stream readers accept must persist too, or \
             the live transcript and the reopened one disagree"
        );

        // Neither: absent stays absent. "No reasoning reported" is not
        // the same fact as "reasoning was empty".
        let silent = serde_json::json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": {},
        });
        assert!(completion_from_wire(&silent, "m", "p")
            .message
            .reasoning
            .is_none());
    }

    /// Reasoning and content are DIFFERENT fields and must not be mixed:
    /// a model that streams `reasoning_content` first and `content`
    /// after is the common shape, and conflating them is what makes a
    /// transcript show thinking with no answer.
    #[tokio::test]
    async fn chat_stream_splits_reasoning_from_content() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"harder\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"the \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":22}}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");

        let mut events = Vec::new();
        let c = client
            .chat_stream(&stream_req("m1"), "prov", None, |e| events.push(e))
            .await
            .expect("stream");

        assert_eq!(c.message.content, "the answer");
        assert_eq!(
            c.message.reasoning.as_deref(),
            Some("think harder"),
            "reasoning is kept separately, not appended to content"
        );
        assert_eq!(c.prompt_tokens, Some(11));
        assert_eq!(c.completion_tokens, Some(22));

        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["the ", "answer"],
            "content streamed as it came"
        );
        let rdeltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(rdeltas, vec!["think ", "harder"]);
    }

    /// The Route event names the provider AND the model.
    ///
    /// It carried the provider alone, so no surface could say which
    /// model answered — USER: "should be full provider->model chain".
    /// This is the emit site; nothing reached it before.
    #[tokio::test]
    async fn chat_stream_route_event_names_provider_and_model() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");

        let mut routes = Vec::new();
        client
            .chat_stream(&stream_req("qwen3-max"), "ali", None, |e| {
                if let StreamEvent::Route {
                    provider,
                    model,
                    reasoning_effort: _,
                } = e
                {
                    routes.push((provider, model));
                }
            })
            .await
            .expect("stream");

        assert_eq!(
            routes,
            vec![("ali".to_string(), "qwen3-max".to_string())],
            "the route event must carry the model that was actually requested"
        );
    }

    /// Tool calls arrive SPLIT ACROSS CHUNKS — the name in one, the
    /// arguments a character at a time after it. Reassembly is the whole
    /// job, and a partial join produces invalid JSON the model then gets
    /// blamed for.
    #[tokio::test]
    async fn chat_stream_reassembles_tool_calls_across_chunks() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"term\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let c = client
            .chat_stream(&stream_req("m"), "p", None, |_| {})
            .await
            .expect("stream");

        let calls = c.message.tool_calls.expect("tool_calls");
        let arr = calls.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "call_1");
        assert_eq!(arr[0]["function"]["name"], "term");
        assert_eq!(
            arr[0]["function"]["arguments"], "{\"cmd\":\"ls\"}",
            "the arguments must be joined in order into valid JSON"
        );
    }

    /// A chunk that is not JSON, or carries no delta, must be skipped
    /// rather than ending the stream — providers really do send keepalive
    /// and comment lines mid-stream.
    #[tokio::test]
    async fn chat_stream_survives_junk_chunks() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
            "data: not json at all\n\n",
            "data: {\"choices\":[{}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let c = client
            .chat_stream(&stream_req("m"), "p", None, |_| {})
            .await
            .expect("stream");
        assert_eq!(
            c.message.content, "ab",
            "a junk chunk is skipped, not fatal"
        );
    }

    /// THE 2026-09-08 CLIP: an edge that closes right after the final
    /// delta, without the blank line SSE events are supposed to carry,
    /// lost the tail of every reply — eventsource-stream drops an
    /// unterminated final event, so live transcripts ended mid-word
    /// ("apply the three change", "carves a wr..."). The hung final
    /// event must survive EOF.
    #[tokio::test]
    async fn chat_stream_keeps_the_final_delta_when_the_edge_closes_mid_event() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"head\"}}]}\n\n",
            // Final event ends with ONE newline — no blank line — and
            // the connection closes right here.
            "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let c = client
            .chat_stream(&stream_req("m"), "p", None, |_| {})
            .await
            .expect("stream");
        assert_eq!(
            c.message.content, "headtail",
            "the hung final delta must survive EOF"
        );
    }

    /// An empty content delta must not be emitted: surfaces append it
    /// verbatim, and a stream of empty strings is how a transcript grows
    /// nothing while looking busy.
    #[tokio::test]
    async fn chat_stream_does_not_emit_empty_deltas() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"real\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let mut n_delta = 0;
        let mut n_reason = 0;
        let c = client
            .chat_stream(&stream_req("m"), "p", None, |e| match e {
                StreamEvent::Delta(_) => n_delta += 1,
                StreamEvent::ReasoningDelta(_) => n_reason += 1,
                _ => {}
            })
            .await
            .expect("stream");
        assert_eq!(n_delta, 1, "only the non-empty content delta is emitted");
        assert_eq!(n_reason, 0, "an empty reasoning delta is not emitted");
        assert_eq!(c.message.content, "real");
        assert!(
            c.message.reasoning.is_none(),
            "reasoning that was only empty strings stays None"
        );
    }

    // ── cache accounting (Step 0.1) ───────────────────────────────────
    //
    // Every provider that matters to this configuration does automatic
    // prefix caching, and each reports it under a DIFFERENT field. The
    // harness used to read prompt_tokens/completion_tokens and drop the
    // rest on the floor — paying full price for a ~90% cacheable prefix
    // with no surface able to say so.

    /// OpenAI / z.ai / OpenRouter shape.
    #[test]
    fn cache_usage_reads_the_openai_shape() {
        let u = serde_json::json!({
            "prompt_tokens": 1000,
            "prompt_tokens_details": { "cached_tokens": 900 }
        });
        assert_eq!(cache_usage(&u), (Some(900), None));
    }

    /// DeepSeek (incl. Aliyun compatible-mode) shape.
    #[test]
    fn cache_usage_reads_the_deepseek_shape() {
        let u = serde_json::json!({
            "prompt_tokens": 27320,
            "prompt_cache_hit_tokens": 27000,
            "prompt_cache_miss_tokens": 320
        });
        assert_eq!(cache_usage(&u), (Some(27000), None));
    }

    /// Anthropic shape: a read AND a write side.
    #[test]
    fn cache_usage_reads_the_anthropic_shape() {
        let u = serde_json::json!({
            "input_tokens": 4,
            "cache_read_input_tokens": 26000,
            "cache_creation_input_tokens": 1300
        });
        assert_eq!(cache_usage(&u), (Some(26000), Some(1300)));
    }

    /// A provider that reports no cache field at all says NOTHING —
    /// that is None, not Some(0). The UI needs the distinction to say
    /// "unknown" rather than "0% cached".
    #[test]
    fn a_provider_that_says_nothing_reports_none_not_zero() {
        let u = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 5
        });
        assert_eq!(cache_usage(&u), (None, None));
        // An EMPTY prompt_tokens_details is likewise nothing.
        let u2 = serde_json::json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": {}
        });
        assert_eq!(cache_usage(&u2), (None, None));
    }

    /// Reasoning tokens use the same "unknown ≠ 0" contract: a provider
    /// that splits them reports a number (possibly 0), one that folds
    /// thinking into completion reports nothing.
    #[test]
    fn reasoning_usage_reads_openai_shape_and_treats_absence_as_none() {
        let u = serde_json::json!({
            "completion_tokens": 200,
            "completion_tokens_details": { "reasoning_tokens": 150 }
        });
        assert_eq!(reasoning_usage(&u), Some(150));

        // Explicit zero is a DIFFERENT fact from absence: the provider
        // split reasoning and found none.
        let z = serde_json::json!({
            "completion_tokens": 200,
            "completion_tokens_details": { "reasoning_tokens": 0 }
        });
        assert_eq!(reasoning_usage(&z), Some(0));

        // No completion_tokens_details at all -> None (folded in).
        assert_eq!(
            reasoning_usage(&serde_json::json!({ "completion_tokens": 200 })),
            None
        );
        // …and so does an empty details object.
        assert_eq!(
            reasoning_usage(&serde_json::json!({ "completion_tokens_details": {} })),
            None
        );
    }

    /// `completion_from_wire` carries reasoning_tokens through the same
    /// way it carries cached_tokens — the driver contract gives this for
    /// free, but only if the field is wired on both halves.
    #[test]
    fn completion_from_wire_surfaces_reasoning_tokens() {
        let v = serde_json::json!({
            "choices": [{ "message": { "content": "hi", "reasoning_content": "thinking" } }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 200,
                "completion_tokens_details": { "reasoning_tokens": 150 }
            }
        });
        let c = completion_from_wire(&v, "m", "p");
        assert_eq!(c.reasoning_tokens, Some(150));
        assert_eq!(c.completion_tokens, Some(200));
        assert_eq!(c.message.reasoning.as_deref(), Some("thinking"));
    }

    /// The streaming usage chunk carries the cache breakdown all the way
    /// to the returned Completion — every interactive turn goes through
    /// this path, so it is the one that matters.
    #[tokio::test]
    async fn chat_stream_surfaces_cache_usage_from_the_final_chunk() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":27320,\"completion_tokens\":7,",
            "\"prompt_tokens_details\":{\"cached_tokens\":27000}}}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");
        let c = client
            .chat_stream(&stream_req("m"), "p", None, |_| {})
            .await
            .expect("stream");
        assert_eq!(c.prompt_tokens, Some(27320));
        assert_eq!(
            c.cached_tokens,
            Some(27000),
            "the cached share of the prompt must reach the Completion"
        );
        assert_eq!(c.cache_write_tokens, None);
    }

    // ── reasoning replay (Step 3.3) ─────────────────────────────────

    fn reasoning_req(replay: bool) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::user("question"),
                ChatMessage::assistant("answer", Some("long chain of thought".into())),
            ],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: replay,
            cache_mode: CacheMode::Auto,
            extra_body: None,
        }
    }

    fn body_of(req: &ChatRequest) -> Value {
        let home = scratch();
        let ua = resolve_user_agent(None, &home);
        ProviderClient::new("http://127.0.0.1:0/v1", "k", ua)
            .unwrap()
            .build_body(req)
    }

    /// `reasoning_content` was sent back on EVERY assistant message,
    /// unconditionally — replayed cost nobody chose, and an outright
    /// error on DeepSeek's reasoner. It is opt-in now.
    #[test]
    fn reasoning_is_not_replayed_by_default() {
        let body = body_of(&reasoning_req(false));
        let msgs = body["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 2);
        assert!(
            msgs[1].get("reasoning_content").is_none(),
            "no reasoning on the wire: {}",
            msgs[1]
        );
        // The message itself still goes out intact — this drops one
        // field, not the turn.
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "answer");
    }

    /// A provider that wants it back gets it back, byte for byte —
    /// `replay_reasoning = true` is the documented rollback.
    #[test]
    fn a_provider_that_opts_in_gets_it_back() {
        let body = body_of(&reasoning_req(true));
        let msgs = body["messages"].as_array().expect("messages");
        assert_eq!(
            msgs[1]["reasoning_content"], "long chain of thought",
            "opt-in replays it verbatim: {}",
            msgs[1]
        );
    }

    /// The store keeps the reasoning either way: this step changes what
    /// goes on the wire, never what is retained. Same message, both
    /// settings — only the wire differs.
    #[test]
    fn dropping_the_replay_does_not_touch_the_message() {
        let off = reasoning_req(false);
        let on = reasoning_req(true);
        assert_eq!(
            off.messages[1].reasoning.as_deref(),
            Some("long chain of thought"),
            "the ChatMessage still carries it"
        );
        assert_eq!(
            off.messages[1].reasoning.as_deref(),
            on.messages[1].reasoning.as_deref()
        );
    }

    // ── explicit cache breakpoints (Step 1.5) ───────────────────────

    /// system + 3 history messages + the current user turn, plus two
    /// tools — enough shape for all three breakpoints to have somewhere
    /// distinct to land.
    fn cache_req(mode: CacheMode) -> ChatRequest {
        let sys = ChatMessage {
            role: "system".into(),
            content: "you are veles".into(),
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: None,
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        };
        ChatRequest {
            model: "m".into(),
            messages: vec![
                sys,
                ChatMessage::user("older question"),
                ChatMessage::assistant("older answer", None),
                ChatMessage::user("current question"),
            ],
            tools: vec![
                json!({"type": "function", "function": {"name": "a"}}),
                json!({"type": "function", "function": {"name": "b"}}),
            ],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: mode,
            extra_body: None,
        }
    }

    /// Count every `cache_control` anywhere in the body, however nested.
    fn breakpoints(v: &Value) -> usize {
        match v {
            Value::Object(m) => m
                .iter()
                .map(|(k, val)| usize::from(k == "cache_control") + breakpoints(val))
                .sum(),
            Value::Array(a) => a.iter().map(breakpoints).sum(),
            _ => 0,
        }
    }

    /// Three breakpoints, in prefix order: the system message, the end
    /// of the tools array, and the last message of the stable history.
    /// More than three is rejected by the wire formats that want them
    /// at all, so the count is the contract.
    #[test]
    fn explicit_mode_marks_exactly_three_breakpoints() {
        let body = body_of(&cache_req(CacheMode::Explicit));
        assert_eq!(breakpoints(&body), 3, "body was: {body}");

        // 1) The system message, promoted to a parts array so the
        //    marker can sit on a content part.
        let sys = &body["messages"][0];
        assert_eq!(sys["role"], "system");
        assert_eq!(sys["content"][0]["type"], "text");
        assert_eq!(sys["content"][0]["text"], "you are veles");
        assert_eq!(sys["content"][0]["cache_control"]["type"], "ephemeral");

        // 2) The END of the tools array, not the start.
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");

        // 3) The last message BEFORE the current user turn — the
        //    rolling breakpoint. Not the current turn itself, which
        //    changes every turn and would cache nothing.
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"]["type"], "ephemeral",
            "the stable-history boundary: {body}"
        );
        assert!(
            body["messages"][3]["content"].is_string(),
            "the current user turn is NOT marked: {body}"
        );
    }

    /// THE regression that keeps a strict endpoint from 400-ing on a
    /// field it never asked for. Auto is the default, and it must put
    /// nothing cache-shaped on the wire — including leaving plain
    /// string contents as plain strings.
    #[test]
    fn auto_mode_emits_no_cache_fields() {
        let body = body_of(&cache_req(CacheMode::Auto));
        assert_eq!(breakpoints(&body), 0, "body was: {body}");
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("cache_control"),
            "not even as a nested key: {body}"
        );
        // Content shapes are untouched: no promotion to parts arrays.
        for i in 0..4 {
            assert!(
                body["messages"][i]["content"].is_string(),
                "message {i} kept its plain string content: {body}"
            );
        }
    }

    /// `off` is a veto, not a preference: it wins even where the route
    /// resolution would have said explicit.
    #[test]
    fn off_mode_emits_nothing_even_if_the_route_asks() {
        let body = body_of(&cache_req(CacheMode::Off));
        assert_eq!(breakpoints(&body), 0, "body was: {body}");
    }

    /// An unknown mode string is `auto` — the do-nothing one. A typo in
    /// config.toml must never start decorating the wire.
    #[test]
    fn an_unknown_cache_mode_parses_as_auto() {
        assert_eq!(CacheMode::parse("explicit"), CacheMode::Explicit);
        assert_eq!(CacheMode::parse(" off "), CacheMode::Off);
        assert_eq!(CacheMode::parse("expliict"), CacheMode::Auto);
        assert_eq!(CacheMode::parse(""), CacheMode::Auto);
    }

    /// A user turn that already carries an image keeps its parts and
    /// gets the marker on the LAST one — promoting or reordering parts
    /// would corrupt the message.
    #[test]
    fn a_breakpoint_on_a_parts_message_lands_on_the_last_part() {
        let mut msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]
        });
        mark_cache_breakpoint(&mut msg);
        assert!(msg["content"][0].get("cache_control").is_none());
        assert_eq!(msg["content"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(msg["content"][1]["type"], "image_url", "parts intact");
    }

    /// An empty content has nothing to cache, and several endpoints
    /// reject an empty text part — leave it exactly as it was.
    #[test]
    fn a_breakpoint_on_empty_content_is_a_no_op() {
        let mut msg = json!({ "role": "assistant", "content": "" });
        mark_cache_breakpoint(&mut msg);
        assert_eq!(msg["content"], "", "still an empty string, no parts array");
    }
    /// ONE CHUNK CARRYING BOTH FIELDS: the transition chunk every
    /// reasoning provider sends at the moment the thought ends and the
    /// answer begins.
    ///
    /// The thought's tail must reach the client BEFORE the answer's
    /// first token. Both decoders here read `content` first and
    /// `reasoning_content` second, so the client was told
    /// answer-then-thought — and every client renders strictly in
    /// event order (the desktop flushes each pending region before
    /// opening the other, and the SSE coalescer stops at the first
    /// different kind), so there is nowhere downstream to fix it.
    ///
    /// USER 2026-09-18: "thinking is followed by one token of output,
    /// then one token of thinking again (the tail of the previous
    /// thinking block) and then the rest of the regular output."
    ///
    /// It is a LIVE-only artifact — the reconcile folds one provider
    /// call's reasoning into one field and its prose into another, so
    /// a reopened transcript was always clean, which is why it read as
    /// intermittent.
    #[tokio::test]
    async fn a_chunk_carrying_both_fields_puts_the_thinking_first() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"weighing \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"it up\",\"content\":\"T\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"he answer\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = sse_server(BODY).await;
        let client = ProviderClient::new(&url, "k", UserAgentProfile::veles()).expect("client");

        let mut events = Vec::new();
        let c = client
            .chat_stream(&stream_req("m1"), "prov", None, |e| events.push(e))
            .await
            .expect("stream");

        let order: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(_) => Some("delta"),
                StreamEvent::ReasoningDelta(_) => Some("reasoning"),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            vec!["reasoning", "reasoning", "delta", "delta"],
            "the thought must finish before the answer starts, even when \
             one chunk carries the end of one and the start of the other"
        );
        // …and the fix is an ORDER change and nothing else: the
        // assembled message is byte for byte what it always was.
        assert_eq!(c.message.reasoning.as_deref(), Some("weighing it up"));
        assert_eq!(c.message.content, "The answer");
    }
}

#[cfg(test)]
#[cfg(test)]
mod key_tests {
    use super::{build_routes, routes_missing_keys};

    fn write_cfg(dir: &std::path::Path, key_env: &str) {
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[model]\nprovider = \"acme\"\ndefault = \"m1\"\n\n\
                 [[custom_providers]]\nname = \"acme\"\n\
                 base_url = \"https://example.invalid/v1\"\nkey_env = \"{key_env}\"\n"
            ),
        )
        .expect("config");
    }

    /// THE fresh-machine failure. `veles migrate` brings the config and
    /// leaves the secrets (they live in `.env`, by design), so every
    /// route is configured and none can authenticate — and doctor
    /// printed the chain and exited 0. Name the route AND the variable.
    #[test]
    fn a_route_with_no_key_anywhere_names_the_variable_to_set() {
        let (home, _g) = crate::testenv::scratch_home();
        write_cfg(home.path(), "VELES_TEST_KEY_NEVER_SET_A1");
        let cfg = crate::config::load().expect("cfg");
        let routes = build_routes(&cfg).expect("routes");
        assert_eq!(routes.len(), 1, "one route");
        assert_eq!(
            routes_missing_keys(&cfg, &routes),
            vec![(
                "acme".to_string(),
                "VELES_TEST_KEY_NEVER_SET_A1".to_string()
            )]
        );
    }

    /// And a key that IS there is not reported — a check that cries
    /// wolf on a working install teaches you to ignore it.
    #[test]
    fn a_key_present_in_dot_env_is_not_reported() {
        let (home, _g) = crate::testenv::scratch_home();
        write_cfg(home.path(), "VELES_TEST_KEY_PRESENT_B2");
        std::fs::write(
            home.path().join(".env"),
            "VELES_TEST_KEY_PRESENT_B2=sk-not-a-real-key\n",
        )
        .expect(".env");
        let cfg = crate::config::load().expect("cfg");
        let routes = build_routes(&cfg).expect("routes");
        assert!(
            routes_missing_keys(&cfg, &routes).is_empty(),
            "a resolvable key must not be reported missing"
        );
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    fn entry(api_mode: &str, driver: Option<&str>, base_url: &str) -> crate::config::ProviderEntry {
        crate::config::ProviderEntry {
            name: "under-test".into(),
            base_url: base_url.into(),
            key_env: None,
            api_mode: api_mode.into(),
            driver: driver.map(str::to_string),
            model: None,
            models: Default::default(),
            context_length: None,
            reasoning_effort: None,
            discover: None,
            extra_body: None,
            cache_mode: None,
            replay_reasoning: None,
            built_in_tools: vec![],
            endpoint_path: None,
            auth_style: None,
            api_version: None,
            max_output_tokens: None,
        }
    }

    /// `api_mode = "plugin"` with no `driver` is a config line that can
    /// never do anything, so it is refused where every other unusable
    /// provider line is refused: at route build, not deep in a turn.
    #[test]
    fn a_plugin_route_must_name_a_driver() {
        let ua = UserAgentProfile::veles();
        let err = build_client(&entry("plugin", None, ""), "k", &ua)
            .expect_err("no driver named")
            .to_string();
        assert!(err.contains("needs `driver"), "{err}");
        assert!(
            err.contains("doctor --providers"),
            "the error must say where to look: {err}"
        );
        // An empty string is the same as absent, not a driver named "".
        assert!(build_client(&entry("plugin", Some("   "), ""), "k", &ua).is_err());
    }

    /// A plugin route's `base_url` is opaque — the DRIVER owns the
    /// transport, and one talking to a socket or a subprocess has no
    /// http(s) URL. The check that rejects "definitely not a url" for
    /// every native wire must not fire here.
    #[test]
    fn a_plugin_route_needs_no_url() {
        let ua = UserAgentProfile::veles();
        let c = build_client(&entry("plugin", Some("some-wire"), ""), "k", &ua)
            .expect("an empty base_url is fine for a driver");
        match c {
            RouteClient::Plugin(p) => {
                assert_eq!(p.driver, "some-wire");
                assert_eq!(p.api_key, "k");
            }
            other => panic!("expected a plugin route, got {other:?}"),
        }
        // The same string on a native wire is still refused.
        assert!(build_client(&entry("chat_completions", None, ""), "k", &ua).is_err());
    }

    /// Resolution happens per CALL, not at build: drivers register when
    /// plugins load, which is after routes are built. A miss must name
    /// what IS registered — otherwise the only way to find out is to
    /// read the plugin source.
    #[tokio::test]
    async fn an_unregistered_driver_names_the_ones_that_are() {
        register_driver(ProviderDriver {
            name: "present-wire".into(),
            plugin: "test".into(),
            call: Arc::new(|_body, _emit| Ok(serde_json::json!({}))),
        })
        .expect("register");

        let route = PluginRoute {
            driver: "absent-wire".into(),
            base_url: String::new(),
            api_key: String::new(),
            ..Default::default()
        };
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Off,
            extra_body: None,
        };
        let err = RouteClient::Plugin(route)
            .chat(&req, "p")
            .await
            .expect_err("no such driver")
            .to_string();
        assert!(err.contains("absent-wire"), "{err}");
        assert!(
            err.contains("present-wire") && err.contains("test"),
            "the error must list what IS registered, and where each came from: {err}"
        );
    }

    /// Replace-by-name, like every other plugin registry here: a reload
    /// must not leave two copies of one wire with the first one winning
    /// forever.
    #[test]
    fn registering_a_driver_twice_replaces_it() {
        for marker in ["first", "second"] {
            let m = marker.to_string();
            register_driver(ProviderDriver {
                name: "replaceable".into(),
                plugin: m.clone(),
                call: Arc::new(move |_b, _e| Ok(serde_json::json!({ "marker": m }))),
            })
            .expect("register");
        }
        let found: Vec<_> = drivers()
            .into_iter()
            .filter(|d| d.name == "replaceable")
            .collect();
        assert_eq!(found.len(), 1, "a reload must replace, not stack");
        assert_eq!(found[0].plugin, "second");
        assert!(register_driver(ProviderDriver {
            name: "  ".into(),
            plugin: "x".into(),
            call: Arc::new(|_b, _e| Ok(serde_json::json!({}))),
        })
        .is_err());
    }

    /// The driver contract in one test: a driver answers an
    /// OpenAI-shaped RESPONSE and the engine parses it with the same
    /// code a native route uses — so tool calls, the reasoning/content
    /// split and every spelling of cache accounting arrive for free.
    /// That is what makes a driver a translation rather than a rewrite
    /// of providers.rs.
    #[tokio::test]
    async fn a_driver_answers_a_wire_shape_and_gets_the_native_parse() {
        register_driver(ProviderDriver {
            name: "parse-me".into(),
            plugin: "test".into(),
            call: Arc::new(|body, _emit| {
                // The driver SEES the assembled request, which is the
                // other half of the bargain.
                assert_eq!(body["model"], "the-model");
                assert_eq!(body["messages"][0]["content"], "hi");
                Ok(serde_json::json!({
                    "choices": [{ "message": {
                        "role": "assistant",
                        "content": "translated",
                        "reasoning_content": "thought about it",
                        "tool_calls": [{
                            "id": "call_1", "type": "function",
                            "function": { "name": "search", "arguments": "{\"q\":1}" }
                        }]
                    }}],
                    "usage": {
                        "prompt_tokens": 11,
                        "completion_tokens": 22,
                        "prompt_tokens_details": { "cached_tokens": 7 },
                        "cache_creation_input_tokens": 3
                    }
                }))
            }),
        })
        .expect("register");

        let req = ChatRequest {
            model: "the-model".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Off,
            extra_body: None,
        };
        let c = RouteClient::Plugin(PluginRoute {
            driver: "parse-me".into(),
            base_url: String::new(),
            api_key: String::new(),
            ..Default::default()
        })
        .chat(&req, "plugged")
        .await
        .expect("the driver answered");

        assert_eq!(c.message.content, "translated");
        assert_eq!(c.message.reasoning.as_deref(), Some("thought about it"));
        assert_eq!(
            c.message.tool_calls.as_ref().unwrap()[0]["function"]["name"],
            "search"
        );
        assert_eq!(c.provider, "plugged");
        assert_eq!(c.model, "the-model");
        assert_eq!(c.prompt_tokens, Some(11));
        assert_eq!(c.completion_tokens, Some(22));
        assert_eq!(c.cached_tokens, Some(7), "cache accounting comes for free");
        assert_eq!(c.cache_write_tokens, Some(3));
    }

    /// A driver reporting failure produces the message, not a silent
    /// empty assistant turn — the same treatment an endpoint's 4xx gets.
    #[tokio::test]
    async fn a_driver_error_object_becomes_a_provider_error() {
        register_driver(ProviderDriver {
            name: "sad-wire".into(),
            plugin: "test".into(),
            call: Arc::new(|_b, _e| {
                Ok(serde_json::json!({ "error": "upstream said no", "status": 418 }))
            }),
        })
        .expect("register");
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Off,
            extra_body: None,
        };
        let err = RouteClient::Plugin(PluginRoute {
            driver: "sad-wire".into(),
            base_url: String::new(),
            api_key: String::new(),
            ..Default::default()
        })
        .chat(&req, "p")
        .await
        .expect_err("an error object is an error");
        assert!(err.to_string().contains("upstream said no"), "{err}");
        assert!(
            err.to_string().contains("418"),
            "the status must survive: {err}"
        );
    }

    /// The status a driver reports decides the ERROR CLASS, which decides
    /// whether the route fails over. 429 → RateLimited, 5xx → Server, both
    /// of which `should_fallback` answers true for; everything else stays a
    /// client error the caller surfaces.
    ///
    /// This used to be wrong: every driver error became `Client` regardless
    /// of the status, so an Anthropic-compatible gateway answering 429 took
    /// the plugin route out of the failover chain entirely — the turn failed
    /// instead of moving to the next route, which is the one thing the chain
    /// exists to do. The status is now read exactly as a real HTTP status is.
    #[tokio::test]
    async fn a_driver_reported_status_decides_whether_the_route_fails_over() {
        register_driver(ProviderDriver {
            name: "statusful-wire".into(),
            plugin: "test".into(),
            // Whatever status the request body asks for, echoed back as the
            // driver's error object. One driver, so the test is about the
            // mapping and not about two drivers disagreeing.
            call: Arc::new(|body, _e| {
                let status = body["__test_status"].as_u64().unwrap_or(500);
                Ok(serde_json::json!({ "error": "upstream unhappy", "status": status }))
            }),
        })
        .expect("register");

        let req_with = |status: u64| ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: CacheMode::Off,
            // extra_body reaches the driver verbatim, which is how the test
            // tells the driver which status to answer with.
            extra_body: Some(serde_json::json!({ "__test_status": status })),
        };

        let route = RouteClient::Plugin(PluginRoute {
            driver: "statusful-wire".into(),
            ..Default::default()
        });

        // 429 is a rate limit, and rate limits fail over.
        let err = route
            .chat(&req_with(429), "p")
            .await
            .expect_err("429 is an error");
        assert!(
            matches!(err, ProviderError::RateLimited(_)),
            "429 must be RateLimited so the chain moves on, got {err:?}"
        );
        assert!(err.should_fallback(), "a 429 fails over: {err}");

        // 5xx is a server error, and server errors fail over.
        let err = route
            .chat(&req_with(503), "p")
            .await
            .expect_err("503 is an error");
        assert!(
            matches!(err, ProviderError::Server { status: 503, .. }),
            "503 must be Server, got {err:?}"
        );
        assert!(err.should_fallback(), "a 5xx fails over: {err}");

        // 400 is the caller's fault: surfaced, and NOT retried elsewhere,
        // because the next route would be sent the same bad request.
        let err = route
            .chat(&req_with(400), "p")
            .await
            .expect_err("400 is an error");
        assert!(
            matches!(err, ProviderError::Client { status: 400, .. }),
            "400 must stay a Client error, got {err:?}"
        );
        assert!(
            !err.should_fallback(),
            "a 400 must not fail over — the request itself is wrong: {err}"
        );
    }

    /// Streaming: chunks a driver emits reach the caller as deltas
    /// WHILE it works, and the final answer is still the response
    /// object it returns — the engine does not assemble the message out
    /// of the chunks, unlike a native SSE stream, because it does not
    /// have to.
    #[tokio::test]
    async fn a_driver_streams_through_provider_emit() {
        register_driver(ProviderDriver {
            name: "streamy".into(),
            plugin: "test".into(),
            call: Arc::new(|_body, _emit| {
                for part in ["one ", "two ", "three"] {
                    let ok = emit_chunk(&serde_json::json!({
                        "choices": [{ "delta": { "content": part } }]
                    }));
                    assert!(ok, "emit must find the in-flight call");
                }
                emit_chunk(&serde_json::json!({
                    "choices": [{ "delta": { "reasoning_content": "hmm" } }]
                }));
                Ok(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": "one two three" }}]
                }))
            }),
        })
        .expect("register");

        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            reasoning_effort: None,
            stream: true,
            replay_reasoning: false,
            cache_mode: CacheMode::Off,
            extra_body: None,
        };
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&seen);
        let c = RouteClient::Plugin(PluginRoute {
            driver: "streamy".into(),
            base_url: String::new(),
            api_key: String::new(),
            ..Default::default()
        })
        .chat_stream(&req, "p", None, move |ev| {
            let mut v = sink.lock().unwrap();
            match ev {
                StreamEvent::Delta(t) => v.push(format!("delta:{t}")),
                StreamEvent::ReasoningDelta(t) => v.push(format!("reasoning:{t}")),
                StreamEvent::Route {
                    provider,
                    model,
                    reasoning_effort: _,
                } => v.push(format!("route:{provider}/{model}")),
                StreamEvent::Done(_) => v.push("done".into()),
                _ => {}
            }
        })
        .await
        .expect("stream");

        let events = seen.lock().unwrap().clone();
        assert_eq!(events.first().map(String::as_str), Some("route:p/m"));
        assert!(events.contains(&"delta:one ".to_string()), "{events:?}");
        assert!(events.contains(&"delta:three".to_string()), "{events:?}");
        assert!(events.contains(&"reasoning:hmm".to_string()), "{events:?}");
        assert_eq!(events.last().map(String::as_str), Some("done"));
        assert_eq!(c.message.content, "one two three");
    }

    /// `provider_emit` outside a driver call is an error, not a no-op:
    /// a plugin emitting into nothing has misread the contract.
    #[test]
    fn emitting_outside_a_driver_call_is_refused() {
        assert!(
            !emit_chunk(&serde_json::json!({"choices":[]})),
            "there is no call in flight, so there is nowhere to emit"
        );
    }

    /// The plugin-driver path decodes the same wire shape with the same
    /// two arms in the same wrong order, so it had the same defect and
    /// gets the same pin.
    #[test]
    fn a_driver_chunk_carrying_both_fields_puts_the_thinking_first() {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "content": "T", "reasoning_content": "it up" } }]
        });
        let mut seen: Vec<String> = Vec::new();
        emit_wire_chunk(&chunk, &mut |e| {
            seen.push(match e {
                StreamEvent::Delta(t) => format!("delta:{t}"),
                StreamEvent::ReasoningDelta(t) => format!("reasoning:{t}"),
                _ => "other".into(),
            })
        });
        assert_eq!(seen, vec!["reasoning:it up", "delta:T"]);
    }
}
