//! Harness-guard family — pure policy modules with no side effects.
//!
//! Ported from hermes' agent/ guard family (verification_stop,
//! tool_guardrails, iteration_budget, error_classifier,
//! tool_result_classification, turn_retry_state, rate_limit_tracker,
//! stream_diag). Design principle kept: controllers track observations
//! and return decisions; runtime code owns whether decisions become
//! synthetic messages, warnings, or turn halts.
//!
//! Everything here is synchronous and dependency-free so it can be
//! unit-tested without a gateway.
//!
//! ## Wired vs unwired
//!
//! Being unit-tested is not the same as being USED, and this file was
//! most of a family that nothing calls. Eight of its public items are
//! reached only from their own tests, so the suite stayed green and the
//! mechanism never ran — the `parse_bundle` pattern at larger scale.
//!
//! The error taxonomy was the sharp case and is now DELETED (2026-09-02,
//! at the user's direction): `classify_error` → `advise` →
//! `TurnRetryState::should_retry` was a complete second implementation
//! of the failover policy, reachable only from its own tests. Failover
//! has one source of truth now — `ProviderError::should_fallback` in
//! providers.rs decides whether an error is worth moving for, and
//! `failover.rs` decides the policy (ask / auto / stop / script, with
//! per-provider rules and vetoable hooks). A dead duplicate of live
//! logic is not a mechanism someone may want later; it is a second
//! answer waiting to be believed.
//!
//! `WIRED_GUARDS` / `UNWIRED_GUARDS` below name both sets, and a test
//! checks each against the call sites so neither list can quietly drift
//! — the same contract `hooks::FIRED_HOOKS` gets.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Guards the engine actually consults.
pub const WIRED_GUARDS: &[&str] = &[
    "IterationBudget",
    "ToolGuardrailController",
    "VerificationLedger",
    "verification_nudge",
    "verification_hint",
    "unverified_summary",
    // RM3 #6: gates record_edit and the file-focus updates on whether
    // a write actually landed (agent.rs dispatch loop).
    "file_mutation_landed",
];

/// Present, unit-tested, and called by NOTHING outside this file.
///
/// Left in place rather than deleted: each is a mechanism someone may
/// want, and wiring one is a smaller job than rewriting it. But nothing
/// here runs today, and a reader must not have to grep to find that
/// out. `veles doctor --mechanisms` prints this list.
///
/// The failover trio that used to head this list (`classify_error`,
/// `advise`, `RetryController`) is gone, not disabled. The standing rule
/// — leave a counter-productive mechanism in and switched off — applies
/// to mechanisms someone might want. A dead SECOND COPY of live logic is
/// a different thing: nobody wants two failover policies, and the one
/// that never runs is the one that quietly goes stale.
pub const UNWIRED_GUARDS: &[&str] = &[
    // Advisory backoff on repeated 429s — never consulted.
    "RateLimitTracker",
    // Stream-stall detection — never consulted.
    "StallDetector",
    // A "still thinking" hint for silent streams — never emitted.
    "reasoning_hint_due",
];

// ── iteration budget ─────────────────────────────────────────────────
// hermes: agent/iteration_budget.py — parent cap 500, subagent 50,
// execute_code rounds refunded. veles keeps the same defaults.

pub const DEFAULT_MAX_ITERATIONS: u32 = 500;
pub const DEFAULT_SUBAGENT_MAX_ITERATIONS: u32 = 50;

/// Thread-safe consume/refund counter for one agent's tool rounds.
pub struct IterationBudget {
    cap: AtomicU32,
    used: AtomicU32,
}

impl IterationBudget {
    pub fn new(cap: u32) -> Self {
        Self {
            cap: AtomicU32::new(cap),
            used: AtomicU32::new(0),
        }
    }

    /// Try to spend one iteration. False = budget exhausted.
    pub fn consume(&self) -> bool {
        loop {
            let used = self.used.load(Ordering::SeqCst);
            if used >= self.cap.load(Ordering::SeqCst) {
                return false;
            }
            if self
                .used
                .compare_exchange(used, used + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Zero the counter at a turn boundary. The cap is "tool rounds
    /// PER TURN" (config.rs GuardsSection) and the halt says "turn
    /// halted" — but nothing reset this, so `used` accumulated for the
    /// life of the Agent and the cap silently became a per-SESSION one.
    /// A session that reached it was bricked: `consume()` is the first
    /// thing in the round loop, so every later turn halted before the
    /// provider was called.
    pub fn reset(&self) {
        self.used.store(0, Ordering::SeqCst);
    }

    /// Give one back (execute_code-style programmatic rounds don't eat
    /// the budget in hermes; veles refunds on request).
    pub fn refund(&self) {
        // rustc 1.99 renames `fetch_update` to `try_update`; the flake's
        // toolchain (1.98) has only the old name. Both build this.
        #[allow(deprecated)]
        let _ = self
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| {
                Some(u.saturating_sub(1))
            });
    }

    pub fn remaining(&self) -> u32 {
        self.cap
            .load(Ordering::SeqCst)
            .saturating_sub(self.used.load(Ordering::SeqCst))
    }

    pub fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }
}

// ── tool guardrails (repeat / stuck detection) ───────────────────────
// hermes: agent/tool_guardrails.py — identical consecutive tool calls
// (name + normalized args hash) trip a warning, then a halt.

#[derive(Debug, Clone, PartialEq)]
pub enum GuardrailDecision {
    Continue,
    /// The same call has now repeated `count` times in a row.
    RepeatWarning {
        count: u32,
    },
    /// Stuck: halt the turn rather than burn more tokens.
    Halt,
}

pub struct ToolGuardrailController {
    last_hash: Option<u64>,
    run_len: u32,
    warn_at: u32,
    halt_at: u32,
}

impl Default for ToolGuardrailController {
    fn default() -> Self {
        Self::new(3, 5)
    }
}

impl ToolGuardrailController {
    pub fn new(warn_at: u32, halt_at: u32) -> Self {
        Self {
            last_hash: None,
            run_len: 0,
            warn_at,
            halt_at,
        }
    }

