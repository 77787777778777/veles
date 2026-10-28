//! The engine operations a plugin can drive, in one table, with no
//! language in them.
//!
//! # Why this module exists
//!
//! Until 2026-09-04 these seventeen capabilities lived *inside* the Tcl
//! FFI shims in `bridge.rs`: `ffi_call_llm` did the arity check, the
//! argument extraction, the work, and `Tcl_SetObjResult` in one
//! function. Nothing that was not a Tcl interpreter could reach any of
//! them. That is why the out-of-tree Rust plugin ABI
//! (`crates/veles-plugin-abi`) shipped with three function pointers,
//! all of them registration, and no way to call back into the engine at
//! all: a `.so` could be called BY veles and could never call veles.
//! `rustplugins.rs` claimed both plugin kinds "drive the SAME engine
//! capabilities", which was true of in-tree plugins (they link
//! veles-core) and false of every out-of-tree one.
//!
//! So the bodies moved here, behind one entry point, and both surfaces
//! became adapters over it:
//!
//! ```text
//!   Tcl  veles::history $sid 5   ──┐
//!                                  ├─→ bridgeops::dispatch(op, args)
//!   C    host_call("history", …) ──┘
//! ```
//!
//! Parity is now structural rather than aspirational: there is one
//! list, `OP_NAMES`, and `tclrt`'s taxonomy test cross-checks it against
//! the live interpreter's `info commands ::veles::*` and against
//! `plugins/veles_api.tcl`. Adding an op without wiring all three fails
//! the gate.
//!
//! # What is NOT here
//!
//! Three commands are Tcl-only because their argument is a Tcl PROC
//! NAME, which has no meaning across a C boundary:
//! `veles::register_tool`, `veles::register_command` and the hook
//! registry (`::veles::hook` and friends, which are Tcl procs in the
//! `tclrt` prelude, not native commands at all). The `.so` side gets
//! equivalents as ABI function pointers taking a callback instead of a
//! name — a different shape for the same capability, so they cannot
//! share a table entry.
//!
//! `veles::tool_call` is here, but `bridge.rs` keeps a Tcl fast path in
//! FRONT of it: when the target tool is itself a Tcl proc, the calling
//! thread IS its interpreter's thread, so the async dispatch below
//! would post an Eval request to the very thread waiting for it. That
//! fast path is a property of the caller, not of the operation.
//!
//! # Contract
//!
//! `Ok(String)` is the result; `Err(String)` is the message the caller
//! shows. Never panics: a panic here would cross an FFI boundary in
//! both directions, which is UB in one and a process abort in the
//! other.

use crate::bridge::{parse_json_lenient, Bridge};
use crate::providers::{ChatMessage, ChatRequest};
use serde_json::Value;
use std::time::Duration;

/// Every operation reachable from BOTH plugin languages, sorted.
///
/// This is the parity list. `tclrt::tests::the_api_stub_declares_every_
/// veles_command` checks it against the live interpreter and the API
/// stub; `rustplugins` checks that `host_call` accepts exactly these.
pub const OP_NAMES: &[&str] = &[
    "call_llm",
    "config_get",
    "config_set",
    "history",
    "http_get",
    "http_post",
    "http_stream_close",
    "http_stream_next",
    "http_stream_open",
    "json_get",
    "limits",
    "note",
    "nodes",
    "principal",
    "provider_emit",
    "routes",
    "sandbox_where",
    "send_outbound",
    "service_call",
    "service_next",
    "services",
    "sessions",
    "task_cancel",
    "tasks",
    "tool_call",
    "usage_totals",
];

/// Registrars that exist in Tcl as native commands but cannot be table
/// entries, because their last argument is a proc name. Named here so
/// the taxonomy test can account for the difference instead of every
/// caller keeping its own copy of the exception list.
pub const TCL_ONLY_COMMANDS: &[&str] = &[
    "register_command",
    "register_provider",
    "register_service",
    "register_tool",
];

