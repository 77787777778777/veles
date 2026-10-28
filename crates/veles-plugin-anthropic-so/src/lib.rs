//! veles-plugin-anthropic-so — the SAME driver as
//! `plugins/examples/tcl-anthropic-driver`, out-of-tree and in Rust.
//!
//! Two implementations of one wire, in two languages, is the point. The
//! question they answer together is whether a complete driver with
//! custom low-level logic can be written in either — not a hook that
//! observes a turn, but a wire the engine did not previously speak, end
//! to end.
//!
//! `providers.rs`'s `driver_parity` test feeds one canned Anthropic
//! response to both and asserts the two `Completion`s are EQUAL. One
//! expected value, two implementations: asserting each separately would
//! prove they both run, not that they agree.
//!
//! # What it links
//!
//! `veles-plugin-abi` and `serde_json`. Not veles-core — an out-of-tree
//! plugin that linked the engine would be an in-tree plugin with extra
//! steps, sharing a crate graph, an allocator and a version lock.
//! Everything it does to the engine goes through `host_call`, which is
//! the capability ABI 1 did not have at all.
//!
//! # What it does NOT contain
//!
//! No HTTP client, no retry policy, no timeout, no streaming parser of its
//! own, no token accounting. The transport is the engine's
//! (`host_call("http_post", …)` and the `http_stream_*` trio) and the
//! response parse is the engine's (answer an OpenAI-shaped object and
//! `completion_from_wire` does the rest). A driver TRANSLATES.
//!
//! # The Anthropic Messages wire, and where it differs
//!
//!   * system prompt: a top-level field, not a `role: "system"` message
//!   * `max_tokens`: required, where OpenAI does not, and the engine never
//!     sets one
//!   * content: an array of BLOCKS (text / thinking / tool_use /
//!     tool_result / image), not a string
//!   * a tool call is a `tool_use` block inside the assistant's content
//!     with the arguments as a JSON OBJECT — OpenAI puts them in a
//!     sibling `tool_calls` array as a JSON STRING
//!   * a tool RESULT is a `tool_result` block in a USER message, not a
//!     message with `role: "tool"`
//!   * auth is `x-api-key` + `anthropic-version`, not bearer
//!   * usage is input_tokens/output_tokens, with cache accounting under
//!     cache_creation_input_tokens / cache_read_input_tokens
//!   * an UNKNOWN field is a hard 400 ("Extra inputs are not permitted"),
//!     so sending something a model does not support fails the turn
//!
//! That last point is why this driver ASKS the endpoint what it supports
//! instead of carrying a table of model families. The Tcl twin's header
//! comment records the measurement: `claude-opus-4-5` accepts
//! `output_config.effort` while its same-numbered siblings reject it, so no
//! generation rule works, and `/v1/models` reports every capability that
//! matters.
//!
//! # Thinking is never replayed
//!
//! A `thinking` block carries a `signature` the endpoint requires on the way
//! back in; sending the block without it is a 400. The engine stores
//! reasoning as a plain string with nowhere to keep a signature, so the only
//! safe translation is to drop thinking on replay — which the endpoint
//! accepts. Reasoning is still surfaced and still persisted.
//!
//! # Panics
//!
//! There are none, deliberately: no `unwrap`, no `expect`, no indexing.
//! Every failure here becomes an `{"error":…}` object, which the engine
//! surfaces to the user as a provider error. (On ABI 4 the SDK would
//! catch a panic at the boundary anyway and answer the same shape; the
//! driver still does not rely on it.)

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use veles_plugin_abi_sdk::{veles_plugin, Host};

/// Anthropic dates its wire changes to this header and requires it.
/// Pinned rather than tracking "latest", so an upstream change is a
/// decision here and not a surprise in production. A gateway that needs a
/// different value sets `api_version` on the provider entry.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// API version 2026-10-21 REMOVED the sampling parameters: a request under
/// that version sending `temperature`, `top_p` or `top_k` is a hard 400 on
/// EVERY model — the ban is per-version, not per-generation, and the
/// capability catalog cannot answer it, because it reports what a model
/// supports, not what a version permits. (Measured live 2026-10-22:
/// haiku-4-5 + temperature is 200 under 2023-06-01 and 400 under
/// 2026-10-21.) So the sampling gate has TWO questions, and the version's
/// answer overrides the model's: what the catalog says about the model, and
/// what the negotiated version permits at all.
const SAMPLING_REMOVED_VERSION: &str = "2026-10-21";

/// Anthropic REQUIRES max_tokens; OpenAI does not, and the engine never
/// sets one. A default has to come from somewhere, so it is stated here
/// rather than hidden in a shrug. Used only when neither the provider entry
/// (`max_output_tokens`) nor capability discovery can say. Deliberately
/// modest: too high is a hard 400 on every model below it, while too low
/// costs output length on one turn.
const FALLBACK_MAX_TOKENS: u64 = 8192;

/// The effort names this wire accepts. veles' ladder is a superset;
/// anything outside this set is dropped rather than sent, because an
/// unknown effort is a hard 400 and losing a turn over a spelling is a bad
/// trade.
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Server-side tools, by the name an operator would write in
/// `built_in_tools` → the tool object this wire wants. Anthropic versions
/// its server tools in the type name, and a type it does not know is a hard
/// 400, so an unrecognized name is dropped rather than guessed at.
fn server_tool_for(name: &str) -> Option<Value> {
    match name {
        "web_search" | "web_extractor" => {
            Some(json!({ "type": "web_search_20250305", "name": "web_search" }))
        }
        "code_execution" => Some(json!({
            "type": "code_execution_20250825",
            "name": "code_execution"
        })),
        _ => None,
    }
}

