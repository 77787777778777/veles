//! Override points over three of core's subsystems
//! (PLAN-extension-api.md E3): the system prompt, tool dispatch and
//! inbound routing, each a decision a plugin in ANY tier replaces or
//! wraps with `next` — what the R7 layers give a Rust module, for the
//! stable tiers. The turn runner stays Rust-only (it streams events and
//! mutates the history in place; a JSON round trip per event is not
//! viable).
//!
//! Each seam asks [`crate::services::overridden`] first: with nothing
//! above core's default, the seam runs its own code and builds no JSON.

use serde_json::{json, Value};

use crate::services::{OnError, PointDecl};

// ── prompt.build ─────────────────────────────────────────────────────

/// The system prompt a session starts with.
pub const PROMPT: &str = "prompt.build";

pub fn prompt_decl() -> PointDecl {
    PointDecl {
        name: PROMPT.into(),
        summary: "the system prompt a session starts with: core's (after any module \
                  layer) is handed in; answer it changed, or another"
            .into(),
        input: json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "session_id": {"type": ["string", "null"]},
                "principal": {"description": "who the session acts for (null: an owner surface)"},
                "disabled_skills": {"type": "array", "items": {"type": "string"}},
                "prompt": {"type": "string", "description": "the prompt core built"}
            }
        }),
        output: json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {"prompt": {"type": "string"}}
        }),
        on_error: OnError::FallThrough,
        deadline_ms: 10_000,
    }
}

// ── tool.dispatch ────────────────────────────────────────────────────

/// Every tool call a session makes.
pub const TOOL: &str = "tool.dispatch";

pub fn tool_decl() -> PointDecl {
    PointDecl {
        name: TOOL.into(),
        summary: "every tool call a session makes: answer its result, or pass it (or a \
                  changed name and args) to `next`, which runs core's gated dispatch"
            .into(),
        input: json!({
            "type": "object",
            "required": ["session_id", "name", "args", "call_id"],
            "properties": {
                "session_id": {"type": "string"},
                "name": {"type": "string"},
                "args": {"description": "the call's arguments, as the model sent them"},
                "call_id": {"type": "string",
                    "description": "this call's id; core's default needs it, pass the input on to `next` whole"}
            }
        }),
        output: json!({
            "type": "object",
            "required": ["result"],
            "properties": {"result": {"type": "string", "description": "what the model sees"}}
        }),
        on_error: OnError::FallThrough,
        deadline_ms: 600_000,
    }
}

// ── inbound.route ────────────────────────────────────────────────────

/// Every message a platform delivers to the gateway.
pub const INBOUND: &str = "inbound.route";

/// The fields an `inbound.route` override may not change: the ACL
/// resolves the sender after this point, so changing them would be a way
/// to speak as somebody else.
pub const INBOUND_IDENTITY: &[&str] = &["platform", "chat_id", "sender", "sender_id", "route"];

pub fn inbound_decl() -> PointDecl {
    PointDecl {
        name: INBOUND.into(),
        summary: "every message a platform delivers, before the ACL, the commands and the \
                  turn: route it (its text changed or not) or drop it"
            .into(),
        input: json!({
            "type": "object",
            "required": ["platform", "chat_id", "sender", "text"],
            "description": "the inbound message (veles_types::bus::InboundMessage)"
        }),
        output: json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {"enum": ["route", "drop"]},
                "message": {"description": "for `route`: the message; its platform, chat_id, \
                                            sender, sender_id and route must be unchanged"}
            }
        }),
        on_error: OnError::FallThrough,
        deadline_ms: 10_000,
    }
}

// ── core's defaults ──────────────────────────────────────────────────