/// Run one operation. `args` excludes the operation name (Tcl's
/// `objv[0]`), so `args.len()` is the Tcl `objc - 1`.
pub fn dispatch(bridge: &Bridge, op: &str, args: &[String]) -> Result<String, String> {
    match op {
        "call_llm" => call_llm(bridge, args),
        "config_get" => config_get(bridge, args),
        // `tool_call` and `config_set` were the only two ops that did
        // not announce themselves — `send_outbound`, `note`,
        // `task_cancel` and `call_llm` all do — which made the two most
        // powerful things a plugin can do the two invisible ones. The
        // doctrine in notes.rs is that EVERY act a plugin performs
        // announces itself.
        "config_set" => config_set(args),
        "history" => history(bridge, args),
        "http_get" => http_get(bridge, args),
        "http_post" => http_post(bridge, args),
        "http_stream_open" => http_stream_open(bridge, args),
        "http_stream_next" => http_stream_next(bridge, args),
        "http_stream_close" => http_stream_close(args),
        "json_get" => json_get(args),
        "limits" => limits(),
        "principal" => principal(args),
        "sandbox_where" => sandbox_where(args),
        "note" => note(args),
        "nodes" => nodes(bridge),
        "provider_emit" => provider_emit(args),
        "routes" => routes(bridge),
        "send_outbound" => send_outbound(args),
        "service_call" => service_call(bridge, args),
        "service_next" => service_next(bridge, args),
        "services" => Ok(crate::services::report_json().to_string()),
        "sessions" => sessions(bridge),
        "task_cancel" => task_cancel(args),
        "tasks" => Ok(crate::tasks::listing()),
        "tool_call" => tool_call(bridge, args),
        "usage_totals" => usage_totals(bridge, args),
        other => Err(format!(
            "unknown veles op `{other}` — this build has: {}",
            OP_NAMES.join(", ")
        )),
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    /// The guard is specific on purpose: broad enough and everyone sets
    /// `allow_acl_writes = true` permanently, which is worse than no
    /// guard at all.
    #[test]
    fn the_config_guard_covers_the_access_model_and_the_fence_only() {
        for blocked in [
            "acl",
            "acl.owner",
            "acl.limits.default_max_tokens_per_day",
            "sandbox.user_env_network",
            "sandbox.mounts",
            "sandbox.extra_args",
            "sandbox.capabilities",
            "sandbox.admin_default_zone",
            "tool_caps",
            "tool_caps.wordcount",
            // The guard cannot be the thing a plugin turns off.
            "plugins.allow_acl_writes",
        ] {
            assert!(
                guarded_path(blocked).is_some(),
                "{blocked} decides what others may do and must be guarded"
            );
        }
        for allowed in [
            "sandbox.memory_mb",
            "sandbox.image",
            "sandbox.max_concurrent_exec",
            "sandbox.user_env_idle_stop_secs",
            "model.default",
            "agent.guards.max_iterations",
            "mechanisms.silenced",
            // Not a prefix match on a shorter name: `sandbox.mounts`
            // is guarded, `sandbox.mounts_are_fine` is not a key at all
            // but must not be caught by accident either way.
            "sandbox.backend_notes",
        ] {
            assert!(
                guarded_path(allowed).is_none(),
                "{allowed} is ordinary work and must not need the escape hatch"
            );
        }
    }

    /// Every op in the table announces itself, so a plugin cannot act
    /// invisibly. `tool_call` and `config_set` were the two exceptions
    /// — the two most powerful ones.
    #[test]
    fn the_powerful_ops_announce_themselves() {
        let (_home, _g) = crate::testenv::scratch_home();
        crate::notes::take_plugin_notes(); // drain anything parked
        let _ = config_set(&["model.default".into(), "\"x\"".into()]);
        let notes = crate::notes::take_plugin_notes();
        assert!(
            notes.iter().any(|n| n.summary.contains("set config")),
            "config_set must announce: {:?}",
            notes.iter().map(|n| &n.summary).collect::<Vec<_>>()
        );
    }
}

/// Is this config path one that decides what OTHER callers may do?
///
/// Deliberately a short, specific list rather than whole sections. A
/// plugin nudging `sandbox.memory_mb` is ordinary work and refusing it
/// would teach people to set `allow_acl_writes = true` and leave it on,
/// which defeats the guard. What is listed is the access model itself
/// and the parts of `[sandbox]` that ARE the fence — the posture, the
/// host mounts, the capability set, the backend, and the two knobs that
/// decide whether a caller is fenced at all.
fn guarded_path(path: &str) -> Option<&'static str> {
    let p = path.trim().to_ascii_lowercase();
    if p == "acl" || p.starts_with("acl.") {
        return Some("`[acl]` is the access model itself, and it is live-reloaded");
    }
    const FENCE: &[&str] = &[
        "sandbox.user_env_network",
        "sandbox.mounts",
        "sandbox.extra_args",
        "sandbox.capabilities",
        "sandbox.backend",
        "sandbox.admin_default_zone",
        "sandbox.local_default_zone",
        "sandbox.assign",
        "sandbox.envs",
    ];
    if FENCE
        .iter()
        .any(|k| p == *k || p.starts_with(&format!("{k}.")))
    {
        return Some("that key IS the sandbox fence, or decides who is inside it");
    }
    if p == "tool_caps" || p.starts_with("tool_caps.") {
        return Some("`[tool_caps]` classifies which capability a tool needs");
    }
    if p == "plugins.allow_acl_writes" {
        return Some("that would be the guard unlocking itself");
    }
    None
}

/// Arity guard. The message keeps the `veles::` spelling because that
/// is what a plugin author reads in `plugins/veles_api.tcl`, whichever
/// language they called from.
fn arity(args: &[String], min: usize, max: usize, usage: &str) -> Result<(), String> {
    if args.len() < min || args.len() > max {
        return Err(format!("usage: {usage}"));
    }
    Ok(())
}

// ── LLM ──────────────────────────────────────────────────────────────

fn call_llm(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        4,
        "veles::call_llm <system> <user> ?provider? ?model?",
    )?;
    let (system, user) = (args[0].clone(), args[1].clone());
    let provider = args.get(2).filter(|s| !s.is_empty()).cloned();
    let model = args.get(3).filter(|s| !s.is_empty()).cloned();

    let routes = bridge
        .routes_or_err()
        .map_err(|e| format!("no routes configured: {e}"))?;

    // Route selection: explicit provider wins; else resolve through the
    // failover-aware aux router (honors aux_fallback so plugin aux calls
    // move off a throttled scope).
    let route = match &provider {
        Some(p) => match routes.iter().find(|r| r.provider == *p) {
            Some(r) => Some(r.clone()),
            None => return Err(format!("unknown provider: {p}")),
        },
        None => {
            let r = crate::auxroute::resolve_aux_route(bridge.cfg(), routes, None).cloned();
            // Only a DIVERSION is worth reporting: aux work on the
            // conversation's own route is not a mechanism acting behind
            // the user's back.
            if let (Some(r), Some(primary)) = (r.as_ref(), routes.first()) {
                if r.provider != primary.provider || r.model != primary.model {
                    crate::notes::record_aux_route("a plugin's LLM call", &r.provider, &r.model);
                }
            }
            r
        }
    };
    let Some(route) = route else {
        return Err("no routes configured".to_string());
    };
    let req = ChatRequest {
        model: model.unwrap_or_else(|| route.model.clone()),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: system,
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: None,
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
            ChatMessage::user(&user),
        ],
        tools: vec![],
        reasoning_effort: None,
        stream: false,
        replay_reasoning: false,
        cache_mode: crate::providers::CacheMode::Auto,
        extra_body: None,
    };
    let provider_name = route.provider.clone();
    bridge
        .block_on(route.client.chat(&req, &provider_name))
        .map(|c| c.message.content)
        .map_err(|e| format!("call_llm failed: {e}"))
}