/// The host API. A `'static` on the host side, so keeping the pointer
/// is sound — see `load_so`, which learned that the hard way.
static HOST: OnceLock<Host> = OnceLock::new();

/// model capabilities, keyed per endpoint. The plugin interpreter is
/// long-lived and so is this, so the catalog is read once per
/// (endpoint, model) rather than once per turn: it is ~14 models of nested
/// JSON, and asking for it every turn would make it the slowest part of
/// the turn.
///
/// A `Mutex` and not a `OnceLock` map because the driver runs on a blocking
/// thread that may be entered concurrently; the lock is held only long
/// enough to read or write one entry, never across a network call.
fn caps_cache() -> &'static Mutex<HashMap<String, Caps>> {
    static C: OnceLock<Mutex<HashMap<String, Caps>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What the catalog said about one model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Caps {
    /// Discovery answered for this model at all. When false, nothing
    /// capability-dependent is sent, because on a strict endpoint a guess
    /// is a failed turn.
    known: bool,
    effort: bool,
    adaptive: bool,
    legacy: bool,
    sampling: bool,
    max_out: u64,
}

/// The capability set used when discovery is not worth a round trip:
/// nothing capability-dependent is sent. `sampling: true` is the one
/// permissive field, because a sampling parameter this driver forwards can
/// only have come from an operator's own extra_body — if they set one for a
/// model that rejects it, that is their request and the endpoint's answer,
/// not a guess of ours.
fn unknown_caps() -> Caps {
    Caps {
        known: false,
        effort: false,
        adaptive: false,
        legacy: false,
        sampling: true,
        max_out: 0,
    }
}

/// Call an engine op through the host (`host.call` decodes the
/// `{"ok":…}` / `{"error":…}` envelope).
fn engine(op: &str, args: &[&str]) -> Result<String, String> {
    let host = HOST
        .get()
        .ok_or_else(|| "plugin not initialised".to_string())?;
    host.call(op, args)
}

// ── capabilities ────────────────────────────────────────────────────────

fn caps(base: &str, key: &str, model: &str) -> Caps {
    // The separator is U+241F, a symbol that cannot appear in a URL or a
    // model id, so two different (endpoint, model) pairs cannot collide into
    // one cache entry. A cache that conflated two gateways would serve one
    // gateway's capability answers to the other.
    let ck = format!("{base}\u{241f}{model}");
    if let Ok(cache) = caps_cache().lock() {
        if let Some(c) = cache.get(&ck) {
            return *c;
        }
    }
    // Discovered OUTSIDE the lock: a network call under a mutex would
    // serialize every concurrent turn through one catalog read.
    let c = discover(base, key, model);
    if let Ok(mut cache) = caps_cache().lock() {
        cache.insert(ck, c);
    }
    c
}

/// Read one model's capabilities out of `/v1/models`.
///
/// Failures are not errors: a gateway that does not implement the endpoint,
/// a network blip, or a model the catalog has not caught up to all yield
/// `known: false`, and the driver degrades to the conservative shape
/// instead of failing the turn. What must never happen is a driver that
/// cannot talk to its endpoint because it could not read a catalog.
fn discover(base: &str, key: &str, model: &str) -> Caps {
    let url = format!("{}/v1/models", trim_base(base));
    let headers = headers_for_key(key, "", ANTHROPIC_VERSION);
    let raw = match engine("http_get", &[&url, &headers]) {
        Ok(r) => r,
        Err(_) => return unknown_caps(),
    };
    let catalog: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return unknown_caps(),
    };
    let Some(entry) = catalog["data"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["id"].as_str() == Some(model)))
    else {
        // The catalog answered but did not list this model. Same posture as
        // a catalog that did not answer: send nothing capability-dependent.
        return unknown_caps();
    };

    let supported = |path: &[&str]| -> bool {
        let mut node = &entry["capabilities"];
        for k in path {
            node = &node[k];
        }
        node.as_bool().unwrap_or(false)
    };
    let effort = supported(&["effort", "supported"]);
    let adaptive = supported(&["thinking", "types", "adaptive", "supported"]);
    let legacy = supported(&["thinking", "types", "enabled", "supported"]);
    let max_out = entry["max_tokens"].as_u64().unwrap_or(0);

    // Sampling is the one thing the catalog does not report directly, and it
    // is the one that changed behaviour mid-line: temperature is accepted
    // through 4.6 and a hard 400 from 4.7 on ("deprecated for this model").
    // `thinking.types.enabled.supported` is the field that tracks it, and it
    // does so across all fourteen models this key can reach — verified
    // 2026-10-16 by asking each model for `temperature` and comparing
    // against the catalog:
    //
    //   enabled=true   opus-4-5, sonnet-4-5, haiku-4-5, opus-4-6,
    //                  sonnet-4-6            → temperature ACCEPTED
    //   enabled=false  opus-4-7, opus-4-8, and every 5-series
    //                                        → temperature REJECTED
    //
    // That the legacy thinking shape and the sampling parameters were
    // retired together is why one field predicts the other. It is still an
    // inference, so it is stated rather than buried: if a future line breaks
    // it, this is the line to change. (Note that `capabilities.effort.
    // supported` does NOT work as the predictor — opus-4-5 supports effort
    // and still accepts temperature, while its sibling sonnet-4-5 rejects
    // effort and accepts temperature.)
    let sampling = legacy;

    Caps {
        known: true,
        effort,
        adaptive,
        legacy,
        sampling,
        max_out,
    }
}

fn trim_base(base: &str) -> &str {
    base.trim_end_matches('/')
}

