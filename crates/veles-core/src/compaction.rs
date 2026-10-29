//! Context compression (M10) — keeps long sessions inside the model's
//! context window without losing the thread.
//!
//! Design (hermes-inspired, clean-room):
//!   1. Estimate history size in chars. Above `threshold_chars` and with
//!      enough messages → compress.
//!   2. Pick a CUT POINT: the tail (last ~keep_recent messages) must
//!      start on a `user` message so assistant/tool-call pairings stay
//!      intact (an assistant message with tool_calls must always be
//!      followed by its tool results).
//!   3. Summarize the head via one non-streaming chat call.
//!   4. Archive the head rows in the store (archived=1 — NEVER deleted,
//!      still FTS-searchable) and replace the head with a single
//!      `[CONTEXT COMPACTION]` marker message, both in-memory and
//!      persisted.
//!
//! This module holds the PURE parts (estimation, cut selection,
//! transcript assembly) so they are testable without a live provider.

use crate::providers::ChatMessage;

/// The marker that opens every compaction summary — surfaces and
/// session_search can recognize compacted sessions by it.
pub const COMPACTION_MARKER: &str =
    "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted into the summary below.";

/// Rough char budget per model context (chars ≈ 4×tokens).
pub const CHARS_PER_TOKEN: usize = 4;

pub fn estimate_chars(history: &[ChatMessage], replay_reasoning: bool) -> usize {
    history
        .iter()
        .map(|m| {
            // CHARS, not bytes: the budget it feeds is a char budget,
            // and multibyte text made the estimate overstate by the
            // UTF-8 expansion factor. And tool_calls COUNT — they are
            // real prompt payload (name + arguments JSON), measured at
            // 47% of one large live history, and the old estimate
            // ignored them entirely.
            let mut n = m.content.chars().count();
            // Reasoning is replayed to the provider ONLY on routes with
            // `replay_reasoning` set. A route that strips thinking on
            // send must not have its estimate inflated by thinking that
            // never reaches the wire — otherwise the compaction trigger
            // fires early, against a smaller real prompt (2026-09-12,
            // xenia item 2). The reasoning stays in the store either
            // way; this only decides whether it counts as SENT bytes.
            if replay_reasoning {
                n += m.reasoning.as_ref().map(|r| r.chars().count()).unwrap_or(0);
            }
            if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                for c in calls {
                    let f = c.get("function");
                    n += f
                        .and_then(|f| f.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .map(|s| s.chars().count())
                        .unwrap_or(0);
                    n += f
                        .and_then(|f| f.get("arguments"))
                        .and_then(serde_json::Value::as_str)
                        .map(|s| s.chars().count())
                        .unwrap_or(0);
                }
            }
            n
        })
        .sum()
}

/// History char budget from a context window: min(window_tokens ×
/// trigger percent, max_tokens) × chars/token. Percent is clamped to
/// 0–100 (0 → compaction disabled); `max_tokens` caps the bar so a
/// route that declares a huge window cannot price compaction out of
/// existence (0 = uncapped escape hatch).
pub fn budget_chars(context_tokens: u64, trigger_percent: f64, max_tokens: u64) -> usize {
    let pct = trigger_percent.clamp(0.0, 100.0) / 100.0;
    let window_budget = (context_tokens as f64 * pct) as u64;
    let tokens = if max_tokens > 0 {
        window_budget.min(max_tokens)
    } else {
        window_budget
    };
    (tokens as f64 * CHARS_PER_TOKEN as f64) as usize
}

/// Choose the index where the KEPT tail begins.
///
/// Rules:
///   - the tail keeps at most `keep_recent` messages
///   - the tail must start on a `user` message (tool-round integrity)
///   - the head must retain at least `min_head` messages to be worth
///     summarizing (otherwise return None = skip compression)
///   - the newest user message (the current turn) is always in the tail
pub fn choose_cut(history: &[ChatMessage], keep_recent: usize, min_head: usize) -> Option<usize> {
    choose_cut_impl(history, keep_recent, min_head, false)
}

/// The same, but for a FORCED manual `/compact`: when no clean user
/// boundary exists in the summarizable head (a single long agent turn —
/// one user prompt followed by many tool/assistant rows), fall back to
/// a straight keep_recent cut instead of returning None — SNAPPED
/// forward off any tool round it would otherwise split
/// (`snap_cut_off_tool_round`), because an orphaned tool result is a
/// provider-level 400 on every later request, not a nice-to-have
/// (session 20260926_020336_18bd3e bricked exactly this way). An
/// explicit compact must compact; what it must never do is cut between
/// a call and its result.
pub fn choose_cut_forced(
    history: &[ChatMessage],
    keep_recent: usize,
    min_head: usize,
) -> Option<usize> {
    choose_cut_impl(history, keep_recent, min_head, true)
}

fn choose_cut_impl(
    history: &[ChatMessage],
    keep_recent: usize,
    min_head: usize,
    force: bool,
) -> Option<usize> {
    let n = history.len();
    if n <= min_head + 2 {
        return None;
    }
    let want_start = n.saturating_sub(keep_recent.max(2));
    // Walk BACKWARD to the nearest user message AT OR BEFORE the keep
    // window. The old code walked FORWARD only, so a long agent turn —
    // many tool/assistant rows with no user message among them — found
    // nothing and returned None, silently skipping an explicitly-
    // requested /compact (USER: the compaction did nothing, "1690 →
    // 1690"). Backward guarantees a user boundary when one exists: it
    // lands on the last user turn that BEGAN at or before the cut.
    let mut start = want_start.min(n - 1);
    while start > 0 && history[start].role != "user" {
        start -= 1;
    }
    // No user boundary in the summarizable head (single giant turn):
    // the walk landed on the turn's own prompt at index 0, which is
    // below min_head. Forced? Cut straight at keep_recent. Routine?
    // Skip (tool-round integrity cannot be satisfied).
    if start < min_head {
        if force && want_start >= min_head {
            return Some(snap_cut_off_tool_round(history, want_start));
        }
        return None;
    }
    Some(start)
}

/// A forced cut at `want` must not split a tool round: an assistant row
/// that declares tool_calls must stay on the same side of the cut as the
/// tool rows answering them, or the kept tail opens with an orphaned
/// tool result — a provider-level 400 (`tool_call_id is not found`) on
/// EVERY later request, which is how session 20260926_020336_18bd3e was
/// bricked. That session was one long turn (no user row below the cut),
/// the forced fallback cut at `want_start` raw, and `archive_head`
/// archived the assistant while its result row stayed live.
///
/// Walk FORWARD from `want` to the first index that is not inside a
/// call/result chain. Forward, not backward: the tail keeps the NEWEST
/// rows, so rows before `want` are being archived either way — the only
/// question is how many. Cutting later keeps a few more rows live, which
/// is always safe; cutting earlier would archive rows the caller already
/// decided to keep.
///
/// The invariant a safe cut satisfies: the set of tool_call ids declared
/// BEFORE the cut must equal the set answered before it. We track the
/// running imbalance: entering an assistant row opens its ids, a tool
/// row closes one. The first index at which the open set is EMPTY is the
/// cut.
///
/// Only calls that some row ANSWERS are opened. A call answered nowhere
/// (a crash between the two persists, an in-flight round) has no result
/// to be split from, so it cannot make a cut illegal — and opening it
/// held the imbalance open for the rest of the history, which ran the
/// snap off the end and archived every row, the ones the caller asked
/// to keep included. If the imbalance still never closes (a result that
/// precedes its own call — already-damaged ordering), the cut lands at
/// the end: archiving everything is still well-formed.
fn snap_cut_off_tool_round(history: &[ChatMessage], want: usize) -> usize {
    use std::collections::HashSet;
    let answered: HashSet<&str> = history
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_meta.as_ref())
        .map(|meta| meta.tool_call_id.as_str())
        .collect();
    let mut open: HashSet<String> = HashSet::new();
    let mut cut = 0usize;
    for (i, m) in history.iter().enumerate() {
        match m.role.as_str() {
            "assistant" => {
                if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                    for c in calls {
                        if let Some(id) = c.get("id").and_then(serde_json::Value::as_str) {
                            if answered.contains(id) {
                                open.insert(id.to_string());
                            }
                        }
                    }
                }
            }
            "tool" => {
                if let Some(meta) = &m.tool_meta {
                    open.remove(&meta.tool_call_id);
                }
            }
            _ => {}
        }
        // The earliest index AT OR PAST `want` where nothing is pending.
        // i+1 because the cut is an exclusive bound: rows [0, cut) go.
        if i + 1 >= want && open.is_empty() {
            cut = i + 1;
            break;
        }
    }
    // `want` itself may sit inside a round with an open set that never
    // closes before the end; then cut stays 0 and the caller's
    // `want_start >= min_head` gate already accepted this cut, so fall
    // back to the full length rather than split the round.
    if cut == 0 {
        history.len()
    } else {
        cut
    }
}