fn routes(bridge: &Bridge) -> Result<String, String> {
    let routes = bridge
        .routes_or_err()
        .map_err(|e| format!("no routes configured: {e}"))?;
    Ok(routes
        .iter()
        .map(|r| format!("{} -> {}", r.provider, r.model))
        .collect::<Vec<_>>()
        .join("\n"))
}

// ── store reads ──────────────────────────────────────────────────────

fn sessions(bridge: &Bridge) -> Result<String, String> {
    let store = bridge
        .store_conn()
        .map_err(|e| format!("store unavailable: {e}"))?;
    let metas = store
        .session_meta(50)
        .map_err(|e| format!("sessions failed: {e}"))?;
    let arr: Vec<Value> = metas
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "title": m.title,
                "parent": m.parent,
                "created_at": m.created_at,
                "message_count": m.message_count,
            })
        })
        .collect();
    Ok(serde_json::to_string(&arr).unwrap_or_default())
}

/// `veles::nodes` — the cluster registry as JSON, for the desktop's
/// pool view and any Tcl plugin that wants to reason about placement.
/// Read-only from the bridge: registration/heartbeat is the gateway's
/// own boot path (`node::register_self`), not something a plugin should
/// be able to forge on a whim.
fn nodes(bridge: &Bridge) -> Result<String, String> {
    let store = bridge
        .store_conn()
        .map_err(|e| format!("store unavailable: {e}"))?;
    let rows = store
        .nodes_list()
        .map_err(|e| format!("nodes failed: {e}"))?;
    // A read: no identity is minted here (NodeIdentity::load, not
    // load_or_create), and staleness uses the SAME threshold the CLI
    // does — the first version hardcoded 300s here, so the two surfaces
    // could disagree about whether one node was alive.
    let self_id = crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
        .unwrap_or_default();
    let stale_after = crate::config::load()
        .map(|c| c.cluster.stale_after_secs)
        .unwrap_or_else(|_| crate::config::ClusterSection::default().stale_after_secs)
        as i64;
    let arr: Vec<Value> = rows
        .into_iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "name": n.name,
                "host": n.host,
                "version": n.version,
                "uptime_class": n.uptime_class,
                "gpu": n.gpu,
                "endpoint": n.endpoint,
                "caps": n.caps,
                "registered_at": n.registered_at,
                "heartbeat_at": n.heartbeat_at,
                "self": n.id == self_id,
                "stale": crate::node::is_stale(&n.heartbeat_at, stale_after),
            })
        })
        .collect();
    Ok(serde_json::to_string(&arr).unwrap_or_default())
}

fn history(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(args, 1, 2, "veles::history <session_id> ?limit?")?;
    let sid = args[0].clone();
    let limit: u32 = match args.get(1).filter(|s| !s.is_empty()) {
        Some(s) => s
            .parse::<u32>()
            .map(|n| n.clamp(1, 200))
            .map_err(|_| "limit must be a number".to_string())?,
        None => 20,
    };
    let store = bridge
        .store_conn()
        .map_err(|e| format!("store unavailable: {e}"))?;
    let (msgs, total) = store
        .recent_messages(&sid, limit)
        .map_err(|e| format!("history failed: {e}"))?;
    let arr: Vec<Value> = msgs
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "msg_id": m.msg_id,
                "role": m.role,
                "content": m.content,
                "tool_name": m.tool_name,
                "created_at": m.created_at,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "session_id": sid,
        "total": total,
        "messages": arr,
    })
    .to_string())
}

fn usage_totals(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(args, 0, 1, "veles::usage_totals ?session_id?")?;
    let sid = args.first().filter(|s| !s.is_empty()).cloned();
    let store = bridge
        .store_conn()
        .map_err(|e| format!("store unavailable: {e}"))?;
    let t = store
        .usage_totals(sid.as_deref())
        .map_err(|e| format!("usage_totals failed: {e}"))?;
    Ok(serde_json::json!({
        "calls": t.calls,
        "prompt_tokens": t.prompt_tokens,
        "completion_tokens": t.completion_tokens,
        "cached_tokens": t.cached_tokens,
        "cache_write_tokens": t.cache_write_tokens,
    })
    .to_string())
}

// ── config ───────────────────────────────────────────────────────────

fn config_get(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(args, 1, 1, "veles::config_get <dotted.path>")?;
    let root = serde_json::to_value(bridge.cfg())
        .map_err(|e| format!("config_get: serialize failed: {e}"))?;
    let mut node = &root;
    for seg in args[0].split('.') {
        match node.get(seg) {
            Some(n) => node = n,
            // json_get convention: a missing path is "" and not an error.
            None => return Ok(String::new()),
        }
    }
    Ok(scalar(node))
}