/// Does the negotiated wire version still accept sampling parameters?
///
/// Only a real Anthropic version DATE can trigger the ban: a gateway may
/// put any string in the version header (`api_version` is an operator
/// knob), and the 2026-10-21 rule is Anthropic's, not the gateway's —
/// dropping an operator's temperature against a proxy that would have
/// accepted it is a worse guess than forwarding it. A date at or past
/// `SAMPLING_REMOVED_VERSION` gets the ban; anything else the permissive
/// answer. ISO dates sort correctly as strings, which is why the compare
/// is a plain `>=`.
fn version_allows_sampling(version: &str) -> bool {
    let is_date = version.len() == 10
        && version.as_bytes().iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    !(is_date && version >= SAMPLING_REMOVED_VERSION)
}

// ── endpoint knobs ──────────────────────────────────────────────────────
//
// All of these come from the reserved `veles` key the engine adds to every
// driver request, which is what lets ONE driver serve several provider
// entries with different credentials and different gateway shapes. Each is
// optional and each falls back to this driver's own default when empty.

fn veles_str(req: &Value, key: &str) -> String {
    req["veles"][key].as_str().unwrap_or("").to_string()
}

fn endpoint_path(req: &Value) -> String {
    let p = veles_str(req, "endpoint_path");
    if p.is_empty() {
        return "/v1/messages".to_string();
    }
    if p.starts_with('/') {
        p
    } else {
        format!("/{p}")
    }
}

fn api_version_for(req: &Value) -> String {
    let v = veles_str(req, "api_version");
    if v.is_empty() {
        ANTHROPIC_VERSION.to_string()
    } else {
        v
    }
}

/// The headers for a route, with its configured auth style and version.
///
/// `x-api-key` is Anthropic's own; `bearer` is what the gateways that resell
/// it usually want, because that is what their auth stack speaks. An
/// unrecognized style falls back to x-api-key — config load already refuses
/// anything outside the two (`validate_auth_styles`), so this arm is a
/// defence against a caller that bypassed validation, not a silent default.
fn headers_for(req: &Value, key: &str) -> String {
    headers_for_key(key, &veles_str(req, "auth_style"), &api_version_for(req))
}

fn headers_for_key(key: &str, style: &str, version: &str) -> String {
    if style == "bearer" {
        json!({
            "authorization": format!("Bearer {key}"),
            "anthropic-version": version,
        })
        .to_string()
    } else {
        json!({
            "x-api-key": key,
            "anthropic-version": version,
        })
        .to_string()
    }
}

/// What the operator pinned as an output cap on the provider entry, or 0 when
/// they did not. Split out because `drive` needs the ANSWER to "was one
/// pinned?" before it decides whether the catalog read is worth making, and
/// `max_out` needs the value itself — two questions about one field, which is
/// where a duplicated parse would drift.
fn configured_max_out(req: &Value) -> u64 {
    req["veles"]["max_output_tokens"].as_u64().unwrap_or(0)
}

/// The output cap to send. Provider config wins, then what the catalog said,
/// then this driver's default. A configured value is NOT clamped to the
/// catalog's: an operator who sets one has a reason, and a gateway's catalog
/// may lag the models it serves.
fn max_out(req: &Value, c: Caps) -> u64 {
    let configured = configured_max_out(req);
    if configured > 0 {
        return configured;
    }
    if c.max_out > 0 {
        return c.max_out;
    }
    FALLBACK_MAX_TOKENS
}

// ── request: OpenAI shape → Anthropic Messages shape ────────────────────