/// Declare the three points once per process, as CORE's, with core's own
/// answers at the bottom (`services::ensure_core_points` runs this before
/// any lookup or registration).
pub fn ensure_points() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let declared = [
            crate::services::declare_core_point(
                prompt_decl(),
                // The prompt core built is handed in; core's answer is it.
                crate::services::handler(|input, _cx| async move {
                    Ok(json!({ "prompt": input["prompt"] }))
                }),
            ),
            crate::services::declare_core_point(
                tool_decl(),
                crate::services::handler(|input, _cx| tool_default(input)),
            ),
            crate::services::declare_core_point(
                inbound_decl(),
                // Route the message as it came.
                crate::services::handler(|input, _cx| async move {
                    Ok(json!({ "action": "route", "message": input }))
                }),
            ),
        ];
        for r in declared {
            if let Err(e) = r {
                tracing::error!("points: a core point could not be declared: {e}");
            }
        }
    });
}

// ── tool.dispatch: core's default reaches the dispatching registry ───

/// One call handed back to the registry that is dispatching it: the
/// (possibly rewritten) name and args, and where its result goes.
pub(crate) type DispatchRequest = (String, Value, tokio::sync::oneshot::Sender<String>);

type Slots = std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<DispatchRequest>>;

fn dispatch_slots() -> &'static std::sync::Mutex<Slots> {
    static S: std::sync::OnceLock<std::sync::Mutex<Slots>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(Slots::new()))
}

/// A dispatch in flight through `tool.dispatch`: its id (the input's
/// `call_id`) and the requests core's default sends back to it. Dropping
/// it ends the call's slot.
pub(crate) struct DispatchSlot {
    pub id: String,
    pub requests: tokio::sync::mpsc::UnboundedReceiver<DispatchRequest>,
}