    /// Reset between turns (a new user message clears the stuck state).
    pub fn reset(&mut self) {
        self.last_hash = None;
        self.run_len = 0;
    }

    /// Observe one tool call; returns the decision for it.
    pub fn observe(&mut self, tool: &str, args_json: &str) -> GuardrailDecision {
        let h = hash_call(tool, args_json);
        if self.last_hash == Some(h) {
            self.run_len += 1;
        } else {
            self.last_hash = Some(h);
            self.run_len = 1;
        }
        if self.run_len >= self.halt_at {
            GuardrailDecision::Halt
        } else if self.run_len >= self.warn_at {
            GuardrailDecision::RepeatWarning {
                count: self.run_len,
            }
        } else {
            GuardrailDecision::Continue
        }
    }
}

/// FNV-1a over name + args (hermes hashes the normalized JSON; the
/// exact hash function doesn't matter, only stability within a turn).
pub fn hash_call(tool: &str, args_json: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in tool.bytes().chain(args_json.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ── rate-limit tracker ───────────────────────────────────────────────
// Session-wide 429 history; advisory backoff, no enforcement.

#[derive(Default)]
pub struct RateLimitTracker {
    events: VecDeque<Instant>,
}

impl RateLimitTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self) {
        self.events.push_back(Instant::now());
        self.prune();
    }

    fn prune(&mut self) {
        let cutoff = Instant::now() - Duration::from_secs(300);
        while let Some(front) = self.events.front() {
            if *front < cutoff {
                self.events.pop_front();
            } else {
                break;
            }
        }
    }

    /// 429s in the last 60s.
    pub fn recent(&mut self) -> usize {
        self.prune();
        let cutoff = Instant::now() - Duration::from_secs(60);
        self.events.iter().filter(|t| **t >= cutoff).count()
    }

    /// Advisory: 3+ rate limits in a minute → slow down for 15s.
    pub fn advisory_pause(&mut self) -> Option<Duration> {
        if self.recent() >= 3 {
            Some(Duration::from_secs(15))
        } else {
            None
        }
    }
}

// ── stream diagnostics ───────────────────────────────────────────────
// Detects a stream that went silent (no tokens at all for too long).

pub struct StreamDiag {
    last_event: Instant,
}

impl Default for StreamDiag {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDiag {
    pub fn new() -> Self {
        Self {
            last_event: Instant::now(),
        }
    }

    pub fn mark_event(&mut self) {
        self.last_event = Instant::now();
    }

    /// True when the stream has been silent longer than `timeout`.
    pub fn is_stalled(&self, timeout: Duration) -> bool {
        self.last_event.elapsed() > timeout
    }

    pub fn silent_for(&self) -> Duration {
        self.last_event.elapsed()
    }
}

// ── reasoning-timeout guidance ───────────────────────────────────────
// Reasoning models can sit in a long silent reasoning phase; hermes
// nudges the display. Pure decision: given silent-for + saw_reasoning,
// should we show a "still thinking" hint?

pub fn reasoning_hint_due(silent_for: Duration, saw_reasoning_chunk: bool) -> bool {
    saw_reasoning_chunk && silent_for >= Duration::from_secs(45)
}

// ── tool-result classification ───────────────────────────────────────
// Did a file mutation actually land? Used by the verification ledger.

const FAILURE_MARKERS: &[&str] = &[
    "error:",
    "error ",
    "failed:",
    "not found",
    "ambiguous",
    "no unique match",
    "permission denied",
    "could not",
    "refused",
];

pub fn file_mutation_landed(result: &str) -> bool {
    let lower = result.to_lowercase();
    !FAILURE_MARKERS.iter().any(|m| lower.contains(m))
}

// ── verification evidence ledger ─────────────────────────────────────
// hermes: agent/verification_evidence.py — passive record of what was
// edited vs what was proved. Never runs anything itself.

#[derive(Debug, Clone)]
pub struct EvidenceEvent {
    pub kind: String, // "terminal" | "execute_code" | "test"
    pub ok: bool,
}

#[derive(Default)]
pub struct VerificationLedger {
    edits: u64,
    /// Code edits stacked up since the last PASSING evidence event.
    /// Prose is not counted: appending to a README cannot invalidate a
    /// green test run, and pretending it could made the guard fire on
    /// exactly the turns that did the most work — edit, verify, write it
    /// up. Found by the ghostwire qwen3.8-max run, 2026-09-03.
    unverified_code_edits: u64,
    evidence_after_last_edit: Vec<EvidenceEvent>,
    edited_paths: Vec<String>,
}

impl VerificationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// A file was written/patched. A CODE edit resets the evidence
    /// window. A prose edit is recorded and otherwise ignored: writing
    /// up what you did is not a reason to stop believing you verified
    /// it, and treating it as one is how a turn that edited, checked,
    /// and then documented got told it had never checked.
    pub fn record_edit(&mut self, path: &str) {
        self.edits += 1;
        if !self.edited_paths.iter().any(|p| p == path) {
            self.edited_paths.push(path.to_string());
        }
        if is_non_code_path(path) {
            return;
        }
        self.unverified_code_edits += 1;
        self.evidence_after_last_edit.clear();
    }