fn to_wire(req: &Value, c: Caps, streaming: bool) -> Value {
    let mut system = String::new();
    let mut system_cache: Option<Value> = None;
    let mut messages: Vec<Value> = Vec::new();

    for m in req["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("");

        if role == "system" {
            // Hoisted out of `messages`. CONCATENATED when there is more
            // than one: keeping only the first would silently lose
            // instructions.
            //
            // The engine may have marked a cache breakpoint here, which
            // turns `content` into a parts array. Both shapes are handled
            // and the breakpoint is preserved — Anthropic's system field
            // takes the same block array, so a cache_control the engine
            // computed survives the translation instead of being dropped.
            let mut text = String::new();
            match &m["content"] {
                Value::Array(parts) => {
                    for p in parts {
                        if p["type"].as_str() == Some("text") {
                            text.push_str(p["text"].as_str().unwrap_or(""));
                        }
                        if !p["cache_control"].is_null() {
                            system_cache = Some(p["cache_control"].clone());
                        }
                    }
                }
                other => text.push_str(other.as_str().unwrap_or("")),
            }
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&text);
            continue;
        }

        if role == "tool" {
            // A tool result is a block in a USER message. Consecutive
            // results are MERGED into one user message, which is the
            // canonical shape when the model called several tools in one
            // turn; separate user messages also work, but one message is
            // what the wire means by "here are your results".
            let block = json!({
                "type": "tool_result",
                "tool_use_id": m["tool_call_id"].as_str().unwrap_or(""),
                "content": m["content"].as_str().unwrap_or(""),
            });
            if let Some(last) = messages.last_mut() {
                if is_tool_result_msg(last) {
                    if let Some(arr) = last["content"].as_array_mut() {
                        arr.push(block);
                        continue;
                    }
                }
            }
            messages.push(json!({ "role": "user", "content": [block] }));
            continue;
        }

        // An assistant turn that CALLED tools carries them as blocks beside
        // its text, not in a separate field.
        let mut blocks: Vec<Value> = Vec::new();
        match &m["content"] {
            // A user message with attachments arrives as OpenAI parts: text
            // and image_url. Translated to this wire's blocks, which is the
            // difference between an image reaching the model and an image
            // silently becoming an empty string.
            Value::Array(parts) => {
                for p in parts {
                    match p["type"].as_str().unwrap_or("") {
                        "text" => {
                            let t = p["text"].as_str().unwrap_or("");
                            if t.is_empty() {
                                continue;
                            }
                            blocks.push(text_block(t, p.get("cache_control")));
                        }
                        "image_url" => {
                            if let Some(b) = image_block(
                                p["image_url"]["url"].as_str().unwrap_or(""),
                                p.get("cache_control"),
                            ) {
                                blocks.push(b);
                            }
                        }
                        _ => {}
                    }
                }
            }
            other => {
                let t = other.as_str().unwrap_or("");
                if !t.is_empty() {
                    blocks.push(text_block(t, None));
                }
            }
        }
        for call in m["tool_calls"].as_array().into_iter().flatten() {
            // OpenAI carries arguments as a JSON *string*; this wire wants
            // the object itself.
            let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
            let input: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": call["id"].as_str().unwrap_or(""),
                "name": call["function"]["name"].as_str().unwrap_or(""),
                "input": input,
            }));
        }

        // NOTE what is NOT here: `reasoning_content`. The engine replays it
        // when the provider asks, and this driver drops it, because a
        // thinking block without its `signature` is a hard 400 and there is
        // nowhere in the engine's message model to keep one. See the module
        // comment. Dropping is accepted by the endpoint; sending is not.

        if blocks.is_empty() {
            continue;
        }
        messages.push(json!({ "role": role, "content": blocks }));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req["model"].as_str().unwrap_or("")));
    body.insert("max_tokens".into(), json!(max_out(req, c)));
    body.insert("messages".into(), Value::Array(messages));

    if !system.is_empty() {
        body.insert(
            "system".into(),
            match system_cache {
                // The engine asked for an explicit cache breakpoint. system
                // takes the block-array form so the breakpoint has somewhere
                // to live; the bare string form has no such slot.
                Some(cc) => json!([{
                    "type": "text",
                    "text": system,
                    "cache_control": cc,
                }]),
                None => json!(system),
            },
        );
    }

    // ── thinking / effort, per what the model actually supports ─────────
    //
    // The engine's `reasoning_effort` is the only signal, and it maps to two
    // different fields on this wire depending on the model. Sending either
    // one to a model that does not support it is a 400, so when capabilities
    // are unknown BOTH are withheld. That is the conservative posture
    // `discover` states: a gateway that will not say what it supports gets
    // no guesses.
    let effort = req["reasoning_effort"].as_str().unwrap_or("");
    if !effort.is_empty() && c.known {
        let want_thinking = effort != "off" && effort != "none";
        if c.adaptive {
            if want_thinking {
                body.insert("thinking".into(), json!({ "type": "adaptive" }));
            }
        } else if c.legacy {
            // The legacy shape wants a token budget and requires
            // max_tokens > budget_tokens. Half the cap is the split: enough
            // room to think and enough left to answer, and it cannot violate
            // the ordering the endpoint enforces. 1024 is the floor the
            // endpoint itself states.
            let budget = (max_out(req, c) / 2).max(1024);
            if want_thinking {
                body.insert(
                    "thinking".into(),
                    json!({ "type": "enabled", "budget_tokens": budget }),
                );
            }
        }
        if c.effort && EFFORTS.contains(&effort) {
            body.insert("output_config".into(), json!({ "effort": effort }));
        }
    }

    // ── sampling, only where it is still accepted ───────────────────────
    //
    // temperature/top_p/top_k are DEPRECATED on the current lines and a hard
    // 400 there, so they are forwarded only when the model is known to take
    // them. They arrive via extra_body, which the engine merges verbatim
    // onto the request body; this driver lifts them into the wire body.
    //
    // TWO gates, because the model check alone was version-blind:
    // 2026-10-21 removed the parameters per-VERSION, so a model that still
    // takes temperature under 2023-06-01 (and the catalog says so — the
    // capability is real, the version just no longer permits sending it)
    // gets a hard 400 under the new wire. The version gate overrides BOTH
    // the catalog's answer and `unknown_caps`'s permissive default, which
    // is exactly the combination that would otherwise forward a temperature
    // into a guaranteed 400.
    if c.sampling && version_allows_sampling(&api_version_for(req)) {
        for f in ["temperature", "top_p", "top_k"] {
            if let Some(v) = req[f].as_f64() {
                body.insert(f.into(), json!(v));
            }
        }
    }

    // stop_sequences: the engine has no field for these, so they ride
    // extra_body under either spelling. Both are accepted because OpenAI
    // says `stop` and Anthropic says `stop_sequences`, and an operator
    // writing extra_body for this wire should not have to remember which.
    let stop = if !req["stop_sequences"].is_null() {
        &req["stop_sequences"]
    } else {
        &req["stop"]
    };
    if stop.is_array() {
        body.insert("stop_sequences".into(), stop.clone());
    } else if let Some(s) = stop.as_str() {
        body.insert("stop_sequences".into(), json!([s]));
    }

    // tool_choice, translated from OpenAI's spellings. The engine does not
    // set it, so this only fires for an operator who put it in extra_body —
    // but "required" and the named-function form are real OpenAI shapes that
    // do not exist here, and forwarding one verbatim is a 400.
    if !req["tool_choice"].is_null() {
        if let Some(tc) = tool_choice(&req["tool_choice"]) {
            body.insert("tool_choice".into(), tc);
        }
    }

    // Tools: OpenAI nests the schema under function.parameters; this wire
    // puts name/description/input_schema at the top level. Server tools come
    // after, in their own object shape.
    let mut tools: Vec<Value> = req["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let f = &t["function"];
            let mut o = json!({
                "name": f["name"].as_str().unwrap_or(""),
                "description": f["description"].as_str().unwrap_or(""),
                "input_schema": if f["parameters"].is_null() {
                    json!({ "type": "object" })
                } else {
                    f["parameters"].clone()
                },
            });
            // A cache breakpoint the engine put on the last tool survives.
            if !t["cache_control"].is_null() {
                o["cache_control"] = t["cache_control"].clone();
            }
            o
        })
        .collect();
    for name in req["veles"]["built_in_tools"]
        .as_array()
        .into_iter()
        .flatten()
    {
        // Unknown names are dropped: sending a server-tool type the endpoint
        // has never heard of is a 400, and one unrecognized name must not
        // cost the turn.
        if let Some(s) = name.as_str().and_then(server_tool_for) {
            tools.push(s);
        }
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }

    // Anything else an operator put in extra_body that this wire understands
    // verbatim. Passthrough is last so an explicit knob cannot be overwritten
    // by an inferred one; the transport keys the engine owns
    // (messages/model/stream/tools) never reach here, because its own
    // denylist drops them before the driver sees the body.
    for f in ["metadata", "service_tier", "container"] {
        if !req[f].is_null() {
            body.insert(f.into(), req[f].clone());
        }
    }

    if streaming {
        body.insert("stream".into(), json!(true));
    }
    Value::Object(body)
}

