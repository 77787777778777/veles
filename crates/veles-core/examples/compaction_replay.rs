//! Throwaway replay: run the REAL failing session through the new
//! compaction path and report what it would do.
//!
//! Session 20260913_142929_3dfd8c carried 1595k live chars after FOUR
//! compactions (each ~no-op) and reported ~241k prompt tokens — over the
//! 200k warn line, under the 450k bar. This replays /compact against a
//! COPY of state.db using the same pure functions and the same target
//! math compact_impl uses, with a synthetic ~5k summary standing in for
//! the summarizer call (the session's real markers measured 4–6k).

use veles_core::compaction::{
    apply_tail_prunes, choose_cut_forced, estimate_chars, plan_round_drops, plan_tail_prunes,
    CHARS_PER_TOKEN,
};
use veles_core::providers::ChatMessage;
use veles_core::store::Store;

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: compaction_replay <state.db> [session_id]");
    let sid = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "20260913_142929_3dfd8c".to_string());
    let store = Store::open(std::path::Path::new(&db)).expect("open db");
    let history = store.load_history(&sid).expect("load history");

    let replay_reasoning = false; // the deployed posture; reasoning is not on the wire
    let before = estimate_chars(&history, replay_reasoning);
    println!("session {sid}");
    println!(
        "  live rows: {}   chars(content+tool_calls): {}  ≈{} tokens",
        history.len(),
        before,
        before / CHARS_PER_TOKEN
    );

    // ── the same cut compact_forced picks ──
    let (keep_recent, min_head) = (12usize, 6usize);
    let cut = choose_cut_forced(&history, keep_recent, min_head).expect("forced cut");
    println!(
        "  choose_cut_forced → head {cut} rows, tail {}",
        history.len() - cut
    );

    // ── the head becomes a summary (real markers measured 4–6k chars) ──
    let summary = ChatMessage::user(&format!(
        "[CONTEXT COMPACTION — REFERENCE ONLY] {}",
        "s".repeat(5_000)
    ))
    .with_injection("compaction");
    let tail: Vec<ChatMessage> = history[cut..].to_vec();
    let tail_chars = estimate_chars(&tail, replay_reasoning);
    println!(
        "  tail after head-summary: {} rows, {} chars ≈{} tokens",
        tail.len(),
        tail_chars,
        tail_chars / CHARS_PER_TOKEN
    );
    let mut after_head = std::iter::once(summary).chain(tail).collect::<Vec<_>>();
    let after_head_chars = estimate_chars(&after_head, replay_reasoning);

    // ── the target: 33% of the 200k warn line, minus the fixed prefix ──
    // The live deployment's prefix measured ≈6.2k tokens (system prompt +
    // tool schemas; commit cb5e592). Use it, and also report the
    // prefix-free bound so the margin is visible either way.
    let warn = 200_000usize;
    let prefix_tokens = 6_216usize;
    let target_tokens = (warn as f64 * 0.33) as usize - prefix_tokens;
    let target_chars = target_tokens * CHARS_PER_TOKEN;
    println!(
        "  target: 33% of {warn} warn = {} tok − {prefix_tokens} prefix = {target_tokens} tok = {target_chars} chars",
        (warn as f64 * 0.33) as usize
    );

    // ── tier 2: prune ──
    let plan = plan_tail_prunes(&after_head, target_chars, replay_reasoning, keep_recent);
    println!(
        "  tier 2 plan: {} prunes, {} chars saved, reached_target={}",
        plan.prunes.len(),
        plan.chars_saved,
        plan.reached_target
    );
    apply_tail_prunes(&mut after_head, &plan);
    let after_prune = estimate_chars(&after_head, replay_reasoning);

    // ── tier 3: drop complete rounds, ONLY if tier 2 fell short ──
    // Exactly the ordering compact_impl uses: tier 3 is strictly lossier,
    // so it is not reached for when stubbing tool output is enough.
    let drop_plan = if plan.reached_target {
        plan_round_drops(&[], 0, replay_reasoning, keep_recent)
    } else {
        plan_round_drops(&after_head, target_chars, replay_reasoning, keep_recent)
    };
    println!(
        "  tier 3 plan: {} rounds ({} rows), {} chars saved, reached_target={}",
        drop_plan.rounds,
        drop_plan.indices.len(),
        drop_plan.chars_saved,
        drop_plan.reached_target
    );
    let dropped: std::collections::HashSet<usize> = drop_plan.indices.iter().copied().collect();
    let final_history: Vec<ChatMessage> = after_head
        .iter()
        .enumerate()
        .filter(|(i, _)| !dropped.contains(i))
        .map(|(_, m)| m.clone())
        .collect();
    let after = estimate_chars(&final_history, replay_reasoning);

    // ── well-formedness: the request must still be legal ──
    // Every tool result in the FINAL history must still have its declaring
    // call present, and every call must still have its result. A drop that
    // broke this would be a provider-level error, not a small context loss.
    let mut declared = std::collections::HashSet::new();
    let mut answered = std::collections::HashSet::new();
    for m in &final_history {
        if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
            for c in calls {
                if let Some(id) = c.get("id").and_then(|v| v.as_str()) {
                    declared.insert(id.to_string());
                }
            }
        }
        if let Some(meta) = &m.tool_meta {
            answered.insert(meta.tool_call_id.clone());
        }
    }
    let orphan_calls: Vec<_> = declared.difference(&answered).collect();
    let orphan_results: Vec<_> = answered.difference(&declared).collect();

    println!("── RESULT ──");
    println!(
        "  tier 1 head-summarize : {after_head_chars} chars ≈{} tok",
        after_head_chars / CHARS_PER_TOKEN
    );
    println!(
        "  + tier 2 pruning      : {after_prune} chars ≈{} tok",
        after_prune / CHARS_PER_TOKEN
    );
    println!(
        "  + tier 3 round drops  : {after} chars ≈{} tok",
        after / CHARS_PER_TOKEN
    );
    println!("  target                : {target_chars} chars ≈{target_tokens} tok");
    println!(
        "  rows: {} → {} (dropped {})",
        after_head.len(),
        final_history.len(),
        dropped.len()
    );
    println!(
        "  under target: {}   under warn line ({}tok): {}   reduction vs before: {:.1}x",
        after <= target_chars,
        warn,
        after / CHARS_PER_TOKEN < warn,
        before as f64 / after.max(1) as f64
    );
    println!(
        "  tool_call/result pairing: {} (orphan calls {}, orphan results {})",
        if orphan_calls.is_empty() && orphan_results.is_empty() {
            "INTACT"
        } else {
            "BROKEN"
        },
        orphan_calls.len(),
        orphan_results.len()
    );
    // The old code's outcome, for contrast: head-summary only.
    println!(
        "  OLD behaviour would have stopped at ≈{} tok ({} the warn line)",
        after_head_chars / CHARS_PER_TOKEN,
        if after_head_chars / CHARS_PER_TOKEN >= warn {
            "OVER"
        } else {
            "under"
        }
    );
}