    /// A command ran. `ok` = exit success / harness success.
    pub fn record_evidence(&mut self, kind: &str, ok: bool) {
        if self.edits == 0 {
            return; // nothing to prove yet
        }
        if ok {
            self.unverified_code_edits = 0;
        }
        self.evidence_after_last_edit.push(EvidenceEvent {
            kind: kind.to_string(),
            ok,
        });
    }

    pub fn edited_any(&self) -> bool {
        self.edits > 0
    }

    pub fn edited_paths(&self) -> &[String] {
        &self.edited_paths
    }

    /// Fresh = at least one SUCCESSFUL evidence event after the last edit.
    pub fn has_fresh_evidence(&self) -> bool {
        self.evidence_after_last_edit.iter().any(|e| e.ok)
    }

    /// How many code edits have piled up with no passing check between
    /// them. One is the ordinary way of working and says nothing; two or
    /// more is the shape worth a word.
    pub fn unverified_code_edits(&self) -> u64 {
        self.unverified_code_edits
    }

    /// Fresh evidence of ANY kind (even failed runs count as "you tried
    /// to verify" for the soft nudge; hermes distinguishes, we keep it
    /// simple: success required for "no nudge").
    pub fn evidence_count(&self) -> usize {
        self.evidence_after_last_edit.len()
    }
}

// ── verification-stop policy ─────────────────────────────────────────
// hermes: agent/verification_stop.py — policy-only nudge when code was
// edited without fresh proof. Suppressed for prose-only edits and for
// human messaging surfaces (telegram/irc/...), bounded by attempts.

const NON_CODE_EXTENSIONS: &[&str] = &[
    ".md",
    ".markdown",
    ".mdx",
    ".rst",
    ".txt",
    ".text",
    ".adoc",
    ".asciidoc",
    ".org",
    ".log",
    ".csv",
    ".tsv",
];

const NON_CODE_FILENAMES: &[&str] = &[
    "license",
    "licence",
    "notice",
    "authors",
    "contributors",
    "changelog",
    "codeowners",
];

const MESSAGING_SURFACES: &[&str] = &["telegram", "irc", "signal", "email"];

pub fn is_non_code_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    if let Some(dot) = lower.rfind('.') {
        let ext = &lower[dot..];
        if NON_CODE_EXTENSIONS.contains(&ext) {
            return true;
        }
    }
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    NON_CODE_FILENAMES.contains(&name)
}

pub fn filter_verifiable_paths<'a>(paths: impl IntoIterator<Item = &'a String>) -> Vec<&'a String> {
    paths.into_iter().filter(|p| !is_non_code_path(p)).collect()
}

/// Decide whether to nudge at turn end. Returns the synthetic message
/// text, or None when no nudge is due.
pub fn verification_nudge(
    edited_paths: &[String],
    has_fresh_evidence: bool,
    surface: &str,
    attempts: u32,
    max_attempts: u32,
) -> Option<String> {
    if edited_paths.is_empty() {
        return None;
    }
    let verifiable = filter_verifiable_paths(edited_paths.iter());
    if verifiable.is_empty() {
        return None; // prose-only turn — nothing to verify
    }
    if has_fresh_evidence {
        return None;
    }
    if MESSAGING_SURFACES.contains(&surface) {
        return None; // don't nag a human chat channel
    }
    if attempts >= max_attempts {
        return None; // bounded — never loop
    }
    let shown: Vec<&str> = verifiable.iter().take(8).map(|s| s.as_str()).collect();
    let more = if verifiable.len() > 8 {
        format!(" ... and {} more", verifiable.len() - 8)
    } else {
        String::new()
    };
    Some(format!(
        "[System: You edited code ({}){} but this turn has no fresh passing verification. Run the relevant build/tests/checks now and report the real output, or state plainly why verification is impossible.]",
        shown.join(", "),
        more
    ))
}