impl Drop for DispatchSlot {
    fn drop(&mut self) {
        dispatch_slots()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

/// Open a slot for one dispatch. The id is random: a plugin that sees
/// one call's id cannot guess another's.
pub(crate) fn open_dispatch() -> DispatchSlot {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    dispatch_slots()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.clone(), tx);
    DispatchSlot { id, requests: rx }
}

/// Core's answer to `tool.dispatch`: hand the call (as the layer above
/// passed it) back to the registry dispatching it, whose own gated
/// dispatch runs it, and answer its result.
async fn tool_default(input: Value) -> Result<Value, String> {
    let id = input["call_id"]
        .as_str()
        .ok_or("no `call_id`: pass the input on to `next` whole (its name and args may change)")?;
    let tx = dispatch_slots()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .cloned()
        .ok_or("this tool call is no longer in flight")?;
    let name = input["name"].as_str().ok_or("no tool `name`")?.to_string();
    let (reply, result) = tokio::sync::oneshot::channel();
    tx.send((name, input["args"].clone(), reply))
        .map_err(|_| "the tool call ended".to_string())?;
    let result = result
        .await
        .map_err(|_| "the tool call ended".to_string())?;
    Ok(json!({ "result": result }))
}

/// Is `got` the same identity as `original` for every field an
/// `inbound.route` override may not change? A field absent on one side
/// and null on the other is the same.
pub fn inbound_identity_kept(original: &Value, got: &Value) -> Result<(), String> {
    for f in INBOUND_IDENTITY {
        let a = original.get(*f).unwrap_or(&Value::Null);
        let b = got.get(*f).unwrap_or(&Value::Null);
        if a != b {
            return Err(format!("`{f}` was {a}, the override made it {b}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{handler, purge_owner, register_for, ServiceDecl};
    use std::sync::Arc;

    fn decl(name: &str) -> ServiceDecl {
        ServiceDecl {
            name: name.into(),
            description: "a test's override".into(),
            cap: None,
        }
    }

    /// `prompt.build`: an override wraps core's prompt through `next`,
    /// or replaces it; an answer outside the schema falls through to
    /// core's. The override acts on its own sessions only, so the tests
    /// beside it pass through.
    #[test]
    fn prompt_build_is_wrapped_replaced_and_survives_a_broken_override() {
        let (_home, _env) = crate::testenv::scratch_home();
        let cfg = crate::config::VelesConfig::default();
        let skills = crate::skills::SkillSelection::default();
        let owner = crate::lifecycle::owner_tag("module:pt-prompt");
        register_for(
            owner,
            decl(PROMPT),
            handler(|input, cx| async move {
                match cx.session_id.as_deref() {
                    Some("pt-prompt-wrap") => {
                        let mut v = cx.next(input).await?;
                        let p = format!("{}\n[wrapped]", v["prompt"].as_str().unwrap_or(""));
                        v["prompt"] = json!(p);
                        Ok(v)
                    }
                    Some("pt-prompt-replace") => Ok(json!({"prompt": "only this"})),
                    Some("pt-prompt-broken") => Ok(json!({"nope": 1})),
                    _ => cx.next(input).await,
                }
            }),
        )
        .unwrap();
        let build = |sid: &str| crate::prompt::build_for_session(&cfg, None, &skills, Some(sid));
        let wrapped = build("pt-prompt-wrap");
        assert!(wrapped.ends_with("\n[wrapped]"), "{wrapped}");
        assert!(
            wrapped.len() > "\n[wrapped]".len(),
            "core's prompt is inside"
        );
        assert_eq!(build("pt-prompt-replace"), "only this");
        let broken = build("pt-prompt-broken");
        assert!(
            !broken.is_empty() && !broken.contains("[wrapped]") && broken != "only this",
            "an off-schema answer falls through to core's prompt: {broken}"
        );
        purge_owner(Some(owner));
        assert!(!build("pt-prompt-wrap").contains("[wrapped]"));
    }

    struct Echo;
    #[async_trait::async_trait]
    impl crate::tools::Tool for Echo {
        fn name(&self) -> &str {
            "pt_echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, args: Value) -> String {
            format!("echo {args}")
        }
    }

    /// `tool.dispatch`: an override wraps a call's result through `next`
    /// (core's gated dispatch runs it), answers it itself, or hands
    /// `next` changed args; a purge puts core's dispatch back.
    #[tokio::test]
    async fn tool_dispatch_is_wrapped_answered_and_rewritten_through_next() {
        let mut r = crate::tools::ToolRegistry::new().for_session("pt-tool");
        r.register(Arc::new(Echo));
        let owner = crate::lifecycle::owner_tag("module:pt-tool");
        register_for(
            owner,
            decl(TOOL),
            handler(|input, cx| async move {
                if cx.session_id.as_deref() != Some("pt-tool") {
                    return cx.next(input).await;
                }
                match input["args"]["mode"].as_str() {
                    Some("upper") => {
                        let mut v = cx.next(input).await?;
                        v["result"] = json!(v["result"].as_str().unwrap_or("").to_uppercase());
                        Ok(v)
                    }
                    Some("refuse") => Ok(json!({"result": "refused by the plugin"})),
                    Some("rewrite") => {
                        let mut changed = input.clone();
                        changed["args"] = json!({"mode": "rewritten"});
                        cx.next(changed).await
                    }
                    _ => cx.next(input).await,
                }
            }),
        )
        .unwrap();
        assert_eq!(
            r.dispatch("pt_echo", json!({"mode": "upper"})).await,
            r#"ECHO {"MODE":"UPPER"}"#
        );
        assert_eq!(
            r.dispatch("pt_echo", json!({"mode": "refuse"})).await,
            "refused by the plugin"
        );
        assert_eq!(
            r.dispatch("pt_echo", json!({"mode": "rewrite"})).await,
            r#"echo {"mode":"rewritten"}"#
        );
        purge_owner(Some(owner));
        assert_eq!(
            r.dispatch("pt_echo", json!({"mode": "upper"})).await,
            r#"echo {"mode":"upper"}"#
        );
    }

    #[test]
    fn the_inbound_identity_check_names_the_changed_field() {
        let original = json!({"platform": "irc", "chat_id": "#c", "sender": "a", "text": "x"});
        let same = json!({"platform": "irc", "chat_id": "#c", "sender": "a", "sender_id": null, "text": "y"});
        assert_eq!(inbound_identity_kept(&original, &same), Ok(()));
        let worse = json!({"platform": "irc", "chat_id": "#c", "sender": "a", "sender_id": "irc:owner", "text": "x"});
        let e = inbound_identity_kept(&original, &worse).unwrap_err();
        assert!(e.contains("`sender_id`"), "{e}");
    }
}