fn config_set(args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        2,
        "veles::config_set <dotted.path> <json-value> (\"\" or null unsets)",
    )?;
    let path = args[0].trim().to_string();
    if path.is_empty() {
        return Err("config_set: no path supplied".to_string());
    }
    // The ACL and the sandbox posture are the two sections that decide
    // what everyone ELSE may do, and `[acl]` is now genuinely
    // hot-reloaded — so an unrestricted `config_set` let a plugin grant
    // itself owner and have it take effect immediately. Plugins are
    // owner-installed code, so this is not a stranger-reachable hole;
    // it is a capability nobody declared and nothing announced.
    //
    // Off by default, one line to allow it, and the refusal names the
    // line.
    if let Some(why) = guarded_path(&path) {
        if !crate::config::live().plugins.allow_acl_writes {
            return Err(format!(
                "config_set: `{path}` is refused — {why}. Set `[plugins] allow_acl_writes = \
                 true` to permit it."
            ));
        }
    }
    let segments: Vec<String> = path.split('.').map(str::to_string).collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(format!("config_set: `{path}` has an empty path segment"));
    }
    // Announced HERE rather than in the dispatch arm, so every path
    // into the op says so — and after the guard, because "a plugin set
    // config" would be false for a write that was refused.
    //
    // `tool_call` and `config_set` were the only two ops that did not
    // announce themselves; `send_outbound`, `note`, `task_cancel` and
    // `call_llm` all do. The two most powerful things a plugin can do
    // were the two invisible ones, against notes.rs's explicit
    // doctrine that every act a plugin performs announces itself.
    crate::notes::record_plugin_note(
        &format!("a plugin set config `{path}`"),
        "veles::config_set edits config.toml in place (comments and order preserved, \
         .bak written). The access model and the sandbox fence are refused unless \
         [plugins] allow_acl_writes is set.",
    );
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let raw = args[1].clone();
    // JSON first (numbers/bools/arrays/objects keep their type), plain
    // string fallback so `config_set sandbox.backend podman` works
    // without quoting games.
    let value = if raw.trim().is_empty() {
        Value::Null
    } else {
        parse_json_lenient(&raw).unwrap_or_else(|_| Value::String(raw.clone()))
    };
    let report = crate::configedit::edit(&path, |doc| match value.clone() {
        Value::Null => crate::configedit::unset(doc, &refs),
        v => {
            let tv = crate::configedit::json_to_toml(&v)?;
            crate::configedit::set_value(doc, &refs, tv)
        }
    })
    .map_err(|e| format!("config_set: {e:#}"))?;
    Ok(serde_json::json!({
        "saved": report.path.display().to_string(),
        "backup": report.backup.as_ref().map(|b| b.display().to_string()),
        "effect": report.effect.as_str(),
        "note": report.effect.note(),
    })
    .to_string())
}

// ── http ─────────────────────────────────────────────────────────────

/// Body cap for the plugin bridge (slopdrop parity) — a chatty endpoint
/// must not blow the caller's result string.
const HTTP_BODY_CAP: usize = 1_000_000;

fn http_get(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(args, 1, 2, "veles::http_get <url> ?headers_json?")?;
    http_request(bridge, "GET", &args[0], None, args.get(1))
}

fn http_post(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        4,
        "veles::http_post <url> <body> ?content_type? ?headers_json?",
    )?;
    let ct = args
        .get(2)
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "application/json".into());
    http_request(
        bridge,
        "POST",
        &args[0],
        Some((args[1].clone(), ct)),
        args.get(3),
    )
}

// ── streaming HTTP: the transport `http_post` cannot be ────────────────
//
// Three ops, and the reason they are three is in `streamhttp`'s module
// docs. The short version: `http_post` buffers a whole response and
// carries a 30 second total timeout, so a provider driver using it can
// neither show incremental output nor survive a long turn. These give a
// driver a real server-sent-event stream to pull from, in the same
// blocking-call shape `service_next` established — which is what makes
// the capability identical in Tcl and in an out-of-tree `.so`, rather
// than something only the C ABI can express.

/// `veles::http_stream_open <url> <body> ?content_type? ?headers_json?`
///
/// Opens a streaming POST and answers once the STATUS is known:
/// `{"id":N,"status":200}`. The status arriving first is the point — a
/// driver classifies 429/5xx before reading a byte of body, which is what
/// lets a plugin route take part in failover.
fn http_stream_open(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        4,
        "veles::http_stream_open <url> <body> ?content_type? ?headers_json?",
    )?;
    let ct = args
        .get(2)
        .filter(|s| !s.is_empty())
        .map(String::as_str)
        .unwrap_or("application/json");
    crate::streamhttp::open(
        bridge,
        &args[0],
        &args[1],
        ct,
        args.get(3).map(String::as_str),
    )
}

/// `veles::http_stream_next <id>`
///
/// Pulls one event: `{"event":"…"}`, or `{"done":true}` when the server
/// finished, or `{"error":"…"}` if the transport failed. Both terminal
/// answers free the handle, so a driver that returns early on an error
/// leaks nothing.
fn http_stream_next(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(args, 1, 1, "veles::http_stream_next <id>")?;
    let id = stream_id(&args[0])?;
    crate::streamhttp::next(bridge, id)
}

/// `veles::http_stream_close <id>`
///
/// Closes a stream and frees its handle. Idempotent, because what a
/// driver needs guaranteed is that nothing is left running — not that it
/// can tell whether something was.
fn http_stream_close(args: &[String]) -> Result<String, String> {
    arity(args, 1, 1, "veles::http_stream_close <id>")?;
    let id = stream_id(&args[0])?;
    crate::streamhttp::close(id)
}

/// The id both `next` and `close` take. Parsed here rather than in each so
/// a non-numeric id gives the same message from either op, and so neither
/// can be handed a negative or overflowing value that `streamhttp` would
/// then have to defend against.
pub(crate) fn stream_id(raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("stream id must be a positive integer, got {raw:?}"))
}