/// Render the head as a plain transcript for the summarizer. Each
/// message is capped (tool outputs are huge and mostly noise for a
/// summary) and the whole transcript is capped too.
pub fn build_transcript(head: &[ChatMessage], per_msg_cap: usize, total_cap: usize) -> String {
    let mut out = String::new();
    for m in head {
        let role = match m.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            "tool" => "Tool",
            other => return_unused(other),
        };
        let body = truncate_chars(&m.content, per_msg_cap);
        out.push_str(&format!("{role}: {body}\n"));
        if out.len() > total_cap {
            out.truncate(total_cap);
            out.push_str("\n[transcript truncated]");
            break;
        }
    }
    out
}

fn return_unused(other: &str) -> &'static str {
    // Non-chat roles (system) should not be in the head, but be safe.
    let _ = other;
    "System"
}

fn truncate_chars(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        s.to_string()
    } else {
        // char-boundary safe truncation
        let mut end = cap;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// The summarizer system prompt — hermes-style: preserve goals,
/// decisions, files, constraints, and open work; drop chatter.
pub const SUMMARIZER_SYSTEM: &str = "\
You are compressing an agent conversation so work can continue without \
losing context. Produce a compact handoff summary with these sections: \
Goal, Key decisions, Completed actions (with concrete artifacts/paths), \
Active state (files, processes, config), Open items. Preserve exact file \
paths, command names, identifiers, and quoted user instructions. No \
chatter, no meta-commentary. Markdown headings, terse bullets.";

/// Assemble the final marker message content.
pub fn compaction_message(summary: &str) -> String {
    format!(
        "{COMPACTION_MARKER}\n\n{summary}\n\n--- END OF CONTEXT SUMMARY — \
treat the summary as background reference; the active task continues below."
    )
}

// ── TAIL PRUNING ────────────────────────────────────────────────────
//
// Summarizing the HEAD is not enough for an agent session, and the
// measured proof is session 20260913_142929_3dfd8c: four compactions,
// still 1.6M live chars, because every one of them kept ONE giant turn
// intact (a single user prompt followed by 433 assistant/tool rows —
// `choose_cut` walks back to that prompt and refuses to cut inside the
// turn, correctly, because an assistant tool_call must keep its tool
// results). The same shape recurs across the history: six other sessions
// carry 191–579 rows after their last user message.
//
// MEASURED, on those six adversarial sessions (examples/compaction_replay
// against a copy of the live DB). Head-summarizing alone leaves them at
// 63k–198k tokens — one of them lands within 2k of the warn line, which
// is the exact complaint: "not even enough to go under our 200k token
// warning threshold". Adding tail pruning:
//
//     session            head-only      +pruning   under 66k target?
//     20260822_182915      198,247 tok   117,378       no
//     20260821_201838      157,004        96,929       no
//     20260809_200110      132,238        85,465       no
//     20260823_102044       97,254        78,370       no
//     20260817_231339       78,056        59,408       YES
//     20260822_203720       62,684        59,642       YES
//
// So pruning always clears the warn line (2.4–6.6× reduction) but only
// sometimes reaches the target. The residue is NOT tool output — it is
// the `tool_calls` JSON on the assistant rows (404k chars in
// 20260822_182915, where `patch` and `write_file` arguments alone are
// 178k). That is the honest limit of this lever, and it is why
// `reached_target` is reported rather than assumed.
//
// The lever is the OLD TOOL OUTPUTS. Pruning them shrinks the prompt
// without dropping a single message: every tool row stays (pairing with
// its assistant tool_call intact), only its content is replaced by a
// stub. The original text is NOT lost — `messages_fts` is a standalone
// FTS5 table filled by an AFTER INSERT trigger, so an UPDATE of
// `messages.content` leaves the full text searchable via session_search.
// A test pins exactly that property.
//
// `tool_calls` are deliberately NOT pruned: dropping a tool_call would
// orphan its tool result row and the provider rejects the request. That
// is a structural rewrite, not a prune, and the measurement above shows
// it is not needed to reach the target.

/// Below this size a tool result is left verbatim. Pruning a 300-char
/// output to a ~200-char stub saves almost nothing and destroys real
/// information; the stub only pays for itself on outputs big enough that
/// the model would not want them re-sent every round anyway.
pub const MIN_PRUNABLE_CHARS: usize = 1_000;

/// The text that replaces a pruned tool result. Names the tool and the
/// original size so the model can judge whether to re-run the call, and
/// states where the original still lives.
pub fn prune_stub(tool_name: Option<&str>, original_chars: usize) -> String {
    let who = match tool_name {
        Some(n) if !n.is_empty() => format!("{n} output"),
        _ => "tool output".to_string(),
    };
    format!(
        "[{who} pruned during context compaction — was {original_chars} chars. The original is \
         still in this session's store and findable with session_search; re-run the call if you \
         need it again.]"
    )
}

/// One planned prune inside a tail slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailPrune {
    /// Index into the slice given to `plan_tail_prunes`.
    pub index: usize,
    /// The stub replacing that row's content.
    pub stub: String,
    /// Chars this prune removes from the estimate.
    pub chars_saved: usize,
}

/// A prune plan, and whether it was ENOUGH.
#[derive(Debug, Default)]
pub struct PrunePlan {
    pub prunes: Vec<TailPrune>,
    pub chars_saved: usize,
    /// The plan brought the estimate under the target. `false` means
    /// every prunable row was used and the tail is still over — the
    /// caller must say so plainly instead of reporting success.
    pub reached_target: bool,
}

/// Plan the MINIMAL set of tool-output prunes that brings `tail` under
/// `target_chars`, OLDEST FIRST.
///
/// Oldest-first is the policy, not an accident of iteration: the newest
/// tool outputs are what the model is working from right now, and the
/// old ones are what the head summary already covers. Minimal means the
/// loop stops the moment the target is met, so a session near the bar
/// loses one stale `read_file` rather than its whole history of them.
///
/// `protect_recent` is a FLOOR, not a preference: the last N messages are
/// never candidates at all. Without it a degenerate budget — a fat tool
/// registry eating the whole target, so `target_chars` arrives as 0 —
/// would stub out the tool result the model read one message ago, and the
/// turn would continue with no idea what it had just found out. Honesty
/// beats that: when the protected tail alone exceeds the target the plan
/// returns `reached_target: false` rather than eating live context to
/// hit a number.
///
/// Pure: it plans, `apply_tail_prunes` mutates. Splitting them keeps the
/// interesting decisions (which rows, how many, is it enough) testable
/// without a store or a provider.
pub fn plan_tail_prunes(
    tail: &[ChatMessage],
    target_chars: usize,
    replay_reasoning: bool,
    protect_recent: usize,
) -> PrunePlan {
    let mut est = estimate_chars(tail, replay_reasoning);
    let mut plan = PrunePlan::default();
    if est <= target_chars {
        // Already fits. An empty plan that reports success is the honest
        // answer — there is nothing to give up.
        plan.reached_target = true;
        return plan;
    }
    // The protected boundary: indices at or beyond this are off limits.
    let protect_from = tail.len().saturating_sub(protect_recent);
    for (i, m) in tail.iter().enumerate() {
        if i >= protect_from {
            // Stop rather than continue: we iterate oldest-first, so
            // everything from here on is protected too.
            break;
        }
        if m.role != "tool" {
            continue;
        }
        let chars = m.content.chars().count();
        if chars < MIN_PRUNABLE_CHARS {
            continue;
        }
        let stub = prune_stub(m.tool_meta.as_ref().map(|t| t.name.as_str()), chars);
        let stub_chars = stub.chars().count();
        // Never "prune" something into being longer.
        if stub_chars >= chars {
            continue;
        }
        let saved = chars - stub_chars;
        est -= saved;
        plan.chars_saved += saved;
        plan.prunes.push(TailPrune {
            index: i,
            stub,
            chars_saved: saved,
        });
        if est <= target_chars {
            plan.reached_target = true;
            break;
        }
    }
    plan
}