/// The cheap half of the same idea: one line appended to the EDIT's own
/// tool result, so the model learns the edit is unverified inside a
/// round that was happening anyway.
///
/// Why this shape rather than the nudge above:
///   * **no extra round** — the nudge re-sends the whole prefix and the
///     whole history to say twelve tokens' worth of thing;
///   * **it lands while acting is still cheap** — the nudge fires after
///     a final answer is composed, so complying means discarding it,
///     which is the moment a model is least inclined to comply;
///   * **it is honestly attributed** — the nudge writes a message the
///     USER never sent and leaves it in the transcript forever. This is
///     tool output, which is exactly what it is.
///
/// Returns `None` for a path where "verify" means nothing (prose, data,
/// config), reusing the nudge's own filter so the two cannot drift on
/// which files count — and `None` for the first unverified edit, which
/// is simply what working looks like.
///
/// The earlier version said "no passing check has run since this edit"
/// after EVERY code edit. That sentence is true by construction every
/// time it is emitted — no check can have run since an edit made a
/// moment ago — so it carried no information at all. What is worth
/// saying is that edits are stacking up unchecked, which needs a count.
pub fn verification_hint(path: &str, unverified_code_edits: u64) -> Option<String> {
    let owned = path.to_string();
    if filter_verifiable_paths(std::iter::once(&owned)).is_empty() {
        return None;
    }
    if unverified_code_edits < 2 {
        return None;
    }
    Some(format!(
        "\n\n[unverified: {unverified_code_edits} edits since the last passing check]"
    ))
}

/// What the HUMAN is told at the end of a turn that edited code and
/// never verified it. Costs zero tokens: it rides the note stream, not
/// the prompt. The model gets the hint; the user gets the receipt.
pub fn unverified_summary(edited_paths: &[String], has_fresh_evidence: bool) -> Option<String> {
    if has_fresh_evidence {
        return None;
    }
    let verifiable = filter_verifiable_paths(edited_paths.iter());
    if verifiable.is_empty() {
        return None;
    }
    let shown: Vec<&str> = verifiable.iter().take(5).map(|s| s.as_str()).collect();
    let more = if verifiable.len() > 5 {
        format!(" (+{} more)", verifiable.len() - 5)
    } else {
        String::new()
    };
    Some(format!(
        "{} file(s) edited, no passing check run: {}{}",
        verifiable.len(),
        shown.join(", "),
        more
    ))
}