/// Parse the optional `headers_json` argument: a JSON object of
/// name → value.
///
/// Added 2026-09-04 with provider drivers, because without it a driver
/// cannot AUTHENTICATE. Anthropic wants `x-api-key` and
/// `anthropic-version`; Azure wants `api-key`; anything with a custom
/// gateway wants something else. Both reference drivers documented
/// those headers before this existed and neither could send one, which
/// made a "complete driver" a driver that could not reach a real
/// endpoint.
///
/// No denylist. The plugins interpreter is the TRUSTED owner surface —
/// the same surface that already has `config_set` and `send_outbound` —
/// so a header it cannot construct is a malformed-input error, not a
/// policy decision.
fn parse_headers(raw: Option<&String>) -> Result<Vec<(String, String)>, String> {
    parse_headers_arg(raw.map(String::as_str))
}

/// The `&str` form, for callers inside the crate that already have the
/// argument as a string slice (`streamhttp`, which shares this parser so
/// that a header is validated the same way whichever transport sends it).
pub(crate) fn parse_headers_arg(raw: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let v = parse_json_lenient(raw).map_err(|e| format!("headers must be JSON: {e}"))?;
    let Value::Object(map) = v else {
        return Err("headers must be a JSON OBJECT of name -> value".to_string());
    };
    let mut out = Vec::new();
    for (k, val) in map {
        let s = match val {
            Value::String(s) => s,
            Value::Null => continue,
            other => other.to_string(),
        };
        out.push((k, s));
    }
    Ok(out)
}

/// 30s timeout, 1MB body cap. The plugins interpreter is the TRUSTED
/// owner surface — untrusted callers never reach these.
fn http_request(
    bridge: &Bridge,
    method: &str,
    url: &str,
    body: Option<(String, String)>,
    headers_json: Option<&String>,
) -> Result<String, String> {
    let headers = parse_headers(headers_json)?;
    bridge.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let req = match method {
            "POST" => {
                let (b, ct) = body.unwrap_or_default();
                client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, ct)
                    .body(b)
            }
            _ => client.get(url),
        };
        // Named individually so a typo says WHICH header was wrong,
        // rather than the whole request failing anonymously.
        let mut req = req;
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| format!("header name {k:?}: {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(&v)
                .map_err(|e| format!("header {k} value: {e}"))?;
            req = req.header(name, value);
        }
        let resp = req.send().await.map_err(|e| format!("http: {e}"))?;
        let status = resp.status();
        let mut text = resp.text().await.map_err(|e| format!("http body: {e}"))?;
        if text.len() > HTTP_BODY_CAP {
            text.truncate(HTTP_BODY_CAP);
            text.push_str("\n[truncated at 1MB]");
        }
        if status.is_success() {
            Ok(text)
        } else {
            Err(format!("http {status}: {text}"))
        }
    })
}

// ── gateway, notes, tasks ────────────────────────────────────────────

fn send_outbound(args: &[String]) -> Result<String, String> {
    arity(
        args,
        3,
        3,
        "veles::send_outbound <platform> <chat_id> <text>",
    )?;
    let (platform, chat_id, text) = (args[0].clone(), args[1].clone(), args[2].clone());
    let Some(bus) = crate::bus::global_bus() else {
        return Err("gateway not running — no outbound channel".to_string());
    };
    let msg =
        crate::bus::OutboundMessage::plain(&chat_id, text.clone(), crate::bus::MessageKind::Reply);
    if bus.send_outbound(&platform, msg) {
        // Silent outbound automation is prohibited: park the note so
        // the next turn surfaces what the plugin just sent, where.
        crate::notes::record_plugin_note(
            &format!("plugin sent a message to {platform}/{chat_id}"),
            &format!("{} chars", text.len()),
        );
        Ok("sent".to_string())
    } else {
        Err(format!("no adapter registered for platform `{platform}`"))
    }
}

/// A plugin announcing its own automation on the transcript (the
/// visibility half of the notes charter).
fn note(args: &[String]) -> Result<String, String> {
    arity(args, 1, 2, "veles::note <summary> ?detail?")?;
    crate::notes::record_plugin_note(&args[0], args.get(1).map(String::as_str).unwrap_or(""));
    Ok(String::new())
}

/// Emit one streaming chunk from inside a provider driver.
///
/// A driver runs on a thread that is not the one waiting for its
/// answer — the Tcl one is an interpreter thread reached through a
/// request/reply hop — so this is how a partial result gets out while
/// the driver is still working. The chunk is an OpenAI-shaped
/// `chat.completion.chunk` object; the engine feeds it through the same
/// delta handling a native stream uses.
///
/// An op rather than a callback threaded through the driver
/// registration, which is what makes the two languages match: the C
/// ABI's `register_provider` needs to carry nothing but the chat
/// function, because streaming goes back out through `host_call` the
/// same way Tcl's goes out through `veles::provider_emit`.
///
/// Called outside a driver call it is an ERROR, not a no-op: a plugin
/// emitting into nothing has misunderstood the contract and should hear
/// about it.
fn provider_emit(args: &[String]) -> Result<String, String> {
    arity(args, 1, 1, "veles::provider_emit <chunk_json>")?;
    let chunk = parse_json_lenient(&args[0])
        .map_err(|e| format!("provider_emit: chunk is not JSON: {e}"))?;
    if crate::providers::emit_chunk(&chunk) {
        Ok(String::new())
    } else {
        Err("provider_emit called outside a provider driver call — \
             it only means something while your driver proc is running"
            .to_string())
    }
}