/// OpenAI's tool_choice spellings → this wire's. `None` means "do not
/// send", so a shape this wire cannot express is dropped rather than
/// forwarded into a 400.
fn tool_choice(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "none" => Some(json!({ "type": "none" })),
            "required" => Some(json!({ "type": "any" })),
            _ => None,
        },
        Value::Object(_) => match tc["type"].as_str().unwrap_or("") {
            "function" => tc["function"]["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(|n| json!({ "type": "tool", "name": n })),
            // Already an Anthropic-shaped object, from an operator who knew
            // the wire.
            "auto" | "any" | "tool" | "none" => Some(tc.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn text_block(text: &str, cache_control: Option<&Value>) -> Value {
    let mut o = json!({ "type": "text", "text": text });
    // A cache breakpoint the engine marked on this part, preserved.
    if let Some(cc) = cache_control.filter(|c| !c.is_null()) {
        o["cache_control"] = cc.clone();
    }
    o
}

/// An OpenAI `image_url` part → this wire's `image` block.
///
/// The engine hands over a data URL (`data:<mime>;base64,…`), which is the
/// shape OpenAI's `image_url` wants and NOT the shape this wire wants: here
/// the media type and the payload are separate fields under `source`. A
/// plain http(s) URL becomes a `url` source. Anything else — an unrecognized
/// scheme, a malformed data URL — yields `None`, so the rest of the message
/// still goes out. Losing one image with the turn intact beats a 400 that
/// loses the turn.
fn image_block(url: &str, cache_control: Option<&Value>) -> Option<Value> {
    if url.is_empty() {
        return None;
    }
    let source = if let Some(rest) = url.strip_prefix("data:") {
        let semi = rest.find(';')?;
        let comma = rest.find(',')?;
        if comma < semi {
            return None;
        }
        let mime = &rest[..semi];
        let b64 = &rest[comma + 1..];
        if mime.is_empty() || b64.is_empty() {
            return None;
        }
        json!({ "type": "base64", "media_type": mime, "data": b64 })
    } else if url.starts_with("http://") || url.starts_with("https://") {
        json!({ "type": "url", "url": url })
    } else {
        return None;
    };
    let mut block = json!({ "type": "image", "source": source });
    if let Some(cc) = cache_control.filter(|c| !c.is_null()) {
        block["cache_control"] = cc.clone();
    }
    Some(block)
}

/// Is this an already-built user message holding only tool_result blocks?
/// The merge in `to_wire` needs to know so consecutive results land in one
/// user message instead of a run of single-block ones.
fn is_tool_result_msg(m: &Value) -> bool {
    m["role"].as_str() == Some("user")
        && m["content"]
            .as_array()
            .is_some_and(|a| !a.is_empty() && a.iter().all(|b| b["type"] == "tool_result"))
}

// ── response: Anthropic Messages shape → OpenAI shape ───────────────────

fn from_wire(resp: &Value) -> Value {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: Vec<Value> = Vec::new();

    for block in resp["content"].as_array().into_iter().flatten() {
        match block["type"].as_str().unwrap_or("") {
            "text" => text.push_str(block["text"].as_str().unwrap_or("")),
            // The REPLAY field, deliberately: rewriting what the user SEES
            // is a post_llm_call hook's job, not a driver's.
            "thinking" => thinking.push_str(block["thinking"].as_str().unwrap_or("")),
            "tool_use" => {
                let input = if block["input"].is_null() {
                    json!({})
                } else {
                    block["input"].clone()
                };
                calls.push(json!({
                    "id": block["id"].as_str().unwrap_or(""),
                    "type": "function",
                    "function": {
                        "name": block["name"].as_str().unwrap_or(""),
                        // Back to an arguments STRING, which is what the
                        // engine's parser expects.
                        "arguments": input.to_string(),
                    },
                }));
            }
            // server_tool_use / web_search_tool_result: the endpoint ran a
            // server tool itself. Their content is not a reply to render, so
            // it is not — the model's own text block is. Skipped rather than
            // appended, because pasting a search-result JSON blob into the
            // visible answer is worse than dropping it.
            _ => {}
        }
    }

    let mut message = json!({ "role": "assistant", "content": text });
    if !thinking.is_empty() {
        // Surfaced and persisted; never replayed (see the module comment).
        message["reasoning_content"] = json!(thinking);
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }

    // Usage. The two cache fields keep their Anthropic spellings on purpose:
    // the engine's `cache_usage` already reads cache_read_input_tokens and
    // cache_creation_input_tokens, so renaming them here would throw the
    // accounting away.
    let mut usage = json!({});
    let u = &resp["usage"];
    if let Some(n) = u["input_tokens"].as_u64() {
        usage["prompt_tokens"] = json!(n);
    }
    if let Some(n) = u["output_tokens"].as_u64() {
        usage["completion_tokens"] = json!(n);
    }
    for f in ["cache_creation_input_tokens", "cache_read_input_tokens"] {
        if let Some(n) = u[f].as_u64() {
            usage[f] = json!(n);
        }
    }
    // Thinking tokens. Anthropic reports them under
    // usage.output_tokens_details.thinking_tokens; the engine's
    // `reasoning_usage` reads ONLY the OpenAI spelling
    // (completion_tokens_details.reasoning_tokens). Translated here, or the
    // number is lost and the thinking is billed as answer output — the exact
    // lie that separate counter exists to expose.
    if let Some(n) = u["output_tokens_details"]["thinking_tokens"].as_u64() {
        usage["completion_tokens_details"] = json!({ "reasoning_tokens": n });
    }

    // The engine reads finish_reason to decide whether a turn is over and
    // whether a tool round follows, so mapping it is not cosmetic:
    // `tool_use` has to become `tool_calls` or the loop stops at the first
    // tool call instead of running it.
    json!({
        "choices": [{
            "message": message,
            "finish_reason": finish_reason(resp["stop_reason"].as_str().unwrap_or("")),
        }],
        "usage": usage
    })
}

fn finish_reason(stop: &str) -> &'static str {
    match stop {
        "end_turn" | "stop_sequence" | "pause_turn" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        // Unknown stop_reason: an empty finish_reason, which the engine
        // treats as "no verdict from the provider". Guessing "stop" would
        // claim a turn ended cleanly when the wire said something this
        // driver has never seen.
        _ => "",
    }
}

// ── streaming ───────────────────────────────────────────────────────────
//
// Anthropic streams SSE, and the events are typed: message_start carries the
// input usage, content_block_start/delta/stop carry the blocks, message_delta
// carries stop_reason and the output usage, message_stop ends it, and ping is
// a keepalive to ignore. Assembling that into one response is what this does,
// and it ends by handing the NON-streaming shape to `from_wire` so the two
// paths cannot disagree about what a completion looks like.

fn drive_stream(body: &str, url: &str, headers: &str) -> Value {
    let opened = match engine(
        "http_stream_open",
        &[url, body, "application/json", headers],
    ) {
        Ok(r) => r,
        Err(e) => return err(&e, status_of(&e)),
    };
    let opened: Value = match serde_json::from_str(&opened) {
        Ok(v) => v,
        Err(e) => return err(&format!("stream open was not JSON: {e}"), 502),
    };
    let id = opened["id"].as_u64().unwrap_or(0).to_string();
    let status = opened["status"].as_u64().unwrap_or(502);

    // A non-2xx status still delivers its body as one event, so the error
    // path and the success path are the same loop. The body is pulled here so
    // the message the user sees is the endpoint's own, not "http 429".
    if status != 200 {
        let detail = engine("http_stream_next", &[&id])
            .ok()
            .and_then(|n| serde_json::from_str::<Value>(&n).ok())
            .and_then(|n| n["event"].as_str().map(str::to_string))
            .unwrap_or_default();
        let _ = engine("http_stream_close", &[&id]);
        let msg = error_message(&detail);
        let msg = if msg.is_empty() {
            format!("http {status}")
        } else {
            msg
        };
        return err(&msg, status as u64);
    }

    let mut text_by_idx: HashMap<u64, String> = HashMap::new();
    let mut kind_by_idx: HashMap<u64, String> = HashMap::new();
    let mut partial_by_idx: HashMap<u64, String> = HashMap::new();
    let mut tool_id: HashMap<u64, String> = HashMap::new();
    let mut tool_name: HashMap<u64, String> = HashMap::new();
    let mut usage: Value = json!({});
    let mut stop_reason = String::new();

    loop {
        let nxt = match engine("http_stream_next", &[&id]) {
            Ok(n) => n,
            Err(e) => {
                let _ = engine("http_stream_close", &[&id]);
                return err(&e, 502);
            }
        };
        let nxt: Value = match serde_json::from_str(&nxt) {
            Ok(v) => v,
            Err(e) => {
                let _ = engine("http_stream_close", &[&id]);
                return err(&format!("stream event was not JSON: {e}"), 502);
            }
        };
        if nxt["done"].as_bool().unwrap_or(false) {
            break;
        }
        if let Some(e) = nxt["error"].as_str() {
            let _ = engine("http_stream_close", &[&id]);
            return err(e, 502);
        }
        let data = match nxt["event"].as_str() {
            Some(d) if !d.is_empty() => d,
            _ => continue,
        };
        // `[DONE]` is an OpenAI convention this wire never sends, but a
        // gateway in front of it might; treat it as the end either way.
        if data == "[DONE]" {
            continue;
        }
        let data: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue, // an unparseable event is skipped, not fatal
        };

        match data["type"].as_str().unwrap_or("") {
            "message_start" => {
                // Input usage arrives FIRST on this wire, and the output
                // count with it is a placeholder — message_delta corrects it.
                merge_usage(&mut usage, &data["message"]["usage"]);
            }
            "content_block_start" => {
                let idx = data["index"].as_u64().unwrap_or(0);
                kind_by_idx.insert(
                    idx,
                    data["content_block"]["type"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                );
                text_by_idx.entry(idx).or_default();
                partial_by_idx.entry(idx).or_default();
                if data["content_block"]["type"].as_str() == Some("tool_use") {
                    tool_id.insert(
                        idx,
                        data["content_block"]["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    );
                    tool_name.insert(
                        idx,
                        data["content_block"]["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    );
                }
            }
            "content_block_delta" => {
                let idx = data["index"].as_u64().unwrap_or(0);
                match data["delta"]["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = data["delta"]["text"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            text_by_idx.entry(idx).or_default().push_str(t);
                            // Emitted as it arrives, which is the entire
                            // point of streaming: the surface shows tokens
                            // landing instead of waiting for the turn.
                            emit_delta("content", t);
                        }
                    }
                    "thinking_delta" => {
                        let t = data["delta"]["thinking"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            text_by_idx.entry(idx).or_default().push_str(t);
                            emit_delta("reasoning_content", t);
                        }
                    }
                    "signature_delta" => {
                        // The replay credential. Received and dropped on
                        // purpose: see the module comment on why thinking is
                        // never sent back.
                    }
                    "input_json_delta" => {
                        // Tool arguments arrive as JSON FRAGMENTS — strings
                        // that only parse once concatenated, so they
                        // accumulate raw and are assembled at the end.
                        partial_by_idx
                            .entry(idx)
                            .or_default()
                            .push_str(data["delta"]["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                    if !sr.is_empty() {
                        stop_reason = sr.to_string();
                    }
                }
                merge_usage(&mut usage, &data["usage"]);
            }
            "message_stop" => break,
            "error" => {
                let m = data["error"]["message"].as_str().unwrap_or("stream error");
                let t = data["error"]["type"].as_str().unwrap_or("");
                let _ = engine("http_stream_close", &[&id]);
                return err(m, status_of_type(t));
            }
            // ping, and anything a future wire adds, are ignored: a driver
            // that failed on an event it did not recognize would break the
            // moment Anthropic shipped one.
            _ => {}
        }
    }
    let _ = engine("http_stream_close", &[&id]);

    // Reassemble into the non-streaming shape, in block order.
    let mut idxs: Vec<u64> = kind_by_idx.keys().copied().collect();
    idxs.sort_unstable();
    let mut content: Vec<Value> = Vec::new();
    for idx in idxs {
        let kind = kind_by_idx.get(&idx).map(String::as_str).unwrap_or("");
        let accumulated = text_by_idx.get(&idx).cloned().unwrap_or_default();
        match kind {
            "tool_use" => {
                // A fragment run that does not parse — a stream cut
                // mid-argument — becomes `{}`, the same thing the engine does
                // for a malformed tool call. Failing the turn over truncated
                // arguments would throw away the text that arrived fine
                // beside them.
                let raw = partial_by_idx.get(&idx).cloned().unwrap_or_default();
                let input: Value = if raw.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
                };
                content.push(json!({
                    "type": "tool_use",
                    "id": tool_id.get(&idx).cloned().unwrap_or_default(),
                    "name": tool_name.get(&idx).cloned().unwrap_or_default(),
                    "input": input,
                }));
            }
            "thinking" => content.push(json!({ "type": "thinking", "thinking": accumulated })),
            // text, and only text: a server-tool block leaves the
            // accumulator empty because its content is not a reply to
            // render, so it is skipped here exactly as `from_wire` skips it.
            _ if !accumulated.is_empty() => {
                content.push(json!({ "type": "text", "text": accumulated }))
            }
            _ => {}
        }
    }

    from_wire(&json!({
        "content": content,
        "stop_reason": stop_reason,
        "usage": usage,
    }))
}

/// Fold one usage object into the accumulator. Anthropic spells its counters
/// differently from OpenAI and reports them at two points in a stream, so the
/// later value wins — which is what makes message_delta's authoritative
/// output_tokens override message_start's placeholder.
fn merge_usage(into: &mut Value, from: &Value) {
    for f in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if let Some(n) = from[f].as_u64() {
            into[f] = json!(n);
        }
    }
    if let Some(n) = from["output_tokens_details"]["thinking_tokens"].as_u64() {
        into["output_tokens_details"] = json!({ "thinking_tokens": n });
    }
}

/// One OpenAI-shaped streaming chunk carrying a single delta field. Split out
/// because both the text and the reasoning arms need it, and hand-writing the
/// same nesting twice is where the two would drift.
fn emit_delta(field: &str, text: &str) {
    let chunk = json!({ "choices": [{ "delta": { field: text } }] }).to_string();
    let _ = engine("provider_emit", &[&chunk]);
}

// ── the driver ──────────────────────────────────────────────────────────

fn drive(request_json: &str) -> Value {
    let req: Value = match serde_json::from_str(request_json) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("request was not JSON: {e}"), "status": 500 }),
    };

    // The route's own endpoint and credential, from the reserved
    // `veles` key the engine adds to every driver request. NOT a global
    // config lookup: one driver serves every provider entry that names
    // it, and a work account and a personal one have different keys — as
    // do two gateways fronting the same wire.
    let base = req["veles"]["base_url"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("https://api.anthropic.com")
        .to_string();
    let key = req["veles"]["api_key"].as_str().unwrap_or("");
    let model = req["model"].as_str().unwrap_or("");

    // Capabilities are consulted when the request needs a decision they
    // inform, and there are TWO such decisions, not one:
    //
    //   * which thinking/effort/sampling shape this model accepts, when the
    //     engine asked for a reasoning effort;
    //   * what `max_tokens` to send, when the operator did not pin one —
    //     Anthropic REQUIRES that field, so the driver must produce a number,
    //     and its own fallback is FALLBACK_MAX_TOKENS. On a model that takes
    //     128000 that is a silent truncation of every long answer, which
    //     reports as "the model stopped mid-sentence".
    //
    // Both are answered by the same cached catalog read, so consulting it on
    // a plain turn costs one GET per (endpoint, model) per process and then
    // nothing. A turn that neither thinks nor needs a discovered cap still
    // makes exactly one HTTP call.
    let needs_caps =
        !req["reasoning_effort"].as_str().unwrap_or("").is_empty() || configured_max_out(&req) == 0;
    let c = if needs_caps {
        caps(&base, key, model)
    } else {
        unknown_caps()
    };

    let url = format!("{}{}", trim_base(&base), endpoint_path(&req));
    let headers = headers_for(&req, key);

    // Streaming, when the engine asked for it. The request body says so;
    // `stream` is on the engine's extra_body denylist because the transport
    // owns it, and here the transport is this driver.
    if req["stream"].as_bool().unwrap_or(false) {
        let body = to_wire(&req, c, true).to_string();
        return drive_stream(&body, &url, &headers);
    }

    let body = to_wire(&req, c, false).to_string();

    // The engine's HTTP client, with its timeout and its 1 MB cap. A driver
    // that dialled its own socket would be reimplementing something it was
    // handed. The headers Anthropic requires — `x-api-key`, not bearer auth,
    // and a dated `anthropic-version` — are what the headers argument to
    // `http_post` exists for: a driver that cannot set headers cannot
    // authenticate against anything real.
    let raw = match engine("http_post", &[&url, &body, "application/json", &headers]) {
        Ok(r) => r,
        Err(e) => {
            // `http_post` raises on non-2xx with the body in the message, so
            // the status has to be read back out of it — that is what lets a
            // 429 reach the engine as a rate limit and drive failover,
            // instead of arriving as an undifferentiated driver failure.
            return err(&error_message(&e), status_of(&e));
        }
    };
    let resp: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("response was not JSON: {e}"), "status": 502 }),
    };
    // Anthropic reports API errors in a 200-shaped envelope too, so this is a
    // second check and not a redundant one.
    if let Some(t) = resp["error"]["type"].as_str() {
        return err(&error_message(&raw), status_of_type(t));
    }

    let out = from_wire(&resp);

    // A non-streaming call still emits its text as one chunk. That is the
    // honest version of "this path does not stream": the surface shows the
    // answer arriving rather than the engine silently deciding nothing was
    // said until the turn ended.
    if let Some(c) = out["choices"][0]["message"]["content"].as_str() {
        if !c.is_empty() {
            emit_delta("content", c);
        }
    }
    out
}