/// Apply a plan in place. The message COUNT and ORDER do not change —
/// that is the whole point: tool rows stay paired with the assistant
/// tool_calls that produced them, so the request stays well-formed.
pub fn apply_tail_prunes(tail: &mut [ChatMessage], plan: &PrunePlan) {
    for p in &plan.prunes {
        if let Some(m) = tail.get_mut(p.index) {
            m.content = p.stub.clone();
        }
    }
}

// ── TIER 3: ROUND DROPPING ──────────────────────────────────────────
//
// Pruning tool OUTPUTS is not always enough, and the measurement says why:
// in session 20260822_182915_67f308 the tail holds 404k chars of
// `tool_calls` JSON on assistant rows (`patch` and `write_file` arguments
// alone are 178k). Stubbing tool content leaves that untouched, so the
// session lands at 117k tokens — under the warn line, but not under the
// target. `tool_calls` cannot be stubbed in place: an assistant row that
// declares a call MUST be followed by that call's result, so shrinking
// one side without the other produces a malformed request.
//
// The lever that reaches the target is therefore the one claude-code and
// opencode use: drop COMPLETE OLD ROUNDS out of the live history and let
// the summary carry them. "Drop" here means archive — `archived = 1`, the
// row stays in the store and stays FTS-searchable, exactly like the head
// rows this module has always archived. Nothing is deleted.
//
// A round is droppable only if it is EXACTLY self-contained: every tool
// result in it answers a call declared in it, and every call declared in
// it has its result in it. Either direction broken means dropping would
// orphan a row, and an orphaned tool result is a provider-level error, so
// such a group is skipped rather than risked.

/// A plan of whole rounds to archive out of the live history.
#[derive(Debug, Default)]
pub struct RoundDropPlan {
    /// Indices into the slice given to `plan_round_drops`, ascending.
    pub indices: Vec<usize>,
    /// How many complete rounds that covers.
    pub rounds: usize,
    pub chars_saved: usize,
    /// The plan brought the estimate under the target. `false` means
    /// every droppable round was used and it is still over.
    pub reached_target: bool,
}

/// Plan the MINIMAL set of whole rounds to archive so `tail` fits under
/// `target_chars`, OLDEST FIRST.
///
/// Three rules, each load-bearing:
///   - only complete, self-contained rounds are candidates (see above);
///   - `user` rows are NEVER dropped. The conversation's spine — what the
///     human actually asked for — is the last thing to give up, and it is
///     also cheap (measured: 0–11k chars across the adversarial
///     sessions, against 400k+ of tool payload). This is also what keeps
///     the compaction marker, the todo re-injection and the hook context
///     in place, since those are user-role rows.
///   - the newest `protect_recent` messages are off limits, the same floor
///     `plan_tail_prunes` honours: the model is reasoning over those now.
pub fn plan_round_drops(
    tail: &[ChatMessage],
    target_chars: usize,
    replay_reasoning: bool,
    protect_recent: usize,
) -> RoundDropPlan {
    let mut est = estimate_chars(tail, replay_reasoning);
    let mut plan = RoundDropPlan::default();
    if est <= target_chars {
        plan.reached_target = true;
        return plan;
    }
    let protect_from = tail.len().saturating_sub(protect_recent);

    // Candidates are minimal BALANCED UNITS, not whole assistant/tool runs.
    //
    // That distinction is the difference between a useful tier and a
    // useless one. The shape this exists for — one user prompt followed by
    // hundreds of tool rows — is a SINGLE run: treating the run as the unit
    // makes dropping all-or-nothing, so a tail one row over the target
    // would either keep everything or throw the whole turn away (and
    // throwing it away would include the protected recent window, which is
    // forbidden). Partitioning into balanced call/result units makes the
    // drop granular: the oldest rounds go, the newest stay.
    //
    // A `user` row always ends a run and is never a candidate.
    let mut i = 0usize;
    while i < protect_from {
        if tail[i].role == "user" {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < protect_from && tail[i].role != "user" {
            i += 1;
        }
        // Split the run, but never look past the protection boundary: a
        // unit whose result row lives inside the protected window is
        // unbalanced within this range and so is not a candidate. That is
        // precisely the orphan-the-result case.
        for (start, end) in balanced_units(&tail[run_start..i]) {
            let (start, end) = (run_start + start, run_start + end);
            // Addressability before structure: a unit is only droppable if
            // EVERY row in it can be archived by id. An in-memory row that
            // was never persisted has no id, and dropping it from the live
            // history without archiving would delete context outright —
            // the one thing this module never does. Skipping leaves it in
            // place, which is the safe direction.
            if (start..end).any(|k| tail[k].msg_id.is_none()) {
                continue;
            }
            if self_contained_round(&tail[start..end]).is_none() {
                // Orphaned in either direction: skipping is the safe answer.
                continue;
            }
            let saved: usize = (start..end)
                .map(|k| estimate_chars(&tail[k..k + 1], replay_reasoning))
                .sum();
            // A unit that saves nothing is not worth the structural risk.
            if saved == 0 {
                continue;
            }
            est -= saved;
            plan.chars_saved += saved;
            plan.rounds += 1;
            plan.indices.extend(start..end);
            if est <= target_chars {
                plan.reached_target = true;
                return plan;
            }
        }
    }
    plan
}

/// Partition a run of assistant/tool rows into MINIMAL balanced units, as
/// `(start, end)` offsets into `run`.
///
/// A unit is balanced when every call id declared inside it is answered
/// inside it and vice versa. The scan cuts at the first balance point after
/// each unit starts, which is what makes units minimal — and minimality is
/// what lets the caller drop the oldest rounds while keeping the newest.
///
/// A trailing remainder that never balances becomes one final unit; it will
/// fail `self_contained_round` and be skipped by the caller. That is the
/// conservative direction: a run that starts with an orphaned tool result
/// (which `choose_cut_forced` can produce by cutting mid-round) blocks its
/// own remainder rather than risking an orphan.
fn balanced_units(run: &[ChatMessage]) -> Vec<(usize, usize)> {
    use std::collections::HashSet;
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut declared: HashSet<String> = HashSet::new();
    let mut answered: HashSet<String> = HashSet::new();
    for (off, m) in run.iter().enumerate() {
        match m.role.as_str() {
            "assistant" => {
                if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                    for c in calls {
                        if let Some(id) = c.get("id").and_then(serde_json::Value::as_str) {
                            declared.insert(id.to_string());
                        }
                    }
                }
            }
            "tool" => {
                if let Some(meta) = &m.tool_meta {
                    answered.insert(meta.tool_call_id.clone());
                }
            }
            // Cannot happen (the caller splits on user rows); treat any
            // other role as a hard boundary rather than absorb it.
            _ => {
                out.push((start, off));
                start = off + 1;
                declared.clear();
                answered.clear();
                continue;
            }
        }
        if declared == answered {
            out.push((start, off + 1));
            start = off + 1;
            declared.clear();
            answered.clear();
        }
    }
    if start < run.len() {
        out.push((start, run.len()));
    }
    out
}