fn task_cancel(args: &[String]) -> Result<String, String> {
    arity(args, 1, 1, "veles::task_cancel <id>")?;
    let id: u32 = args[0]
        .parse()
        .map_err(|_| "task id must be a number".to_string())?;
    if crate::tasks::cancel(id) {
        crate::notes::record_plugin_note(&format!("plugin cancelled background task #{id}"), "");
        Ok("ok".to_string())
    } else {
        Err(format!("unknown or finished task id: {id}"))
    }
}

// ── tools ────────────────────────────────────────────────────────────

/// Dispatch a real tool exactly as a turn would (aliases and deferred
/// tools resolve the same way).
///
/// The Tcl caller in `bridge.rs` handles Tcl-proc-backed tools BEFORE
/// reaching here — see this module's header. Everything arriving at
/// this function goes through the async registry, under a timeout: a
/// tool that fires HOOKS would hop to the Tcl thread, and if the caller
/// is that thread the wedge is permanent. The timeout turns a hang into
/// a message.
fn tool_call(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        3,
        "veles::tool_call <name> <args_json> ?session_id?",
    )?;
    let name = args[0].clone();
    // Whose call this is: the session the plugin NAMED, else the session
    // whose hook is running (set by the engine around every handler
    // call), else the last-published registry. Resolution and its
    // failure modes live in `tools::plugin_call_registry`; the note names
    // the answer so an owner reading it knows which principal was
    // judged, instead of assuming it was theirs.
    let explicit = args.get(2).map(String::as_str);
    let ambient = crate::tools::ambient_hook_session();
    let (registry, judged_as) = crate::tools::plugin_call_registry(explicit, ambient.as_deref())?;
    crate::notes::record_plugin_note(
        &format!("a plugin called the `{name}` tool"),
        &format!(
            "veles::tool_call dispatches through {judged_as}, and acl::gate_denial judges the \
             call as that session's principal — the same check the model-driven tool loop makes."
        ),
    );
    let parsed: Value = parse_json_lenient(&args[1]).map_err(|e| format!("bad args json: {e}"))?;
    let out = bridge.block_on(async {
        tokio::time::timeout(Duration::from_secs(300), registry.dispatch(&name, parsed)).await
    });
    Ok(out.unwrap_or_else(|_| {
        format!(
            "error: tool `{name}` timed out — tools that fire hooks cannot run \
             from plugin context (the hook hop needs this very thread)"
        )
    }))
}

/// `veles::service_call <name> <params_json> ?session_id?` — another
/// plugin's (or core's) service, through the registry
/// (`crate::services`). The provider's declared access decides: an
/// internal service refuses every plugin, a capability-gated one judges
/// the named (or ambient hook) session's principal. A string answer is
/// its text; anything else is its JSON.
fn service_call(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        3,
        "veles::service_call <name> <params_json> ?session_id?",
    )?;
    let params: Value =
        parse_json_lenient(&args[1]).map_err(|e| format!("bad params json: {e}"))?;
    let caller = crate::lifecycle::thread_owner()
        .unwrap_or("a plugin")
        .to_string();
    let sid = args.get(2).cloned();
    let name = args[0].clone();
    let out = bridge.block_on(async move {
        crate::services::call_from_plugin(&caller, &name, params, sid).await
    })?;
    Ok(match out {
        Value::String(s) => s,
        v => v.to_string(),
    })
}

/// `veles::service_next <token> <params_json>` — what the layer below a
/// provider answers (PLAN-extension-api.md D2, `services::call_next`).
/// The token is the `next` its own request carried: it names the layer,
/// so there is no caller to check and nothing to gate here (a provider
/// asking the layer it covers is inside the service already), and it is
/// spent when that request ends.
fn service_next(bridge: &Bridge, args: &[String]) -> Result<String, String> {
    arity(args, 2, 2, "veles::service_next <token> <params_json>")?;
    let params: Value =
        parse_json_lenient(&args[1]).map_err(|e| format!("bad params json: {e}"))?;
    let token = args[0].clone();
    let out = bridge.block_on(async move { crate::services::call_next(&token, params).await })?;
    Ok(match out {
        Value::String(s) => s,
        v => v.to_string(),
    })
}

// ── who am I acting for ──────────────────────────────────────────────
//
// The bridge predates the identity model entirely: sixteen ops, none of
// which could answer "who is this call for". So a plugin hook firing on
// an inbound message could not tell a guest from the owner, which is
// the one distinction the whole ACL round exists to draw. These three
// are READ-ONLY — a plugin observing the model it lives in, not
// changing it.

/// `veles::principal ?session_id?` — the caller a session acts for.
fn principal(args: &[String]) -> Result<String, String> {
    let sid = args.first().map(String::as_str).unwrap_or("");
    let p = if sid.is_empty() {
        None
    } else {
        crate::acl::session_principal(sid)
    };
    let Some(p) = p else {
        // An owner surface has no principal at all, and saying so is
        // the true answer — not a fabricated guest.
        return Ok(serde_json::json!({
            "session_id": sid,
            "principal": Value::Null,
            "note": "no principal — an owner surface (desktop/TUI/CLI/REPL) or an unknown session",
        })
        .to_string());
    };
    let acl = crate::acl::live_config();
    Ok(serde_json::json!({
        "session_id": sid,
        "principal": {
            "subject": p.subject,
            "id": p.id,
            "mask": p.mask,
            "role": p.role.as_str(),
            "via": p.via,
            "label": p.label(),
            "caps": p.caps.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "max_context_tokens": p.max_context_tokens(&acl),
            "max_tokens_per_day": acl.limits.tokens_per_day_for_subject(p.role, &p.subject, &p.budget_key()),
            "max_turns_per_hour": acl.limits.turns_per_hour_for_subject(p.role, &p.subject, &p.budget_key()),
        }
    })
    .to_string())
}

