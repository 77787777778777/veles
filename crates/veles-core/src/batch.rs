//! The `batch` tool — N tool invocations, ONE assistant round-trip.
//!
//! USER via #coven (2026-09-13): *"do we have a batch tool? I noticed
//! qwen and deepseek were sometimes using that to work more efficiently
//! … I meant I noticed them using it to work more efficiently in
//! opencode, so they do make use of that if available/known."* What
//! opencode ships is a real operation batch: the tool is `batch`, its
//! input is a `tool_calls` array. This is the same shape, settled for
//! veles:
//!
//! - **Sequential, in order.** Deterministic order, no races between
//!   mutations, and approvals arrive serially anyway. Parallel WORK
//!   already exists (`delegate_task`'s `tasks` array); the saving here
//!   is ROUND-TRIPS — one assistant turn instead of N is where the
//!   tokens and latency go, and that holds regardless of execution
//!   order.
//! - **Every invocation passes the same gates as a direct call.** The
//!   turn loop expands the envelope and runs each item through the ONE
//!   per-call pipeline (ACL, named-sandbox, read jail, media jail, plan
//!   mode, pre-approval hook, approval, sandbox routing, output cap,
//!   edit ledger) — the item is judged on its resolved TARGET, exactly
//!   the `tool_call`-envelope doctrine. A gate refusal is that item's
//!   RESULT; it does not stop the batch (the model reads all results
//!   and recovers next round — items are independent by contract).
//! - **One combined result row.** Tool results are id-paired with the
//!   model's own call on the provider wire, so N items ride back inside
//!   the one `batch` call's result. Rewriting the assistant row to
//!   carry N synthesized calls would falsify the transcript.
//! - **A cap** (`MAX_INVOCATIONS`), because a batch is one turn of
//!   bounded patience: beyond it the whole call is refused, naming the
//!   cap.
//! - **Some tools may not ride.** See `REFUSED_IN_BATCH` — tools that
//!   park the turn on a human or start future work. Not because the
//!   gates could not judge them, but because their semantics break
//!   under combination: an ask inside a batch commits the model to
//!   items it has not seen the answer to, and a spawn-flood multiplier
//!   (8 items × 3 children) is what the caps exist to stop.
//!
//! The registered [`BatchTool`] exists for its schema and description
//! (the model has to see the contract to use it); its `call` is a
//! REFUSAL. Anything that dispatches it directly — the plugin bridge,
//! a test — would run zero gates, and an ungated batch is worse than
//! no batch: it is a bypass with a friendly name.

use async_trait::async_trait;
use serde_json::Value;

/// How many invocations one `batch` may carry. delegate_task's own
/// parallel cap is 3 because each item is a whole agent; a tool call is
/// cheaper, but the cap is what stops a batch being a flood with one
/// schema entry — every item still pays the output cap and the ledger.
pub const MAX_INVOCATIONS: usize = 8;

/// Tools that may not ride inside a batch, each with its reason. The
/// reason is the item's result, so the model learns the boundary
/// instead of guessing at it.
const REFUSED_IN_BATCH: &[(&str, &str)] = &[
    ("batch", "it would nest batches — one level only"),
    (
        "clarify",
        "it parks the turn on a human; ask your question in your own turn, not inside a batch",
    ),
    (
        "present_plan",
        "it parks the turn on a human; present the plan as your own turn",
    ),
    (
        "delegate_task",
        "it starts subagents and has its own batch mode (`tasks`)",
    ),
    (
        "cronjob",
        "it schedules future work; schedule jobs serially so each is deliberate",
    ),
    ("a2a", "it starts a peer-agent task"),
    ("monitor", "it schedules a future watcher"),
];

/// Why `name` may not ride inside a batch, if it may not.
pub fn refused_reason(name: &str) -> Option<&'static str> {
    REFUSED_IN_BATCH
        .iter()
        .find(|(t, _)| *t == name)
        .map(|(_, why)| *why)
}

/// The tool as the model sees it: schema + description. The turn loop
/// intercepts `batch` before dispatch and expands it; this impl is the
/// defense in depth for every other dispatch path.
pub struct BatchTool;

#[async_trait]
impl crate::tools::Tool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }
    fn description(&self) -> &str {
        "Run several tool invocations in ONE round-trip: `invocations` is an ordered array of \
         {name, arguments}. Sequential, in order — use it for INDEPENDENT calls (read N files, \
         several searches), never for dependent steps where the next call needs the previous \
         result. Every invocation passes the same gates (ACL, plan mode, approvals, sandbox) as \
         a direct call; one refusal does not stop the rest — each item's result comes back \
         numbered, read them all. Arguments must be complete JSON objects. May not contain: \
         batch, clarify, present_plan, delegate_task, cronjob, a2a, monitor."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "invocations": {
                    "type": "array",
                    "maxItems": MAX_INVOCATIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Tool name"},
                            "arguments": {"type": "object", "description": "Arguments as a JSON object"}
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": ["invocations"]
        })
    }
    async fn call(&self, _args: Value) -> String {
        "error: `batch` is expanded by the agent's turn loop, runs every gate on each \
         invocation; this dispatch path runs none, so it refuses. Call `batch` as a normal \
         tool call."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every refused tool names itself and says why — the reason is the
    /// item's RESULT, so a model that tried learns the boundary.
    #[test]
    fn refused_tools_carry_a_reason() {
        for (name, _) in REFUSED_IN_BATCH {
            let why = refused_reason(name).unwrap_or_else(|| panic!("{name} lost its reason"));
            assert!(!why.is_empty(), "{name}: empty reason");
        }
        assert!(refused_reason("read_file").is_none());
        assert!(refused_reason("terminal").is_none());
        assert!(refused_reason("write_file").is_none());
    }

    /// `batch` itself is in the list: nesting is the recursion case, and
    /// the item-time check runs on the RESOLVED name, so an alias or a
    /// casing difference must not slip a batch inside a batch.
    #[test]
    fn batch_itself_may_not_ride() {
        assert!(refused_reason("batch").is_some());
    }
}