/// Is this contiguous run of assistant/tool rows EXACTLY self-contained?
///
/// Both directions are checked, and both matter:
///   - a tool result whose call was declared OUTSIDE the group means
///     dropping the group orphans nothing but leaves a result with no
///     caller (the assistant that called it was already dropped, or sits
///     in the protected window) — malformed either way;
///   - a call declared inside the group whose result sits OUTSIDE it
///     (typically in the protected recent window) means dropping the
///     group orphans that result. This is the easy one to miss, because
///     it only happens at the group/protected boundary.
///
/// Returns the round's size in rows on success, `None` if it is not safe
/// to drop as a unit.
fn self_contained_round(group: &[ChatMessage]) -> Option<usize> {
    use std::collections::HashSet;
    let mut declared: HashSet<String> = HashSet::new();
    let mut answered: HashSet<String> = HashSet::new();
    for m in group {
        match m.role.as_str() {
            "assistant" => {
                if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                    for c in calls {
                        if let Some(id) = c.get("id").and_then(serde_json::Value::as_str) {
                            declared.insert(id.to_string());
                        }
                    }
                }
            }
            "tool" => {
                if let Some(meta) = &m.tool_meta {
                    answered.insert(meta.tool_call_id.clone());
                } else {
                    // A tool row with no identity cannot be proven paired.
                    return None;
                }
            }
            // A user row cannot be here (the caller splits on it), and a
            // system row has no business in a droppable round.
            _ => return None,
        }
    }
    if declared != answered {
        return None;
    }
    Some(group.len())
}

/// What a compaction actually DID, for reporting.
///
/// The old `Option<(chars_before, chars_after)>` could not tell the user
/// WHY a compaction barely helped, which is exactly the complaint that
/// started this work: "compacted: 514 → 433 messages" said nothing about
/// why, and "nothing to compact" was indistinguishable from "compacted
/// hard but the tail was untouchable". These fields let every surface say
/// the true thing.
#[derive(Debug, Clone, Default)]
pub struct CompactOutcome {
    /// History chars before the compaction.
    pub chars_before: usize,
    /// History chars after, including the summary and the pruned stubs.
    pub chars_after: usize,
    /// Head rows archived (summarized away) — the oldest-N prefix that
    /// `choose_cut` selected.
    pub archived: usize,
    /// Old tool outputs replaced by stubs. Lossless for structure, and the
    /// originals stay FTS-searchable.
    pub pruned: usize,
    /// Chars the prunes removed.
    pub prune_chars_saved: usize,
    /// Tail rows archived as whole self-contained rounds (tier 3). Only
    /// used when pruning provably cannot reach the target, because the
    /// residue is `tool_calls` JSON that cannot be stubbed without
    /// orphaning its result. These went INTO the summary first.
    pub dropped_rows: usize,
    /// How many complete rounds that was — the number a human reads.
    pub dropped_rounds: usize,
    /// The compaction landed at or under its target. `false` means every
    /// tier was used and the history is still over — surfaced honestly
    /// rather than reported as a success.
    pub reached_target: bool,
    /// The target it was aiming at, in chars, for the report.
    pub target_chars: usize,
}

impl CompactOutcome {
    /// The one-line reply every surface gives for `/compress`, `/compact`
    /// and the automatic note.
    ///
    /// One formatter, four surfaces (CLI/REPL, gateway, TUI, desktop): the
    /// old per-surface strings were exactly what let "compacted: 514 → 433
    /// messages" pass for a success story while the session stayed over
    /// the warn line. This one says what was archived, what was pruned,
    /// what was dropped, and — when the target was missed — that it was
    /// missed.
    pub fn report(&self, messages_before: usize, messages_after: usize) -> String {
        let prune = if self.pruned > 0 {
            format!(
                ", {} stale tool output{} stubbed ({} chars, originals still searchable)",
                self.pruned,
                if self.pruned == 1 { "" } else { "s" },
                human_chars(self.prune_chars_saved)
            )
        } else {
            String::new()
        };
        let dropped = if self.dropped_rows > 0 {
            format!(
                ", {} complete round{} ({} rows) folded into the summary",
                self.dropped_rounds,
                if self.dropped_rounds == 1 { "" } else { "s" },
                self.dropped_rows
            )
        } else {
            String::new()
        };
        let verdict = if self.reached_target {
            String::new()
        } else if self.target_chars == 0 {
            // Degenerate but reachable: the fixed per-round prefix
            // (system prompt + tool schemas) alone already exceeds
            // target_percent of the warn line, so history's share of the
            // budget is zero. Saying "still over the 0-char target" would
            // be noise; the actionable fact is that no history budget
            // exists at these settings.
            " — the fixed prompt prefix alone already exceeds the compaction target, so no \
             history budget is left (raise agent.compaction.warn_context_tokens or \
             target_percent, or start /new)"
                .to_string()
        } else {
            // All three tiers ran: head summarized, tool outputs stubbed,
            // complete rounds folded in. What is left is the protected
            // recent window plus any round that was not self-contained.
            format!(
                " — still over the {}-char target after every tier ran; what remains is the \
                 protected recent window (agent.compaction.keep_recent) plus rounds that \
                 cannot be dropped without orphaning a tool result (try /new)",
                human_chars(self.target_chars)
            )
        };
        format!(
            "compacted: {messages_before} → {messages_after} messages, {} → {} chars ({} head rows archived{}{}){}",
            human_chars(self.chars_before),
            human_chars(self.chars_after),
            self.archived,
            prune,
            dropped,
            verdict,
        )
    }
}