/// `veles::sandbox_where ?session_id?` — the zone a session's tool
/// calls land in, and which box.
fn sandbox_where(args: &[String]) -> Result<String, String> {
    let sid = args.first().map(String::as_str).unwrap_or("");
    let zone = crate::user_envs::zone_for(
        (!sid.is_empty()).then_some(sid),
        crate::user_envs::ZoneArg::Unset,
    );
    Ok(match zone {
        crate::user_envs::Zone::Host => serde_json::json!({
            "session_id": sid, "zone": "host", "sandbox": Value::Null,
        }),
        crate::user_envs::Zone::Sandbox { name, key } => serde_json::json!({
            "session_id": sid,
            "zone": "sandbox",
            "sandbox": name,
            "key": key,
            "fenced": crate::user_envs::env_is_fenced(),
        }),
    }
    .to_string())
}

/// `veles::limits` — the budget tables, runtime overrides applied.
fn limits() -> Result<String, String> {
    use crate::acl::Role;
    let acl = crate::acl::live_config();
    let rows: Vec<Value> = [
        Role::Guest,
        Role::User,
        Role::Trusted,
        Role::Admin,
        Role::Owner,
    ]
    .iter()
    .map(|r| {
        serde_json::json!({
            "role": r.as_str(),
            "max_context_tokens": acl.limits.for_role(*r),
            "max_tokens_per_day": acl.limits.tokens_per_day_for_role(*r),
            "max_turns_per_hour": acl.limits.turns_per_hour_for_role(*r),
        })
    })
    .collect();
    Ok(serde_json::json!({
        "by_role": rows,
        "max_concurrent_turns": acl.limits.max_concurrent_turns,
    })
    .to_string())
}

// ── json ─────────────────────────────────────────────────────────────