// ── errors ──────────────────────────────────────────────────────────────

fn err(msg: &str, status: u64) -> Value {
    json!({ "error": msg, "status": status })
}

/// The status to report. This is the half of error handling that decides
/// whether the engine retries, falls back to the next route, or fails the
/// turn: `should_fallback` fires on 429 and 5xx and nothing else, so a
/// driver that reported everything as a generic failure would take a plugin
/// route out of failover entirely.
fn status_of(raw: &str) -> u64 {
    // `http_post`'s raise shape begins "http NNN:". Scanned anywhere in the
    // string rather than only at the front, to match the Tcl twin's regexp —
    // the two have to agree on the same input or the parity test is testing
    // a coincidence. Parsed by hand because this plugin links serde_json and
    // the ABI, not a regex crate.
    let bytes = raw.as_bytes();
    let mut at = 0usize;
    while let Some(rel) = raw[at..].find("http ") {
        let start = at + rel + "http ".len();
        if let Some(code) = bytes.get(start..start + 3) {
            if code.iter().all(u8::is_ascii_digit) {
                if let Some(n) = std::str::from_utf8(code)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return n;
                }
            }
        }
        at = start;
    }
    502
}

/// Anthropic's own error TYPE → a status. The envelope arrives with HTTP 200
/// from some gateways, so the type is the only evidence, and mapping it is
/// what lets an overloaded_error fall back instead of failing the turn.
fn status_of_type(t: &str) -> u64 {
    match t {
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        "api_error" => 500,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "invalid_request_error" => 400,
        _ => 502,
    }
}