/// "86000" -> "86k". The ONE humaniser for context sizes: engine notes,
/// the `/compress` reply, and `/context` all format through it so the
/// same number is never worded two different ways.
pub(crate) fn human_chars(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// When the AUTOMATIC compaction may try again after its summarizer failed.
///
/// A failed summary leaves the history as it was, so the next turn is still
/// over the bar and tries again — and if the cause is still there it fails
/// again. Every summary through the Anthropic driver timed out for a whole
/// afternoon (2026-10-29): each turn waited 30 s for a summary that never
/// came, then sent the full 500k-token history anyway. After a failure the
/// automatic path waits 2, 4, 8, 16, then 30 minutes between attempts. A
/// forced `/compact` is the user asking, so it is never held back, and a
/// success clears the count.
#[derive(Debug, Default)]
pub struct SummarizerBackoff {
    failures: u32,
    retry_at: Option<std::time::Instant>,
    /// The failure the user has not been told about yet.
    unreported: Option<String>,
}

impl SummarizerBackoff {
    /// The wait after `failures` consecutive failures.
    pub fn delay(failures: u32) -> std::time::Duration {
        let minutes = match failures {
            0 => 0,
            n => (1u64 << n.min(5)).min(30),
        };
        std::time::Duration::from_secs(minutes * 60)
    }

    /// How long the automatic path must still wait, if at all.
    pub fn waiting(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.retry_at
            .filter(|at| *at > now)
            .map(|at| at.duration_since(now))
    }

    /// Record a failure; answers the wait before the next attempt.
    pub fn failed(&mut self, now: std::time::Instant, why: String) -> std::time::Duration {
        self.failures = self.failures.saturating_add(1);
        let wait = Self::delay(self.failures);
        self.retry_at = Some(now + wait);
        self.unreported = Some(why);
        wait
    }

    pub fn succeeded(&mut self) {
        *self = Self::default();
    }

    /// The failure to tell the user about, once.
    pub fn take_unreported(&mut self) -> Option<String> {
        self.unreported.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The automatic path backs off after a failed summary and a success
    /// resets it: without this a summarizer that keeps failing is retried
    /// at the start of every turn, each one stalling for the failed call
    /// before sending the full history anyway (2026-10-29).
    #[test]
    fn a_failed_summary_backs_off_and_a_success_resets() {
        let t0 = std::time::Instant::now();
        let mut b = SummarizerBackoff::default();
        assert_eq!(b.waiting(t0), None, "nothing has failed yet");
        let first = b.failed(t0, "timed out".into());
        assert_eq!(first, std::time::Duration::from_secs(120));
        assert!(b.waiting(t0).is_some(), "the next turn must not try again");
        assert_eq!(
            b.waiting(t0 + first),
            None,
            "it may try again once the wait is over"
        );
        assert_eq!(b.take_unreported().as_deref(), Some("timed out"));
        assert_eq!(b.take_unreported(), None, "the user hears it once");
        let second = b.failed(t0 + first, "timed out".into());
        assert!(second > first, "a second failure waits longer");
        for _ in 0..10 {
            b.failed(t0, "x".into());
        }
        assert_eq!(
            SummarizerBackoff::delay(12),
            std::time::Duration::from_secs(30 * 60),
            "the wait is capped"
        );
        b.succeeded();
        assert_eq!(b.waiting(t0), None, "a success clears the back-off");
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
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

    #[test]
    fn estimate_counts_content_reasoning_and_tool_calls() {
        let mut m = msg("user", "hello world"); // 11
        m.reasoning = Some("12345".into()); // +5 when replayed
        assert_eq!(estimate_chars(&[m.clone()], true), 16);
        assert_eq!(estimate_chars(&[m], false), 11);
        // A tool call is prompt payload like any other: name + args.
        let mut a = msg("assistant", "");
        a.tool_calls = Some(serde_json::json!([{
            "id": "c1",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": "{\"path\":\"/tmp/x\"}"
            }
        }]));
        // 9 (name) + 17 (arguments) = 26 — the old estimate scored
        // this message ZERO.
        assert_eq!(estimate_chars(&[a.clone()], true), 26);
        assert_eq!(estimate_chars(&[a], false), 26);
    }

    /// Chars, not bytes: a char budget fed with byte counts overstates
    /// multibyte text by the UTF-8 expansion factor and compacts early.
    #[test]
    fn estimate_counts_chars_not_bytes() {
        // 10 × 'é' = 20 UTF-8 bytes, 10 chars.
        assert_eq!(estimate_chars(&[msg("user", "éééééééééé")], false), 10);
    }

    /// The reasoning term is a REPLAY question, not a storage one: a
    /// route that drops thinking on send must not trigger compaction on
    /// characters the model will never see again.
    #[test]
    fn reasoning_only_counts_when_replayed() {
        let mut m = msg("assistant", "answer"); // 6
        m.reasoning = Some("aaaaaaaaaa".into()); // 10
                                                 // Replay off: the model is sent 6 chars, the estimate says 6.
        assert_eq!(estimate_chars(&[m.clone()], false), 6);
        // Replay on: the thinking is on the wire too, so 16.
        assert_eq!(estimate_chars(&[m], true), 16);
    }

    #[test]
    fn budget_is_percent_of_window() {
        // 1000 tokens × 4 chars × 85% = 3400
        assert_eq!(budget_chars(1_000, 85.0, 0), 3_400);
        // clamping: over 100 caps at the full window
        assert_eq!(budget_chars(1_000, 250.0, 0), 4_000);
        // zero percent disables
        assert_eq!(budget_chars(1_000, 0.0, 0), 0);
    }

    #[test]
    fn budget_caps_at_max_context_tokens() {
        // The bill's bug: a route declares a 1M-token window, so 85% of
        // it (850k tokens) can never fire on any real session. The cap
        // must win: min(850k, 450k) = 450k tokens × 4 = 1.8M chars.
        assert_eq!(budget_chars(1_000_000, 85.0, 450_000), 1_800_000);
        // A window under the cap is untouched.
        assert_eq!(budget_chars(128_000, 85.0, 450_000), 128_000 * 4 * 85 / 100);
        // 0 = uncapped escape hatch.
        assert_eq!(budget_chars(1_000_000, 85.0, 0), 3_400_000);
    }

    /// The mid-turn valve must be STRICTER than the turn-start check:
    /// a higher percent means history has to grow further before the
    /// rewrite fires, so routine compactions never land mid-turn.
    #[test]
    fn mid_turn_budget_is_stricter_than_turn_start_budget() {
        let turn_start = budget_chars(128_000, 85.0, 450_000);
        let mid_turn = budget_chars(128_000, 97.0, 450_000);
        assert!(
            mid_turn > turn_start,
            "mid-turn budget {mid_turn} must exceed the turn-start budget {turn_start}"
        );
        // …and a history that trips the routine check must NOT trip the
        // valve: 90% full is between the two.
        let ninety_pct = (128_000.0 * 4.0 * 0.90) as usize;
        assert!(ninety_pct >= turn_start, "routine check fires at 90%");
        assert!(ninety_pct < mid_turn, "mid-turn valve does not");
    }

    #[test]
    fn cut_starts_on_user_message() {
        // u a u a u a u a — keep 3 → start walks to the user at index 4
        let h = vec![
            msg("user", "1"),
            msg("assistant", "2"),
            msg("user", "3"),
            msg("assistant", "4"),
            msg("user", "5"),
            msg("assistant", "6"),
            msg("user", "7"),
            msg("assistant", "8"),
        ];
        let cut = choose_cut(&h, 3, 2).unwrap();
        assert_eq!(h[cut].role, "user");
        assert!(cut >= 2);
    }

    #[test]
    fn skip_when_head_too_small() {
        let h = vec![msg("user", "a"), msg("assistant", "b"), msg("user", "c")];
        assert!(choose_cut(&h, 2, 4).is_none());
    }

    #[test]
    fn a_long_agent_turn_with_no_user_in_the_tail_still_cuts() {
        // A big agent turn: user prompt, then MANY tool/assistant rows,
        // no further user message. keep_recent=12 would put the cut deep
        // inside the tool rows; the old forward-walk found nothing and
        // returned None, silently skipping an explicit /compact.
        let mut h = vec![msg("user", "go do the thing")];
        for i in 0..20 {
            h.push(msg(
                if i % 2 == 0 { "assistant" } else { "tool" },
                &format!("row {i}"),
            ));
        }
        // FORCED cut must not skip even with no clean user boundary.
        let cut = choose_cut_forced(&h, 12, 3).expect("forced compact must cut");
        assert!(cut > 0, "head is not empty");
        assert!(cut < h.len(), "cut is inside the history");
        // Routine (non-forced) path is still allowed to skip here —
        // that's the point: manual forces through, automatic respects
        // tool-round integrity.
        assert_eq!(choose_cut(&h, 12, 3), None);
    }

    /// An assistant row declaring a real tool call, paired with its
    /// result — the shape the forced cut must NEVER split.
    fn paired_call(id: &str) -> ChatMessage {
        let mut m = msg("assistant", "");
        m.tool_calls = Some(serde_json::json!([{
            "id": id,
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]));
        m
    }

    fn paired_result(id: &str, content: &str) -> ChatMessage {
        let mut m = msg("tool", content);
        m.tool_meta = Some(crate::providers::ToolMeta {
            tool_call_id: id.into(),
            name: "read_file".into(),
        });
        m
    }

    /// THE 18bd3e BUG: session 20260926_020336_18bd3e was one long turn
    /// (user prompt, then ~80 assistant/tool rows, no user boundary). A
    /// forced /compact cut at the raw `want_start` index — right between
    /// an assistant's tool_call and its result — and archive_head
    /// archived the assistant. The live history then OPENED with an
    /// orphaned tool result, the provider answered
    /// `400 tool_call_id is not found` on every request, and the session
    /// was unrecoverable. The forced cut must snap forward to the next
    /// point where no call/result pair straddles the line.
    #[test]
    fn forced_cut_never_splits_a_tool_round() {
        let mut h = vec![msg("user", "go do the thing")];
        // 20 paired rounds. want_start for keep_recent=12 lands at
        // 41-12+1: inside the tool rows, plausibly mid-round.
        for i in 0..20 {
            let id = format!("call_{i}");
            h.push(paired_call(&id));
            h.push(paired_result(&id, &format!("output {i}")));
        }
        for keep in [12usize, 11, 13, 10] {
            let cut = choose_cut_forced(&h, keep, 3).expect("must cut");
            // What the tail must satisfy on EVERY keep: no tool result
            // whose declaring call is on the archived side, and no
            // archived call whose result is on the live side.
            let (head, tail) = (&h[..cut], &h[cut..]);
            let mut head_calls = std::collections::HashSet::new();
            let mut head_results = std::collections::HashSet::new();
            for m in head {
                if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                    for c in calls {
                        head_calls.insert(c.get("id").unwrap().as_str().unwrap().to_string());
                    }
                }
                if let Some(meta) = &m.tool_meta {
                    head_results.insert(meta.tool_call_id.clone());
                }
            }
            let mut tail_calls = std::collections::HashSet::new();
            let mut tail_results = std::collections::HashSet::new();
            for m in tail {
                if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                    for c in calls {
                        tail_calls.insert(c.get("id").unwrap().as_str().unwrap().to_string());
                    }
                }
                if let Some(meta) = &m.tool_meta {
                    tail_results.insert(meta.tool_call_id.clone());
                }
            }
            assert!(
                head_calls.is_disjoint(&tail_results),
                "keep={keep}: archived call answered by a LIVE result (orphan)"
            );
            assert!(
                tail_calls.is_disjoint(&head_results),
                "keep={keep}: live call answered by an ARCHIVED result"
            );
            assert_eq!(
                head_calls, head_results,
                "keep={keep}: head must answer everything it declares"
            );
            assert_eq!(
                tail_calls, tail_results,
                "keep={keep}: tail must answer everything it declares"
            );
        }
    }

    /// A call right at the cut whose result row does not exist (the cut
    /// is requested mid-flight, or the history is already damaged) has
    /// nothing to be split from: the cut stays where the caller asked,
    /// and the rows it asked to keep stay live.
    #[test]
    fn forced_cut_at_an_unanswered_call_keeps_what_was_asked() {
        let mut h = vec![msg("user", "go")];
        h.push(paired_call("call_open")); // never answered, right at want
        h.push(msg("assistant", "still thinking"));
        h.push(msg("assistant", "more"));
        // keep_recent=2 → want_start=2.
        let cut = choose_cut_forced(&h, 2, 1).expect("must cut");
        assert_eq!(cut, 2, "an unanswered call does not move the cut");
    }

    /// The degenerate end that remains: a result that PRECEDES its own
    /// call. The imbalance that call opens never closes, the snap runs
    /// out of history, and archiving everything is the only well-formed
    /// answer — the caller's head becomes the summary.
    #[test]
    fn forced_cut_over_a_result_before_its_call_takes_all() {
        let mut h = vec![msg("user", "go")];
        h.push(paired_result("call_x", "early"));
        h.push(paired_call("call_x"));
        h.push(msg("assistant", "still thinking"));
        h.push(msg("assistant", "more"));
        let cut = choose_cut_forced(&h, 2, 1).expect("must cut");
        assert_eq!(cut, h.len(), "unclosable imbalance archives everything");
    }

    /// A call that is answered NOWHERE (a crash between the two persists,
    /// long before the cut) cannot be split from a result that does not
    /// exist — it must not hold the running imbalance open for the rest
    /// of the history. It used to: `open` never emptied, the snap ran off
    /// the end, and the forced compact archived EVERY row, including the
    /// recent ones the caller asked to keep.
    #[test]
    fn a_never_answered_call_does_not_drag_the_cut_to_the_end() {
        let mut h = vec![msg("user", "go"), paired_call("call_dangling")];
        for i in 0..10 {
            let id = format!("call_{i}");
            h.push(paired_call(&id));
            h.push(paired_result(&id, "out"));
        }
        let cut = choose_cut_forced(&h, 4, 1).expect("must cut");
        assert!(
            cut < h.len(),
            "the recent rows stay live: cut {cut} of {}",
            h.len()
        );
        // And the cut it found is still well-formed: no answered round
        // straddles it.
        let head_calls: std::collections::HashSet<String> = h[..cut]
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .filter_map(|v| v.as_array())
            .flatten()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        for m in &h[cut..] {
            if let Some(meta) = &m.tool_meta {
                assert!(
                    !head_calls.contains(&meta.tool_call_id),
                    "live result {} answers an archived call",
                    meta.tool_call_id
                );
            }
        }
    }

    #[test]
    fn transcript_caps_and_truncates() {
        let head = vec![
            msg("user", &"x".repeat(100)),
            msg("assistant", &"y".repeat(100)),
        ];
        let t = build_transcript(&head, 20, 1000);
        assert!(t.contains("User: xxxxxxxxxxxxxxxxxxxx…"));
        assert!(t.contains("Assistant: yyyyyyyyyyyyyyyyyyyy…"));

        // total cap kicks in
        let t2 = build_transcript(&head, 100, 30);
        assert!(t2.contains("[transcript truncated]"));
    }

    #[test]
    fn marker_wraps_summary() {
        let m = compaction_message("S");
        assert!(m.starts_with(COMPACTION_MARKER));
        assert!(m.contains("S"));
    }

    // ── tail pruning ────────────────────────────────────────────────

    fn tool_msg(name: &str, content: &str) -> ChatMessage {
        let mut m = msg("tool", content);
        m.tool_meta = Some(crate::providers::ToolMeta {
            tool_call_id: format!("call_{name}"),
            name: name.into(),
        });
        m
    }

    /// A tail already under the target plans NOTHING and reports success.
    /// Giving up information it did not need to give up is a bug.
    #[test]
    fn plan_is_empty_when_tail_already_fits() {
        let tail = vec![
            msg("user", "go"),
            tool_msg("read_file", &"x".repeat(500)), // below MIN_PRUNABLE
        ];
        let plan = plan_tail_prunes(&tail, 10_000, false, 0);
        assert!(plan.prunes.is_empty());
        assert!(plan.reached_target);
        assert_eq!(plan.chars_saved, 0);
    }

    /// The bug this whole change exists for: ONE giant agent turn — a
    /// single user prompt then hundreds of tool rows — is kept intact by
    /// `choose_cut`, so head-summarizing alone cannot shrink it. Pruning
    /// the stale tool outputs does, WITHOUT dropping a message.
    #[test]
    fn a_giant_turn_is_shrunk_by_pruning_not_by_dropping() {
        // ~100 rounds of 5k-char read_file results: ≈500k chars, the
        // shape session 20260913_142929_3dfd8c had (732k of tool content).
        let mut tail = vec![msg("user", "go do the thing")];
        for i in 0..100 {
            tail.push(msg("assistant", "let me look"));
            tail.push(tool_msg("read_file", &format!("blob {i} ").repeat(500)));
        }
        let before = estimate_chars(&tail, false);
        assert!(before > 350_000, "this tail is huge: {before}");
        let count_before = tail.len();

        let target = 50_000;
        let plan = plan_tail_prunes(&tail, target, false, 0);
        assert!(plan.reached_target, "the plan must reach {target}");
        assert!(!plan.prunes.is_empty(), "and it must actually prune");

        apply_tail_prunes(&mut tail, &plan);
        let after = estimate_chars(&tail, false);
        assert!(after <= target, "{before} → {after}, target {target}");
        // Structure untouched: same messages, same order, same pairing.
        assert_eq!(tail.len(), count_before, "no message was dropped");
        assert_eq!(tail[0].role, "user");
    }

    /// Pruning must never break the assistant↔tool pairing: every tool
    /// row survives with its identity, only its CONTENT changes.
    /// CALIBRATION MUTATION: make the planner drop rows instead of
    /// stubbing them and this goes red.
    #[test]
    fn prune_preserves_every_tool_row_and_its_metadata() {
        let mut tail = vec![msg("user", "go")];
        for _ in 0..40 {
            tail.push(msg("assistant", "tool call"));
            tail.push(tool_msg("terminal", &"y".repeat(3_000)));
        }
        let ids_before: Vec<String> = tail
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.tool_meta.as_ref().unwrap().tool_call_id.clone())
            .collect();
        let roles_before: Vec<String> = tail.iter().map(|m| m.role.clone()).collect();

        let plan = plan_tail_prunes(&tail, 5_000, false, 0);
        apply_tail_prunes(&mut tail, &plan);

        let ids_after: Vec<String> = tail
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.tool_meta.as_ref().unwrap().tool_call_id.clone())
            .collect();
        let roles_after: Vec<String> = tail.iter().map(|m| m.role.clone()).collect();
        assert_eq!(roles_before, roles_after, "roles and order unchanged");
        assert_eq!(ids_before, ids_after, "every tool_call_id still present");
    }

    /// MINIMAL: the plan stops as soon as the target is met, pruning
    /// OLDEST FIRST, so the newest outputs (what the model is working
    /// from now) survive. CALIBRATION MUTATION: prune newest-first and
    /// the surviving-row assertion goes red.
    #[test]
    fn plan_is_minimal_and_prunes_oldest_first() {
        let mut tail = vec![msg("user", "go")];
        for i in 0..10 {
            tail.push(tool_msg("read_file", &format!("r{i} ").repeat(1_000)));
        }
        // Each row ≈4k; target ≈ the last 2 rows plus slack.
        let one = tail[1].content.chars().count();
        let plan = plan_tail_prunes(&tail, one * 3, false, 0);
        assert!(plan.reached_target);
        assert!(
            plan.prunes.len() < 10,
            "minimal: pruned {} of 10, not all",
            plan.prunes.len()
        );
        // Oldest indices first.
        let idx: Vec<usize> = plan.prunes.iter().map(|p| p.index).collect();
        let mut sorted = idx.clone();
        sorted.sort_unstable();
        assert_eq!(idx, sorted, "prunes are oldest-first");
        assert_eq!(idx[0], 1, "the oldest tool row is pruned first");

        apply_tail_prunes(&mut tail, &plan);
        // The NEWEST tool row is untouched.
        let last = tail.iter().rev().find(|m| m.role == "tool").unwrap();
        assert!(
            !last.content.starts_with('['),
            "the newest output survives verbatim: {}",
            &last.content[..40]
        );
    }

    /// A tail with NO prunable rows reports reached_target=false rather
    /// than lying about success — the caller must surface that honestly.
    #[test]
    fn unreachable_target_is_reported_not_faked() {
        let tail = vec![
            msg("user", "go"),
            msg("assistant", &"thinking ".repeat(2_000)), // assistant, never pruned
        ];
        let plan = plan_tail_prunes(&tail, 100, false, 0);
        assert!(plan.prunes.is_empty(), "nothing was prunable");
        assert!(!plan.reached_target, "and it says so");
    }

    /// Idempotent: a pruned stub is below MIN_PRUNABLE and is never
    /// re-pruned, so a second pass changes nothing.
    #[test]
    fn pruning_is_idempotent() {
        let mut tail = vec![msg("user", "go")];
        for _ in 0..5 {
            tail.push(tool_msg("terminal", &"z".repeat(4_000)));
        }
        let plan = plan_tail_prunes(&tail, 2_000, false, 0);
        apply_tail_prunes(&mut tail, &plan);
        let after_first = estimate_chars(&tail, false);
        let plan2 = plan_tail_prunes(&tail, 2_000, false, 0);
        assert!(
            plan2.prunes.is_empty(),
            "a second pass prunes nothing: {:?}",
            plan2.prunes
        );
        apply_tail_prunes(&mut tail, &plan2);
        assert_eq!(estimate_chars(&tail, false), after_first);
    }

    /// The stub names the tool and the original size, and points at
    /// session_search — the model must be able to decide to re-run.
    #[test]
    fn the_stub_says_what_was_lost_and_where_it_lives() {
        let s = prune_stub(Some("read_file"), 35_000);
        assert!(s.contains("read_file"), "{s}");
        assert!(
            s.contains("35000") || s.contains("35_000") || s.contains("35000 chars"),
            "{s}"
        );
        assert!(s.contains("session_search"), "{s}");
        // No name: still a usable stub.
        assert!(prune_stub(None, 10).contains("tool output"));
    }

    /// The protection FLOOR: the newest N messages are never candidates,
    /// however tight the budget. The model is reasoning over those right
    /// now; stubbing the output it just read would leave the turn with no
    /// idea what it had found.
    ///
    /// CALIBRATION MUTATION: drop the `i >= protect_from` break and the
    /// newest-survives assertion goes red.
    #[test]
    fn the_most_recent_outputs_are_never_pruned() {
        let mut tail = vec![msg("user", "go")];
        for i in 0..10 {
            tail.push(msg("assistant", "looking"));
            tail.push(tool_msg("read_file", &format!("r{i} ").repeat(1_000)));
        }
        // Target of ZERO: the most aggressive budget possible. Even so,
        // the last 6 messages (3 assistant/tool rounds) stay verbatim.
        let protect = 6;
        let plan = plan_tail_prunes(&tail, 0, false, protect);
        apply_tail_prunes(&mut tail, &plan);

        let n = tail.len();
        for m in &tail[n - protect..] {
            if m.role == "tool" {
                assert!(
                    !m.content.starts_with('['),
                    "a protected recent output was stubbed: {:?}",
                    &m.content[..50.min(m.content.len())]
                );
            }
        }
        // And something OUTSIDE the window did get pruned, so the
        // assertion above is about the floor and not about a no-op plan.
        assert!(!plan.prunes.is_empty(), "older outputs were pruned");
        assert!(
            plan.prunes.iter().all(|p| p.index < n - protect),
            "every prune is outside the protected window: {:?}",
            plan.prunes.iter().map(|p| p.index).collect::<Vec<_>>()
        );
        assert!(!plan.reached_target, "a zero target is honestly missed");
    }

    /// The degenerate case that motivated the floor: a fat tool registry
    /// can eat the entire target budget, so `compaction_target_chars`
    /// hands the planner 0. Without protection that stubs the tool result
    /// from one message ago and the turn continues blind. With it, the
    /// plan prunes only genuinely old rows and admits it missed.
    #[test]
    fn a_zero_target_does_not_eat_live_context() {
        // A short tail entirely inside the protected window: nothing at
        // all may be pruned, and the plan says the target was missed
        // rather than buying it with context the model needs.
        let mut tail = vec![msg("user", "go")];
        for _ in 0..3 {
            tail.push(msg("assistant", "x"));
            tail.push(tool_msg("terminal", &"y".repeat(9_000)));
        }
        let plan = plan_tail_prunes(&tail, 0, false, 12);
        assert!(
            plan.prunes.is_empty(),
            "nothing outside the window, so nothing pruned: {:?}",
            plan.prunes.iter().map(|p| p.index).collect::<Vec<_>>()
        );
        assert!(!plan.reached_target, "and it does not claim success");
        apply_tail_prunes(&mut tail, &plan);
        assert!(
            tail.iter().all(|m| !m.content.starts_with('[')),
            "every output survived"
        );
    }

    // ── tier 3: round dropping ────────────────────────────────────────

    /// An assistant row that DECLARES tool calls, with real `tool_calls`
    /// JSON — the thing that makes a round self-contained or not.
    fn call_msg(ids: &[&str], arg_chars: usize) -> ChatMessage {
        let mut m = msg("assistant", "let me look");
        m.tool_calls = Some(serde_json::Value::Array(
            ids.iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": "patch",
                            "arguments": "a".repeat(arg_chars),
                        }
                    })
                })
                .collect(),
        ));
        m
    }

    /// A tool result answering `id`, persisted (carries a msg_id) so it is
    /// addressable for archiving.
    fn result_msg(id: &str, chars: usize) -> ChatMessage {
        let mut m = tool_msg("patch", &"r".repeat(chars));
        m.tool_meta = Some(crate::providers::ToolMeta {
            tool_call_id: id.into(),
            name: "patch".into(),
        });
        m.msg_id = Some(id.parse::<i64>().unwrap_or(1) + 1000);
        m
    }

    /// Give every row a msg_id, as `load_history` does: tier 3 archives by
    /// id, so a plan over unpersisted rows must be empty.
    fn stamp_ids(tail: &mut [ChatMessage]) {
        for (i, m) in tail.iter_mut().enumerate() {
            m.msg_id = Some(i as i64 + 1);
        }
    }

    /// The measured residue: a tail whose weight is `tool_calls` JSON, not
    /// tool output. Pruning cannot reach the target here (stubbing tool
    /// content leaves the call arguments untouched), which is exactly why
    /// tier 3 exists. Dropping the complete rounds does reach it.
    #[test]
    fn round_drops_reach_a_target_pruning_cannot() {
        let mut tail = vec![msg("user", "go do the thing")];
        for i in 0..40 {
            let id = format!("c{i}");
            // Fat ARGUMENTS (the real residue: patch/write_file payloads),
            // small results. Pruning the results saves almost nothing.
            tail.push(call_msg(&[&id], 4_000));
            let mut r = result_msg(&id, 300);
            r.msg_id = None;
            tail.push(r);
        }
        stamp_ids(&mut tail);

        // Pruning alone: the results are below MIN_PRUNABLE_CHARS, so
        // there is nothing to stub and the target is missed.
        let prune = plan_tail_prunes(&tail, 20_000, false, 4);
        assert!(!prune.reached_target, "pruning cannot reach it: {prune:?}");

        // Dropping the complete rounds can.
        let drop_plan = plan_round_drops(&tail, 20_000, false, 4);
        assert!(
            drop_plan.reached_target,
            "tier 3 reaches the target: {:?}",
            drop_plan.rounds
        );
        assert!(drop_plan.rounds > 0, "and it dropped real rounds");
        // Only assistant/tool rows, never the user spine.
        for &idx in &drop_plan.indices {
            assert_ne!(tail[idx].role, "user", "a user row was dropped");
        }
    }

    /// A round whose tool result sits OUTSIDE the group (typically in the
    /// protected window) must NOT be dropped: that would orphan the result
    /// and the provider rejects the request. This is the case that is easy
    /// to miss, because it only happens at the group/protected boundary.
    ///
    /// The fixture carries a POSITIVE CONTROL — a complete round that IS
    /// droppable — so the plan is non-empty for the right reason. Without
    /// it the test passes VACUOUSLY: the first version of it protected the
    /// entire tail, so the plan was empty no matter what the planner did
    /// and the assertion could not fail. That version stayed green under
    /// the mutation below; this one does not.
    ///
    /// CALIBRATION MUTATION: make `self_contained_round` always return
    /// `Some` (skip the declared==answered check) and the c1 call gets
    /// dropped while its result survives protected — the orphan assertion
    /// goes red. VERIFIED RED, along with
    /// `an_orphaned_result_makes_its_group_undroppable`.
    #[test]
    fn a_call_whose_result_is_outside_the_group_is_not_dropped() {
        let mut tail = vec![msg("user", "go")];
        // The positive control: a complete, self-contained round that IS
        // droppable, so a non-empty plan proves nothing about c1 on its own.
        tail.push(call_msg(&["c0"], 5_000));
        tail.push(result_msg("c0", 5_000));
        // A round that declares c1 but whose result is the LAST row — i.e.
        // inside the protected window, outside any candidate group.
        tail.push(call_msg(&["c1"], 5_000));
        tail.push(msg("assistant", "filler, no call"));
        tail.push(result_msg("c1", 5_000));
        stamp_ids(&mut tail);

        // protect_recent = 1: only the final c1 result is protected. The c1
        // CALL and the filler are candidates, but they cannot form a
        // balanced unit without their result, so they must be skipped.
        let plan = plan_round_drops(&tail, 0, false, 1);
        let dropped: Vec<usize> = plan.indices.clone();
        assert!(
            !dropped.is_empty(),
            "the complete c0 round is droppable, so the plan is not empty — \
             otherwise this test proves nothing: {dropped:?}"
        );
        // The c1 call must NOT have been dropped: its result survives in the
        // protected window, and dropping the call orphans it.
        let c1_call_dropped = dropped.iter().any(|&i| {
            tail[i]
                .tool_calls
                .as_ref()
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .any(|c| c.get("id").and_then(|v| v.as_str()) == Some("c1"))
                })
                .unwrap_or(false)
        });
        assert!(
            !c1_call_dropped,
            "the c1 call was dropped while its result survives protected — \
             that orphans a tool result and the provider rejects it: {dropped:?}"
        );
        // And the protected result row itself is never dropped.
        assert!(
            !dropped.contains(&(tail.len() - 1)),
            "the protected row was dropped: {dropped:?}"
        );
    }

    /// A tool result with no matching declaration in its group (an orphan)
    /// makes the whole group undroppable.
    #[test]
    fn an_orphaned_result_makes_its_group_undroppable() {
        let mut tail = vec![msg("user", "go")];
        tail.push(result_msg("nowhere", 9_000)); // no call declares it
        tail.push(msg("assistant", "thinking"));
        stamp_ids(&mut tail);
        let plan = plan_round_drops(&tail, 0, false, 0);
        assert!(
            plan.indices.is_empty(),
            "the orphaned group was not dropped: {:?}",
            plan.indices
        );
        assert!(!plan.reached_target, "and it says so");
    }

    /// `user` rows are NEVER dropped, whatever the budget. They are the
    /// conversation's spine — and the marker/todo/hook injections are
    /// user-role rows, so this is also what keeps them in place.
    #[test]
    fn user_rows_are_never_dropped() {
        let mut tail = vec![msg("user", "the important instruction")];
        for i in 0..10 {
            let id = format!("c{i}");
            tail.push(call_msg(&[&id], 3_000));
            tail.push(result_msg(&id, 3_000));
            tail.push(msg("user", &format!("and also {i}")));
        }
        stamp_ids(&mut tail);
        let plan = plan_round_drops(&tail, 0, false, 0);
        assert!(!plan.indices.is_empty(), "something was droppable");
        for &idx in &plan.indices {
            assert_ne!(tail[idx].role, "user", "a user row was dropped");
        }
        // Every user row is still in the tail after the drop set is
        // removed, so the spine survives intact.
        let users_before = tail.iter().filter(|m| m.role == "user").count();
        let dropped: std::collections::HashSet<usize> = plan.indices.iter().copied().collect();
        let users_after = tail
            .iter()
            .enumerate()
            .filter(|(i, m)| m.role == "user" && !dropped.contains(i))
            .count();
        assert_eq!(users_before, users_after, "all user rows survive");
    }

    /// The protected window is a floor for drops too, same as for prunes.
    #[test]
    fn protected_rounds_are_never_dropped() {
        let mut tail = vec![msg("user", "go")];
        for i in 0..10 {
            let id = format!("c{i}");
            tail.push(call_msg(&[&id], 3_000));
            tail.push(result_msg(&id, 3_000));
        }
        stamp_ids(&mut tail);
        let protect = 6;
        let plan = plan_round_drops(&tail, 0, false, protect);
        let n = tail.len();
        for &idx in &plan.indices {
            assert!(
                idx < n - protect,
                "a protected row was dropped: {idx} ≥ {}",
                n - protect
            );
        }
    }

    /// Unpersisted rows are not droppable: archiving is by id, and a row
    /// with no id cannot be archived, so dropping it from memory would
    /// DELETE it. The planner skips such groups entirely.
    ///
    /// CALIBRATION MUTATION: remove the `msg_id.is_none()` guard and this
    /// goes red (a group of unpersisted rows becomes "droppable").
    #[test]
    fn rows_without_a_msg_id_are_not_dropped() {
        let mut tail = vec![msg("user", "go")];
        for i in 0..6 {
            let id = format!("c{i}");
            tail.push(call_msg(&[&id], 3_000));
            tail.push(result_msg(&id, 3_000));
        }
        // Clear every id: this simulates a history that was never reloaded
        // from the store (or predates id stamping). `result_msg` stamps one
        // by default, so the wipe has to be explicit.
        for m in &mut tail {
            m.msg_id = None;
        }
        assert!(tail.iter().all(|m| m.msg_id.is_none()));
        let plan = plan_round_drops(&tail, 0, false, 0);
        assert!(
            plan.indices.is_empty(),
            "nothing is droppable without ids: {:?}",
            plan.indices
        );
        assert!(!plan.reached_target, "and the miss is reported");
    }

    /// Dropping is minimal and oldest-first, like pruning: the newest
    /// rounds (what the model is working from) survive even when the
    /// budget could have been met by dropping more.
    #[test]
    fn round_drops_are_minimal_and_oldest_first() {
        let mut tail = vec![msg("user", "go")];
        for i in 0..10 {
            let id = format!("c{i}");
            tail.push(call_msg(&[&id], 3_000));
            tail.push(result_msg(&id, 3_000));
        }
        stamp_ids(&mut tail);
        let one_round = estimate_chars(&tail[1..3], false);
        // Target that leaves roughly the last 3 rounds standing.
        let plan = plan_round_drops(&tail, one_round * 3, false, 0);
        assert!(plan.reached_target);
        assert!(
            plan.rounds < 10,
            "minimal: dropped {} of 10 rounds",
            plan.rounds
        );
        let idx = plan.indices.clone();
        let mut sorted = idx.clone();
        sorted.sort_unstable();
        assert_eq!(idx, sorted, "oldest-first, contiguous");
        // The newest round is still present.
        assert!(!idx.contains(&(tail.len() - 1)), "the newest row survives");
    }
}