/// A JSON node as the string a plugin expects: strings unquoted, null
/// empty, everything else re-serialized.
fn scalar(node: &Value) -> String {
    match node {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn json_get(args: &[String]) -> Result<String, String> {
    arity(
        args,
        2,
        usize::MAX,
        "veles::json_get <json> <key> ?key2 ...?",
    )?;
    let mut node: Value =
        parse_json_lenient(&args[0]).map_err(|e| format!("json_get: bad json: {e}"))?;
    for key in &args[1..] {
        // Arrays take numeric indices ("messages 0 role"); objects take
        // keys. Anything else is a miss (json_get convention: "" + Ok).
        node = match &node {
            Value::Array(arr) => match key.parse::<usize>() {
                Ok(idx) => match arr.get(idx) {
                    Some(n) => n.clone(),
                    None => return Ok(String::new()),
                },
                Err(_) => return Ok(String::new()),
            },
            _ => match node.get(key) {
                Some(n) => n.clone(),
                None => return Ok(String::new()),
            },
        };
    }
    Ok(scalar(&node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A bridge over a scratch home with an empty config: no routes, a
    /// real (empty) store path, everything else default. Enough for
    /// every op that does not need the wire.
    fn bridge() -> (tempfile::TempDir, crate::testenv::EnvGuard, Arc<Bridge>) {
        let (home, guard) = crate::testenv::scratch_home();
        std::fs::write(home.path().join("config.toml"), "").expect("empty config");
        let b = Bridge::new().expect("bridge boots from a readable config");
        (home, guard, b)
    }

    /// An op name nobody implements must come back as a message that
    /// names the table, not as a panic and not as silence. Both plugin
    /// languages reach `dispatch` across an FFI boundary where a panic
    /// is either UB or a process abort, so this is the load-bearing
    /// case, not a nicety.
    #[test]
    fn an_unknown_op_names_the_table_it_is_not_in() {
        let (_h, _g, b) = bridge();
        let err = dispatch(&b, "definitely_not_an_op", &[]).expect_err("unknown op must fail");
        assert!(err.contains("definitely_not_an_op"), "{err}");
        for name in OP_NAMES {
            assert!(
                err.contains(name),
                "the error must list what this build does have; `{name}` missing from: {err}"
            );
        }
    }

    /// Every op is reachable by its own name. This is the check that
    /// stops `OP_NAMES` from listing something `dispatch` forgot — the
    /// taxonomy test compares the list against Tcl and the ABI, and
    /// would happily agree about an entry that falls through to the
    /// unknown-op arm.
    #[test]
    fn every_name_in_the_table_dispatches_to_something() {
        let (_h, _g, b) = bridge();
        for name in OP_NAMES {
            // Called with NO arguments: ops that need some will answer
            // "usage: …" and ops that need none will answer normally.
            // Either way the one answer that must not appear is the
            // unknown-op message.
            let answer = match dispatch(&b, name, &[]) {
                Ok(s) => s,
                Err(e) => e,
            };
            assert!(
                !answer.contains("unknown veles op"),
                "`{name}` is in OP_NAMES but dispatch has no arm for it"
            );
        }
    }

    /// Arity is checked in the shared layer, so both languages get the
    /// same refusal and the same usage text. Each shape is represented:
    /// exact, ranged, and open-ended.
    #[test]
    fn arity_is_refused_with_the_usage_line() {
        let (_h, _g, b) = bridge();
        let too_many = ["a", "b", "c", "d", "e", "f"].map(String::from).to_vec();
        for (op, args) in [
            ("config_get", vec![]),                     // needs exactly 1
            ("config_get", too_many.clone()),           // exactly 1
            ("history", vec![]),                        // needs 1..=2
            ("history", too_many.clone()),              // 1..=2
            ("http_post", vec!["u".to_string()]),       // needs 2..=3
            ("send_outbound", vec!["irc".to_string()]), // needs exactly 3
            ("note", vec![]),                           // needs 1..=2
            ("task_cancel", vec![]),                    // needs exactly 1
            ("tool_call", vec!["t".to_string()]),       // needs 2..=3
            ("tool_call", too_many.clone()),            // 2..=3
            ("json_get", vec!["{}".to_string()]),       // needs 2..
            ("call_llm", vec!["sys".to_string()]),      // needs 2..=4
            ("usage_totals", too_many),                 // 0..=1
        ] {
            let err = dispatch(&b, op, &args)
                .expect_err(&format!("`{op}` with {} args must be refused", args.len()));
            assert!(
                err.starts_with("usage: veles::"),
                "`{op}` must answer with its usage line, got: {err}"
            );
            assert!(err.contains(op), "the usage line must name the op: {err}");
        }
    }

    /// An empty trailing argument means ABSENT, not "the empty string".
    ///
    /// This is a deliberate change of behaviour from the pre-extraction
    /// shims, and the API stub is the reason: it declares the optionals
    /// as `{provider ""}` / `{model ""}` / `{limit 20}`, so a Tcl
    /// author writing `veles::call_llm $s $u ""` is passing the
    /// documented default. The old code took that literally and
    /// answered `unknown provider: ` — the stub's own default was an
    /// error. Same shape for `usage_totals ""`, which used to ask the
    /// store for a session whose id is the empty string instead of the
    /// global totals.
    #[test]
    fn an_empty_optional_argument_means_absent() {
        let (_h, _g, b) = bridge();

        // No routes are configured here, so call_llm cannot succeed —
        // but WHICH failure it gives is the point. Absent-provider
        // reaches route resolution ("no routes"); a literal empty
        // provider name would have been rejected as unknown first.
        let err = dispatch(
            &b,
            "call_llm",
            &["sys".into(), "user".into(), "".into(), "".into()],
        )
        .expect_err("no routes are configured");
        assert!(
            err.contains("no routes"),
            "an empty provider must be treated as absent, not looked up: {err}"
        );
        assert!(!err.contains("unknown provider"), "{err}");

        // usage_totals with an empty session id is the GLOBAL total,
        // which on a fresh store is a real answer, not an error.
        let global = dispatch(&b, "usage_totals", &[]).expect("global totals");
        let empty_arg = dispatch(&b, "usage_totals", &["".into()]).expect("empty is global");
        assert_eq!(global, empty_arg);

        // history's limit likewise: empty means the default, where the
        // old code answered "limit must be a number".
        let store = b.store_conn().expect("store");
        store.create_session("empties").expect("session");
        let a = dispatch(&b, "history", &["empties".into()]).expect("default limit");
        let c = dispatch(&b, "history", &["empties".into(), "".into()]).expect("empty limit");
        assert_eq!(a, c);
        // A non-numeric limit is still an error — this is not a licence
        // to accept junk.
        assert!(dispatch(&b, "history", &["empties".into(), "soon".into()]).is_err());
    }

    /// `json_get` is pure and is the one op a plugin calls constantly,
    /// so its conventions are pinned here rather than only through Tcl:
    /// a miss is `Ok("")` and not an error, arrays take numeric
    /// indices, strings come back unquoted, and bad JSON IS an error.
    #[test]
    fn json_get_walks_objects_and_arrays_and_misses_quietly() {
        let (_h, _g, b) = bridge();
        let doc = r#"{"a":{"b":[{"c":"deep"},{"c":2}]},"n":null,"t":true}"#.to_string();
        let get = |keys: &[&str]| {
            let mut args = vec![doc.clone()];
            args.extend(keys.iter().map(|k| k.to_string()));
            dispatch(&b, "json_get", &args)
        };
        assert_eq!(get(&["a", "b", "0", "c"]).unwrap(), "deep");
        assert_eq!(get(&["a", "b", "1", "c"]).unwrap(), "2");
        assert_eq!(get(&["t"]).unwrap(), "true");
        // null and every kind of miss are the same empty string.
        assert_eq!(get(&["n"]).unwrap(), "");
        assert_eq!(get(&["nope"]).unwrap(), "");
        assert_eq!(get(&["a", "b", "9", "c"]).unwrap(), "");
        assert_eq!(get(&["a", "b", "notanindex"]).unwrap(), "");
        // Whole subtrees re-serialize rather than stringify oddly.
        assert_eq!(get(&["a", "b", "0"]).unwrap(), r#"{"c":"deep"}"#);
        // Bad JSON is a real error: a plugin that fed us junk should
        // hear about it, not get "" and carry on.
        assert!(dispatch(&b, "json_get", &["not json".into(), "a".into()]).is_err());
    }

    /// Tcl's braces QUOTE a word, so `{"a":1}` written in plugin source
    /// arrives as `"a":1` with the outer braces gone. The lenient parse
    /// exists for that and is shared, which means a C caller gets it
    /// too — harmless, because valid JSON parses on the strict attempt
    /// and never reaches the fallback. Both spellings must work.
    #[test]
    fn brace_stripped_json_still_parses() {
        let (_h, _g, b) = bridge();
        for spelling in [r#"{"k":"v"}"#, r#""k":"v""#] {
            assert_eq!(
                dispatch(&b, "json_get", &[spelling.to_string(), "k".into()]).unwrap(),
                "v",
                "spelling {spelling} must parse"
            );
        }
    }
}