/// The message the user should see: the endpoint's own error text when there
/// is one, else the transport's. Extracted from a JSON error envelope if the
/// payload has one, because a wall of escaped punctuation is not an answer
/// where "rate_limit_error: too many tokens" is. Returns "" when there is
/// nothing worth showing.
fn error_message(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        if let (Some(t), Some(m)) = (v["error"]["type"].as_str(), v["error"]["message"].as_str()) {
            return format!("{t}: {m}");
        }
    }
    // `http_post`'s raise shape: "http NNN: <body>". Find the FIRST "http "
    // occurrence, then the ": " after it, and treat everything past that as
    // the candidate JSON body. The Tcl twin's regexp does the same
    // left-to-right match, so both reach the same substring on the same
    // input.
    if let Some(h) = raw.find("http ") {
        if let Some(rel) = raw[h..].find(": ") {
            let body = &raw[h + rel + 2..];
            if let Ok(v) = serde_json::from_str::<Value>(body) {
                if let (Some(t), Some(m)) =
                    (v["error"]["type"].as_str(), v["error"]["message"].as_str())
                {
                    return format!("{t}: {m}");
                }
            }
        }
    }
    raw.to_string()
}

fn init(h: &Host) -> Result<(), String> {
    let _ = HOST.set(*h);
    h.register_provider("anthropic-messages", |req| drive(req).to_string());
    Ok(())
}

veles_plugin!(init);