/// The longest FOREGROUND `sleep` a shell command blocks on, in seconds
/// (`None` when it has none). Feeds `[agent.guards]
/// foreground_sleep_max_secs`: a model that has started background work
/// and then runs `sleep 240; tail log` holds the whole turn, and the
/// completion it is waiting for queues behind the sleep.
///
/// Deliberately a small tokenizer, not a shell parser:
///   - `sleep` counts only at a COMMAND position — the start, after
///     `; & && | || ( ) { }` or a newline, after `do`/`then`/`else`/`!`,
///     or after `VAR=value` assignments. `echo "sleep 60"` and
///     `bash -c 'sleep 60'` are not foreground sleeps of THIS shell's
///     own text, and quoting keeps them out.
///   - Its arguments are summed like GNU sleep's (`sleep 1m 30s` = 90),
///     with `s m h d` suffixes and `infinity`.
///   - A sleep followed by a lone `&` is backgrounded and ignored.
pub fn longest_foreground_sleep(cmd: &str) -> Option<f64> {
    #[derive(PartialEq)]
    enum Tok {
        Word(String),
        Sep(&'static str),
    }
    let mut toks = Vec::new();
    let mut word = String::new();
    let mut chars = cmd.chars().peekable();
    let flush = |w: &mut String, t: &mut Vec<Tok>| {
        if !w.is_empty() {
            t.push(Tok::Word(std::mem::take(w)));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            ';' | '(' | ')' | '{' | '}' | '\n' => {
                flush(&mut word, &mut toks);
                toks.push(Tok::Sep(";"));
            }
            '&' | '|' => {
                flush(&mut word, &mut toks);
                if chars.peek() == Some(&c) {
                    chars.next();
                    toks.push(Tok::Sep(";"));
                } else if c == '&' {
                    toks.push(Tok::Sep("&"));
                } else {
                    toks.push(Tok::Sep(";"));
                }
            }
            c if c.is_whitespace() => flush(&mut word, &mut toks),
            _ => word.push(c),
        }
    }
    flush(&mut word, &mut toks);

    fn duration(w: &str) -> Option<f64> {
        if w == "infinity" || w == "inf" {
            return Some(f64::INFINITY);
        }
        let (num, mult) = match w.chars().last()? {
            's' => (&w[..w.len() - 1], 1.0),
            'm' => (&w[..w.len() - 1], 60.0),
            'h' => (&w[..w.len() - 1], 3600.0),
            'd' => (&w[..w.len() - 1], 86400.0),
            _ => (w, 1.0),
        };
        let n: f64 = num.parse().ok()?;
        (n.is_finite() && n >= 0.0).then_some(n * mult)
    }

    let mut longest: Option<f64> = None;
    let mut at_cmd = true;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Sep(_) => {
                at_cmd = true;
                i += 1;
            }
            Tok::Word(w) if at_cmd && w == "sleep" => {
                let mut total = 0.0;
                let mut j = i + 1;
                while let Some(Tok::Word(a)) = toks.get(j) {
                    match duration(a) {
                        Some(d) => total += d,
                        None => break,
                    }
                    j += 1;
                }
                let backgrounded = toks.get(j) == Some(&Tok::Sep("&"));
                if j > i + 1 && !backgrounded {
                    longest = Some(longest.map_or(total, |l: f64| l.max(total)));
                }
                at_cmd = false;
                i = j;
            }
            Tok::Word(w) => {
                let keyword = matches!(w.as_str(), "do" | "then" | "else" | "!" | "time");
                let assignment = at_cmd
                    && w.split_once('=').is_some_and(|(k, _)| {
                        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    });
                at_cmd = keyword || assignment;
                i += 1;
            }
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RM3: the exact refusal qwen-flash wore a misleading
    /// "[unverified: 2 edits since the last passing check]" suffix on —
    /// a FAILED call is not a mutation, and must never count as one
    /// (agent.rs gates record_edit on this predicate).
    #[test]
    fn a_failed_call_is_not_a_landed_mutation() {
        assert!(!file_mutation_landed(
            "error: path, old_string, new_string are required"
        ));
        assert!(!file_mutation_landed(
            "error: old_string not found in /x/main.rs"
        ));
        assert!(file_mutation_landed(
            "patched /x/main.rs (1 replacement)\n--- x\n+++ x"
        ));
        assert!(file_mutation_landed("wrote 120 bytes to /x/new.rs"));
    }

    #[test]
    fn budget_consume_refund() {
        let b = IterationBudget::new(3);
        assert!(b.consume());
        assert!(b.consume());
        assert!(b.consume());
        assert!(!b.consume());
        b.refund();
        assert!(b.consume());
        assert_eq!(b.remaining(), 0);
    }

    /// An exhausted budget comes back whole at the turn boundary. Without
    /// this the cap is a per-SESSION one that permanently kills the
    /// session it is reached in — see the agent test
    /// `the_iteration_budget_is_per_turn_not_per_session`.
    #[test]
    fn budget_reset_restores_the_whole_cap() {
        let b = IterationBudget::new(2);
        assert!(b.consume());
        assert!(b.consume());
        assert!(!b.consume(), "spent");
        b.reset();
        assert_eq!(b.used(), 0);
        assert_eq!(b.remaining(), 2, "the next turn gets the full cap");
        assert!(b.consume());
        assert!(b.consume());
        assert!(!b.consume(), "and the cap still binds within that turn");
    }

    #[test]
    fn guardrails_warn_then_halt() {
        let mut g = ToolGuardrailController::new(3, 5);
        for i in 1..=2 {
            assert_eq!(
                g.observe("terminal", r#"{"command":"ls"}"#),
                GuardrailDecision::Continue,
                "call {i}"
            );
        }
        assert_eq!(
            g.observe("terminal", r#"{"command":"ls"}"#),
            GuardrailDecision::RepeatWarning { count: 3 }
        );
        assert_eq!(
            g.observe("terminal", r#"{"command":"ls"}"#),
            GuardrailDecision::RepeatWarning { count: 4 }
        );
        assert_eq!(
            g.observe("terminal", r#"{"command":"ls"}"#),
            GuardrailDecision::Halt
        );
        // Different call resets the run.
        assert_eq!(
            g.observe("terminal", r#"{"command":"pwd"}"#),
            GuardrailDecision::Continue
        );
    }

    #[test]
    fn guardrails_reset_between_turns() {
        let mut g = ToolGuardrailController::new(2, 3);
        g.observe("read_file", "{}");
        g.observe("read_file", "{}");
        g.reset();
        assert_eq!(g.observe("read_file", "{}"), GuardrailDecision::Continue);
    }

    #[test]
    fn stream_diag_stall() {
        let mut d = StreamDiag::new();
        assert!(!d.is_stalled(Duration::from_millis(10)));
        d.mark_event();
        assert!(!d.is_stalled(Duration::from_secs(60)));
        assert!(reasoning_hint_due(Duration::from_secs(50), true));
        assert!(!reasoning_hint_due(Duration::from_secs(50), false));
        assert!(!reasoning_hint_due(Duration::from_secs(10), true));
    }

    #[test]
    fn mutation_landed() {
        assert!(file_mutation_landed("patch applied: 3 lines changed"));
        assert!(!file_mutation_landed(
            "Error: no unique match for old_string"
        ));
        assert!(!file_mutation_landed("FAILED: permission denied"));
    }

    #[test]
    fn ledger_edit_clears_evidence() {
        let mut l = VerificationLedger::new();
        assert!(!l.edited_any());
        l.record_evidence("terminal", true); // before any edit: ignored
        assert!(!l.has_fresh_evidence());
        l.record_edit("src/main.rs");
        assert!(l.edited_any());
        assert!(!l.has_fresh_evidence());
        l.record_evidence("terminal", true);
        assert!(l.has_fresh_evidence());
        l.record_edit("src/lib.rs");
        assert!(!l.has_fresh_evidence()); // new edit resets
        l.record_evidence("terminal", false);
        assert!(!l.has_fresh_evidence()); // failed run is not proof
        assert_eq!(l.evidence_count(), 1);
    }

    #[test]
    fn non_code_paths() {
        assert!(is_non_code_path("docs/README.md"));
        assert!(is_non_code_path("LICENSE"));
        assert!(is_non_code_path("notes.org"));
        assert!(!is_non_code_path("src/main.rs"));
        assert!(!is_non_code_path("flake.nix"));
        assert!(!is_non_code_path("Cargo.toml"));
    }

    /// The hint is the cheap replacement for the nudge: same signal,
    /// no extra round, no words in the user's mouth.
    #[test]
    fn the_hint_marks_code_and_leaves_prose_alone() {
        assert!(
            verification_hint("src/agent.rs", 2)
                .unwrap()
                .contains("unverified"),
            "an edited source file is marked"
        );
        assert!(
            verification_hint("README.md", 9).is_none(),
            "prose has nothing to verify — the nudge's own filter, reused"
        );
        // It is APPENDED to a result, so it must start with a break and
        // never swallow the tool output it rides on.
        let h = verification_hint("src/agent.rs", 2).unwrap();
        assert!(h.starts_with("\n\n"), "must not run into the result: {h:?}");
        // And it has to be small enough that the argument holds. The
        // whole case for this over the nudge is a dozen tokens against
        // a full prefix + history round.
        assert!(h.len() < 80, "the hint must stay cheap: {} chars", h.len());
    }

    /// A line emitted after EVERY edit is true by construction — no
    /// check CAN have run since an edit made a moment ago — so it says
    /// nothing. It has to wait until there is something to report.
    #[test]
    fn the_hint_is_silent_until_edits_stack_up() {
        assert!(
            verification_hint("src/agent.rs", 1).is_none(),
            "one unchecked edit is what working looks like"
        );
        assert!(
            verification_hint("src/agent.rs", 0).is_none(),
            "and a verified edit is certainly not news"
        );
        let h = verification_hint("src/agent.rs", 4).unwrap();
        assert!(h.contains('4'), "it has to carry the count: {h:?}");
    }

    /// The defect this exists for: a turn edited code, verified it, then
    /// wrote up what it had done — and was told it had never verified
    /// anything, because the write-up cleared the ledger. Found by the
    /// ghostwire qwen3.8-max run, 2026-09-03.
    #[test]
    fn documenting_the_work_does_not_erase_the_proof() {
        let mut l = VerificationLedger::new();
        l.record_edit("src/main.rs");
        l.record_evidence("terminal", true);
        assert!(l.has_fresh_evidence());

        l.record_edit("REPORT.md");
        assert!(
            l.has_fresh_evidence(),
            "a prose edit cannot invalidate a passing check"
        );
        assert!(
            unverified_summary(l.edited_paths(), l.has_fresh_evidence()).is_none(),
            "so the human receipt stays quiet"
        );
        assert_eq!(l.unverified_code_edits(), 0, "and nothing is outstanding");

        // The prose edit is still RECORDED as an edit; it is just not a
        // claim that needs proving.
        assert!(l.edited_any());
        assert!(l.edited_paths().iter().any(|p| p == "REPORT.md"));
    }

    /// Only a PASSING check clears the backlog. A red run is an attempt,
    /// not proof.
    #[test]
    fn only_a_passing_check_clears_the_backlog() {
        let mut l = VerificationLedger::new();
        l.record_edit("src/a.rs");
        assert_eq!(l.unverified_code_edits(), 1);
        l.record_edit("src/b.rs");
        assert_eq!(l.unverified_code_edits(), 2);
        l.record_evidence("terminal", false);
        assert_eq!(l.unverified_code_edits(), 2, "a red run proves nothing");
        l.record_evidence("terminal", true);
        assert_eq!(l.unverified_code_edits(), 0);
    }

    /// The human-facing half costs zero tokens and never fires when the
    /// turn actually verified something.
    #[test]
    fn the_unverified_summary_is_for_the_human_and_respects_evidence() {
        let edited = vec!["src/a.rs".to_string(), "docs/b.md".to_string()];
        let s = unverified_summary(&edited, false).expect("a summary");
        assert!(s.contains("src/a.rs"), "{s}");
        assert!(
            !s.contains("docs/b.md"),
            "prose is not counted as unverified: {s}"
        );
        assert!(s.starts_with("1 file"), "counts the VERIFIABLE ones: {s}");
        assert!(
            unverified_summary(&edited, true).is_none(),
            "a turn that ran a passing check is not warned about"
        );
        assert!(
            unverified_summary(&["notes.txt".to_string()], false).is_none(),
            "a prose-only turn is silent"
        );
        assert!(
            unverified_summary(&[], false).is_none(),
            "no edits, no note"
        );
    }
    #[test]
    fn nudge_policy() {
        let code = vec!["src/main.rs".to_string()];
        let prose = vec!["README.md".to_string()];
        // Fresh evidence → no nudge.
        assert!(verification_nudge(&code, true, "desktop", 0, 2).is_none());
        // Prose-only → no nudge.
        assert!(verification_nudge(&prose, false, "desktop", 0, 2).is_none());
        // Messaging surface → no nudge.
        assert!(verification_nudge(&code, false, "telegram", 0, 2).is_none());
        // Attempts exhausted → no nudge.
        assert!(verification_nudge(&code, false, "desktop", 2, 2).is_none());
        // The one case that nudges.
        let n = verification_nudge(&code, false, "desktop", 0, 2).unwrap();
        assert!(n.contains("src/main.rs"));
        assert!(n.contains("verification"));
        // Mixed paths: only code paths listed.
        let mixed = vec!["README.md".to_string(), "agent.rs".to_string()];
        let n2 = verification_nudge(&mixed, false, "tui", 0, 2).unwrap();
        assert!(n2.contains("agent.rs"));
        assert!(!n2.contains("README"));
    }

    // ── wired vs unwired (mechanism audit) ──────────────────────────

    /// Drop `//` comments and the CONTENT of string literals, so a
    /// mechanism merely NAMED in prose is not counted as a call site.
    ///
    /// This test caught exactly that on itself: `veles doctor
    /// --mechanisms` prints the unwired list, so cli.rs contains the
    /// string "guards::classify_error/advise/RetryController" — a
    /// mention, not a call, and the whole point of that output is to
    /// say these are NOT wired.
    ///
    /// `#` is deliberately not treated as a comment: it opens Rust
    /// attributes, and no guard name appears in a Tcl comment anyway.
    fn strip_prose(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for line in src.lines() {
            let mut in_str = false;
            let mut prev = '\0';
            for (i, c) in line.char_indices() {
                if !in_str && line[i..].starts_with("//") {
                    break;
                }
                if c == '"' && prev != '\\' {
                    in_str = !in_str;
                    prev = c;
                    continue;
                }
                if !in_str {
                    out.push(c);
                }
                prev = c;
            }
            out.push('\n');
        }
        out
    }

    /// Every source that could consult a guard, WALKED rather than
    /// hand-listed — Rust and Tcl both.
    ///
    /// The first version of this named five .rs files, which is the
    /// same blind spot the audit itself had: veles-desktop is a Tcl
    /// frontend, Rust reaches it through `bridge.rs`, and a mechanism
    /// exported under a Tcl name would have looked dead to a grep over
    /// five hand-picked files. (It is not — the whole Rust→Tcl surface
    /// is `veles::call_llm`, `veles::routes`, `veles::json_get` — but
    /// that has to be enforced, not assumed.)
    ///
    /// guards.rs itself is excluded on purpose: a guard only its own
    /// tests reach is exactly what this looks for.
    fn engine_sources() -> String {
        fn walk(dir: &std::path::Path, exts: &[&str], out: &mut String) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return; // absent in a sandbox: skip, don't fail
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, exts, out);
                } else if p.file_name().is_some_and(|f| f == "guards.rs") {
                    continue;
                } else if p
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| exts.contains(&x))
                {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        out.push_str(&strip_prose(&s));
                        out.push('\n');
                    }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = String::new();
        walk(&root.join("src"), &["rs"], &mut out);
        // The surfaces are their own crate since 2026-09-28 (veles-frontends);
        // they are engine sources too.
        walk(&root.join("../veles-frontends/src"), &["rs"], &mut out);
        // ../../veles-desktop relative to crates/veles-core.
        if let Some(repo) = root.parent().and_then(|p| p.parent()) {
            walk(&repo.join("veles-desktop"), &["tcl"], &mut out);
            walk(&repo.join("plugins"), &["tcl"], &mut out);
        }
        assert!(
            out.contains("should_fallback"),
            "the walk found no engine sources at all — the paths are wrong, \
             and every assertion built on this would pass vacuously"
        );
        out
    }

    /// A guard listed as WIRED must actually be consulted. If this
    /// fails, either the wiring was removed or the list is aspirational
    /// — and an aspirational guard is the bug this whole split exists
    /// to make visible.
    #[test]
    fn every_wired_guard_is_consulted_by_the_engine() {
        let src = engine_sources();
        for g in WIRED_GUARDS {
            assert!(
                src.contains(g),
                "`{g}` is listed WIRED but nothing outside guards.rs mentions it"
            );
        }
    }

    /// The reverse: an unwired guard that someone has since hooked up
    /// is now documented as dead while running. Whoever wires it moves
    /// it to WIRED_GUARDS, and this test is how they find out they must.
    #[test]
    fn no_unwired_guard_is_secretly_in_use() {
        let src = engine_sources();
        for g in UNWIRED_GUARDS {
            assert!(
                !src.contains(g),
                "`{g}` is listed UNWIRED but the engine references it — move it to WIRED_GUARDS"
            );
        }
    }

    /// The two lists must not overlap, and must not both claim a name.
    #[test]
    fn the_guard_lists_are_disjoint() {
        for g in UNWIRED_GUARDS {
            assert!(
                !WIRED_GUARDS.contains(g),
                "`{g}` cannot be both wired and unwired"
            );
        }
    }

    /// Failover has exactly ONE source of truth, and this is what keeps
    /// it that way.
    ///
    /// `ProviderError::should_fallback` decides whether an error is
    /// worth moving for; `failover.rs` decides the policy (ask / auto /
    /// stop / script, `[[failover]]` rules, vetoable per-hop hooks). The
    /// second taxonomy that used to live in THIS file — `classify_error`
    /// → `advise` → `TurnRetryState` — was deleted on 2026-09-02 rather
    /// than left switched off, because a dead copy of live logic is not
    /// a mechanism anyone wants later, it is a second answer waiting to
    /// be believed. This test fails if it comes back.
    #[test]
    fn the_live_failover_policy_is_the_providers_one() {
        let agent = include_str!("agent.rs");
        assert!(
            agent.contains("should_fallback"),
            "agent.rs decides failover with ProviderError::should_fallback"
        );
        assert!(
            agent.contains("crate::failover::resolve"),
            "…and the POLICY comes from failover.rs, the programmable one"
        );
        // The duplicate must be gone from this file, not merely unused:
        // `me` is guards.rs itself, so a re-introduction fails here.
        //
        // The needles are BUILT rather than written, because a literal
        // would appear in this very file and the assertion would then
        // fail on itself — the same self-reference that `strip_prose`
        // was written for one audit earlier.
        let me = include_str!("guards.rs");
        for (kind, name) in [
            ("fn", "classify_error"),
            ("fn", "advise"),
            ("struct", "TurnRetryState"),
        ] {
            let needle = format!("pub {kind} {name}");
            assert!(
                !me.contains(&needle),
                "`{needle}` is back — that is a second failover policy, and two \
                 policies that can disagree is exactly what was removed"
            );
        }
    }

    /// The nudge is OFF by default but fully intact — the code path
    /// still works, so `verification_nudge = true` restores it exactly.
    /// A mechanism we judged counter-productive should be switchable,
    /// not amputated.
    #[test]
    fn the_verification_nudge_is_off_by_default_but_still_works() {
        let cfg = crate::config::GuardsSection::default();
        assert!(
            !cfg.verification_nudge,
            "default off: each firing costs a full extra round and writes \
             a message the user never sent"
        );
        assert_eq!(cfg.verification_max_attempts, 2, "the bound is unchanged");

        // Turned on, the policy still produces a nudge for the case it
        // exists for: code edited, no fresh evidence, non-chat surface.
        let n = verification_nudge(&["src/main.rs".to_string()], false, "cli", 0, 2);
        assert!(n.is_some(), "the mechanism itself is intact");
        assert!(
            n.unwrap().contains("src/main.rs"),
            "and still names the file"
        );

        // And it still declines where it always did.
        assert!(
            verification_nudge(&["src/main.rs".to_string()], true, "cli", 0, 2).is_none(),
            "fresh evidence still suppresses it"
        );
        assert!(
            verification_nudge(&["src/main.rs".to_string()], false, "cli", 2, 2).is_none(),
            "and the attempt bound still holds"
        );
    }

    /// `strip_prose` decides whether the two audit tests above are
    /// meaningful, so it gets its own pins: a real call must survive,
    /// a mention in prose must not.
    #[test]
    fn strip_prose_keeps_calls_and_drops_mentions() {
        let src = r#"
// classify_error in a comment
/// classify_error in a doc comment
let msg = "guards::classify_error duplicates the live policy";
let c = classify_error(Some(429), "rate limited");
"#;
        let out = strip_prose(src);
        assert!(
            out.contains("classify_error(Some(429)"),
            "the real call survives: {out}"
        );
        assert_eq!(
            out.matches("classify_error").count(),
            1,
            "and only the call — comments and string content go: {out}"
        );

        // An escaped quote must not flip the in-string state and eat
        // the rest of the line.
        let tricky = r#"let s = "a \" b"; real_call();"#;
        assert!(
            strip_prose(tricky).contains("real_call()"),
            "escaped quotes do not swallow the line"
        );
    }
    /// The foreground-sleep tokenizer: the shapes the live store is full
    /// of are caught, backgrounded and quoted sleeps are not, and short
    /// poll-loop sleeps report their own (small) length.
    #[test]
    fn longest_foreground_sleep_reads_the_shapes_agents_use() {
        use super::longest_foreground_sleep as f;
        assert_eq!(f("sleep 240; tail -c 2000 /tmp/x.log"), Some(240.0));
        assert_eq!(f("cd ~/wt && sleep 420 && grep -c ok log"), Some(420.0));
        assert_eq!(f("sleep 1m 30s"), Some(90.0));
        assert_eq!(f("sleep 2h"), Some(7200.0));
        assert_eq!(f("sleep infinity"), Some(f64::INFINITY));
        assert_eq!(f("X=1 sleep 30"), Some(30.0));
        assert_eq!(f("until curl -sf localhost:1; do sleep 1; done"), Some(1.0));
        assert_eq!(f("if true; then sleep 45; fi"), Some(45.0));
        assert_eq!(f("sleep 2; sleep 90\necho done"), Some(90.0));
        // Not a foreground sleep of this shell.
        assert_eq!(f("sleep 300 & echo started"), None);
        assert_eq!(f("echo sleep 60"), None);
        assert_eq!(f("echo \"sleep 60\""), None);
        assert_eq!(f("bash -c 'sleep 60'"), None);
        assert_eq!(f("grep -r sleep src/"), None);
        assert_eq!(f("cargo test"), None);
        assert_eq!(f("sleep"), None, "no duration, no sleep length");
    }
}
