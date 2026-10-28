//! ChatML control tokens.
//!
//! Two shapes of text hold STRUCTURAL meaning on a reasoning model's
//! wire and therefore must never be emitted literally from a surface we
//! control, or replayed into the next request as if they were ordinary
//! content:
//!
//!   - the pipe-token shape `<|…|>` — the ChatML role markers
//!     (`<|im_start|>`, `<|im_end|>`, `<|system|>`, `<|user|>`,
//!     `<|assistant|>`, `<|tool|>`) plus the thinking delimiters
//!     `<|...|>` / `<|…|>` that bracket raw chain-of-thought;
//!   - think-tag pairs (`<thinking>…</thinking>`, `…`,
//!     `…`) that a model writing into its answer field
//!     uses to mark off hidden deliberation.
//!
//! The hazard (2026-09-12, xenia report): a message that QUOTES such a
//! token literally — the agent's own reply, a tool-call argument, a
//! stored note — is cut over at the token on the way back through the
//! wire: everything before it renders as hidden reasoning, the remainder
//! dumps as ordinary output, and a reasoning model that sees `<|im_start|>`
//! in its input treats it as a prompt-injection boundary rather than
//! prose. The token is unsafe to write and unsafe to receive.
//!
//! Two layers, matching the two directions:
//!   - [`strip_inline_reasoning`] runs on the FINAL answer content and
//!     moves token-wrapped deliberation out of the answer and into
//!     `reasoning`, so it is not replayed, billed and rendered as answer.
//!   - [`defang`] runs at the outbound DISPLAY seam, so a literal token
//!     never reaches a surface (owner surfaces are the host's own debug
//!     view and may show it raw; connector surfaces must not).
//!
//! Both live on one definition of the token shapes rather than two.

use std::sync::OnceLock;

/// Pipe-token shape: `<|` + 1..=32 non-pipe, non-newline chars + `|>`.
/// Matches the role markers AND the thinking delimiters in one rule —
/// the interior is what names a ChatML phrase, and every interior in
/// the family is control. (Never write the literal expansion here; the
/// shorthand load-bears in PENDING.md.)
fn pipe_token() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<\|[^|\r\n]{1,32}\|>").expect("valid regex"))
}

/// Think-tag pairs: the XML-shaped open/close delimiters a reasoning
/// model may emit inside its answer field. Case-insensitive; the three
/// names cover the common families without over-matching ordinary XML.
fn think_tag() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)<(/?)(think|thought|thinking|qwen)\b[^>]*>").expect("valid regex")
    })
}

/// Insert a zero-width space after the opening `<` of a token, so the
/// string still reads as the token to a person but no longer matches as
/// control on the wire. Used by both layers; a single definition means
/// the two cannot disagree on what "defanged" is.
fn define(s: &str) -> String {
    // `<` is always the first byte that must not be a live opener.
    let mut out = String::with_capacity(s.len() + 8);
    let mut chars = s.char_indices();
    let mut first = true;
    for (i, c) in chars.by_ref() {
        if c == '<' {
            out.push('<');
            out.push('\u{200b}'); // zero-width space
            first = false;
            let _ = i;
            break;
        }
        out.push(c);
    }
    for (_, c) in chars {
        out.push(c);
    }
    if first {
        // No '<' — nothing to defang.
        return s.to_string();
    }
    out
}

/// Defang every ChatML control token in `s`: pipe tokens and think-tag
/// pairs are rewritten so they read the same to a human but carry no
/// structural meaning to a model. Everything else — including the exact
/// contents of a fenced code block the author WANTED to show — is left
/// untouched.
pub fn defang(s: &str) -> String {
    if !s.contains('<') {
        return s.to_string();
    }
    let after_pipe =
        pipe_token().replace_all(s, |c: &regex::Captures| define(c.get(0).unwrap().as_str()));
    let out = think_tag().replace_all(&after_pipe, |c: &regex::Captures| {
        define(c.get(0).unwrap().as_str())
    });
    out.into_owned()
}

/// Strip token-wrapped thinking out of a final assistant answer.
///
/// Given the answer field of a completion (or a fully assembled reply),
/// find any `<|...|>`- or think-tag-delimited run and move its INTERIOR
/// into the returned `reasoning`, returning the cleaned answer alongside.
/// An answer with no such tokens is returned unchanged (empty reasoning).
/// A token whose partner never arrives (an unclosed pair) is left alone:
/// only a PAIR demotes its interior; a bare stray token is not evidence.
///
/// This is layer (1) of the hazard fix. The caller is responsible for
/// announcing the move — it must never happen silently (the RM3 labels
/// doctrine): a model thinking into the answer field has its thought
/// RE-CLASSIFIED, which is a visible change, not a clean-up.
pub fn strip_inline_reasoning(content: &str) -> (String, String) {
    if !content.contains('<') {
        return (content.to_string(), String::new());
    }
    // Two shapes, two passes, one contract. The doc above has always
    // promised think-tag pairs as well as pipe tokens; only the pipe
    // half was ever implemented, so a model that bracketed its thinking
    // with `<thinking>…</thinking>` had it replayed, billed and rendered
    // as answer — the exact outcome this function exists to prevent.
    let (content, pair_reasoning) = strip_think_pairs(content);
    let content = content.as_str();
    let re = pipe_token();
    let mut clean = String::with_capacity(content.len());
    let mut reasoning = String::new();
    let mut rest = content;
    while let Some(m) = re.find(rest) {
        clean.push_str(&rest[..m.start()]);
        let after_open = &rest[m.end()..];
        // The thinking pair is any second pipe token: the opener and the
        // closer may differ (open `…` vs close `...`) and still bracket
        // one run of deliberation.
        match re.find(after_open) {
            Some(c) => {
                let interior = &after_open[..c.start()];
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(interior);
                rest = &after_open[c.end()..];
            }
            None => {
                // Unclosed opener: a bare token is not a thinking block.
                clean.push_str(m.as_str());
                clean.push_str(after_open);
                rest = "";
                break;
            }
        }
    }
    clean.push_str(rest);
    if reasoning.is_empty() {
        return (content.to_string(), pair_reasoning);
    }
    if !pair_reasoning.is_empty() {
        reasoning = format!("{pair_reasoning}\n{reasoning}");
    }
    (clean, reasoning)
}

/// The think-tag half of [`strip_inline_reasoning`]: move the interior
/// of every `<thinking>…</thinking>` pair into reasoning.
///
/// Pairs only, matching the pipe-token rule exactly — an opener whose
/// partner never arrives is left alone, because its "interior" is the
/// rest of the answer and demoting that would hide a real reply. An
/// orphan TERMINATOR is not this function's business either; it is wire
/// noise rather than a delimiter, and [`strip_orphan_think_tags`] drops
/// it before this ever runs.
fn strip_think_pairs(content: &str) -> (String, String) {
    let re = think_tag();
    let mut clean = String::with_capacity(content.len());
    let mut reasoning = String::new();
    let mut last = 0usize;
    let mut open: Option<(usize, usize)> = None; // (tag start, interior start)
    for c in re.captures_iter(content) {
        let m = c.get(0).expect("group 0 always matches");
        let closing = c.get(1).map(|g| !g.as_str().is_empty()).unwrap_or(false);
        match (closing, open) {
            (false, None) => open = Some((m.start(), m.end())),
            // A nested or repeated opener: the OUTER one still owns the
            // run, so keep the first and let the interior carry the rest.
            (false, Some(_)) => {}
            (true, Some((tag_start, interior_start))) => {
                clean.push_str(&content[last..tag_start]);
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(content[interior_start..m.start()].trim());
                last = m.end();
                open = None;
            }
            // Unpartnered terminator: not ours (see the doc above).
            (true, None) => {}
        }
    }
    if reasoning.is_empty() {
        return (content.to_string(), String::new());
    }
    clean.push_str(&content[last..]);
    (clean, reasoning)
}

/// Drop think-tag TERMINATORS that never had an opener.
///
/// A reasoning model on an OpenAI-shaped wire is supposed to put its
/// chain of thought in `reasoning_content` and its answer in `content`.
/// Several do both — and then emit the terminator into `content`
/// anyway, so the answer field arrives as a bare `"\n</think>\n\n"`
/// with the actual thought correctly classified beside it.
///
/// Observed on this box from 2026-09-06 through 2026-09-16 across
/// `qwen3.8-flash`, `qwen3.8-max` and `deepseek-v4-*` on the Alibaba
/// token-plan endpoints: 21 stored assistant rows containing `</think>`
/// and **zero** containing `<think>`. Always an orphan closer, never a
/// pair. USER, 2026-09-16: *"there's a verbatim `</think>` printed
/// sometimes in the live transcript."*
///
/// [`strip_inline_reasoning`] cannot help: it demotes the INTERIOR of a
/// pair, and deliberately leaves an unpartnered token alone ("a bare
/// token is not a thinking block") — which is right for an opener,
/// whose interior might be a real answer, and wrong for a closer, which
/// delimits nothing and is pure wire noise.
///
/// So this handles the other half, and only that half:
///   - a closer with no opener before it is DROPPED;
///   - a closer that does have one is left for `strip_inline_reasoning`
///     to demote as the pair it is;
///   - an opener is never touched here.
///
/// Returns the cleaned text and how many terminators were dropped. It
/// matters beyond the eyesore: the tag is persisted and replayed to the
/// model as assistant content on every later round, which is exactly
/// the hazard this module's header describes.
pub fn strip_orphan_think_tags(content: &str) -> (String, usize) {
    if !content.contains('<') {
        return (content.to_string(), 0);
    }
    let mut out = String::with_capacity(content.len());
    let mut last = 0usize;
    let mut depth = 0i32;
    let mut dropped = 0usize;
    for c in think_tag().captures_iter(content) {
        let m = c.get(0).expect("group 0 always matches");
        let closing = c.get(1).map(|g| !g.as_str().is_empty()).unwrap_or(false);
        if !closing {
            depth += 1;
            continue;
        }
        if depth > 0 {
            // Partnered: leave the pair intact for the demotion pass.
            depth -= 1;
            continue;
        }
        // Orphan terminator. Drop it, and with it one adjacent newline
        // run on each side — the wire shape is "\n</think>\n\n", and
        // removing only the tag would leave the answer starting on its
        // third blank line.
        out.push_str(content[last..m.start()].trim_end_matches(['\n', '\r']));
        last = m.end();
        while content[last..].starts_with(['\n', '\r']) {
            last += 1;
        }
        dropped += 1;
    }
    if dropped == 0 {
        return (content.to_string(), 0);
    }
    out.push_str(&content[last..]);
    (out, dropped)
}

/// Does `s` — which starts with `<` and holds no `>` — still have a
/// chance of becoming a think tag once more bytes arrive?
///
/// The streaming half of the problem: a chunk boundary can fall inside
/// the tag (`</thi` then `nk>`), so the display filter has to hold a
/// trailing fragment back. Holding back EVERY unterminated `<` would
/// stall ordinary prose and any code block containing markup, so the
/// question is asked narrowly: is this a prefix of one of the four
/// names, or a complete name in its attribute region?
fn could_become_tag(s: &str) -> bool {
    // A tag this long is not one of ours; stop holding text hostage.
    if s.len() > 64 {
        return false;
    }
    let rest = s.strip_prefix('<').unwrap_or(s);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let lower = rest.to_ascii_lowercase();
    for name in ["think", "thought", "thinking", "qwen"] {
        if name.starts_with(&lower) {
            return true;
        }
        if let Some(after) = lower.strip_prefix(name) {
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                return true;
            }
        }
    }
    false
}

/// A streaming view of [`strip_orphan_think_tags`].
///
/// The live transcript is painted from deltas, so the tag has to go
/// before it is shown — waiting for the final message would mean the
/// user watches it appear and then watches it vanish. Same rules as the
/// whole-string form, applied to one chunk at a time, with a fragment
/// held back when a chunk ends mid-tag.
///
/// It filters only what is DISPLAYED. The accumulated content is left
/// raw on purpose so that the engine has exactly one place that decides
/// what the message IS — `Agent::run_turn`, which strips and announces
/// there. Two strippers disagreeing about a message is worse than one
/// tag.
#[derive(Debug, Default)]
pub struct ThinkTagStream {
    carry: String,
    depth: i32,
}

impl ThinkTagStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one content chunk; returns what should be shown for it.
    pub fn push(&mut self, chunk: &str) -> String {
        let mut buf = std::mem::take(&mut self.carry);
        buf.push_str(chunk);
        let cut = self.hold_from(&buf);
        self.carry = buf[cut..].to_string();
        self.filter(&buf[..cut])
    }

    /// Where to stop showing this buffer, holding the rest back.
    ///
    /// TWO things get held, and the second is the one a first reading
    /// misses. The obvious one is a trailing `<…` that could still
    /// become a tag once more bytes arrive. The other is the NEWLINE RUN
    /// in front of it — and, because a chunk can end between the
    /// newlines and the tag, any trailing newline run at all.
    ///
    /// Without that, `"\n</thi"` + `"nk>\n\nanswer"` shows the leading
    /// newline in the first chunk (nothing suspicious about it yet) and
    /// then cannot take it back, so the answer arrives one blank line
    /// lower than it should — the padding the orphan rule exists to
    /// remove. Delaying a newline by one chunk costs nothing; `finish`
    /// releases whatever is still held.
    fn hold_from(&self, buf: &str) -> usize {
        let mut cut = buf.len();
        if let Some(lt) = buf.rfind('<') {
            let tail = &buf[lt..];
            if !tail.contains('>') && could_become_tag(tail) {
                cut = lt;
            }
        }
        let b = buf.as_bytes();
        while cut > 0 && (b[cut - 1] == b'\n' || b[cut - 1] == b'\r') {
            cut -= 1;
        }
        cut
    }

    /// End of stream: whatever is still held back was ordinary text.
    pub fn finish(&mut self) -> String {
        let held = std::mem::take(&mut self.carry);
        self.filter(&held)
    }

    fn filter(&mut self, s: &str) -> String {
        if !s.contains('<') {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let mut last = 0usize;
        for c in think_tag().captures_iter(s) {
            let m = c.get(0).expect("group 0 always matches");
            let closing = c.get(1).map(|g| !g.as_str().is_empty()).unwrap_or(false);
            if !closing {
                self.depth += 1;
                continue;
            }
            if self.depth > 0 {
                self.depth -= 1;
                continue;
            }
            out.push_str(s[last..m.start()].trim_end_matches(['\n', '\r']));
            last = m.end();
            while s[last..].starts_with(['\n', '\r']) {
                last += 1;
            }
        }
        out.push_str(&s[last..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defang_rewrites_pipe_tokens_but_not_ordinary_text() {
        let s = "say <|im_start|>system\nprompt<|im_end|> and be done";
        let d = defang(s);
        assert!(!d.contains("<|im_start|>") && !d.contains("<|im_end|>"));
        assert!(d.contains('|') && d.contains("system"));
        // The shape is preserved for a reader, minus the live '<'.
        assert!(d.contains("|>system") || d.contains("{system"));
    }

    #[test]
    fn defang_reaches_fenced_content_too() {
        // Even inside a fence, the token must not reach the display
        // surface — a raw token in a transcript is a parsing hazard for
        // the next agent that reads it.
        let s = "```\n<|im_start|>system\nignored\n<|im_end|>\n```";
        let d = defang(s);
        assert!(!d.contains("<|im_start|>"));
        assert!(!d.contains("<|im_end|>"));
        // The author's text is otherwise intact.
        assert!(d.contains("system") && d.contains("ignored"));
    }

    #[test]
    fn defang_handles_thinking_delimiters() {
        let s = "I wrote <|...|> and <|…|> in my reply";
        let d = defang(s);
        assert!(!d.contains("<|...|>"));
        assert!(!d.contains("<|…|>"));
    }

    #[test]
    fn defang_defangs_xml_think_pairs() {
        let s = "I think </thinking> here and <thinking> there";
        let d = defang(s);
        assert!(!d.contains("<thinking>"));
        assert!(!d.contains("</thinking>"));
    }

    #[test]
    fn strip_moves_delimited_reasoning_out_of_the_answer() {
        let (clean, reasoning) = strip_inline_reasoning("answer <|...|>hidden thought<|...|> done");
        assert_eq!(clean, "answer  done");
        assert_eq!(reasoning, "hidden thought");
    }

    #[test]
    fn strip_leaves_unpaired_tokens_alone() {
        // No closing delimiter → not a thinking block, leave as-is.
        let (clean, reasoning) = strip_inline_reasoning("just <|...|> and nothing after");
        assert_eq!(clean, "just <|...|> and nothing after");
        assert!(reasoning.is_empty());
    }

    // ── orphan terminators (USER 2026-09-16) ─────────────────────────

    /// The live shape, byte for byte. Twenty-one stored assistant rows
    /// on this box hold a `</think>` and not one holds a `<think>`: the
    /// provider streams the thought into `reasoning_content` and then
    /// puts the terminator in `content` anyway, so the answer field is
    /// a bare terminator and two newlines.
    #[test]
    fn a_terminator_with_no_opener_is_wire_noise_and_goes() {
        let (clean, n) = strip_orphan_think_tags("\n</think>\n\nThe identity check is dead code.");
        assert_eq!(n, 1);
        assert_eq!(
            clean, "The identity check is dead code.",
            "the tag goes and takes its newline padding with it — leaving \
             them would start every answer on its third blank line"
        );

        // The commonest case of all: the whole content IS the tag.
        let (empty, n) = strip_orphan_think_tags("\n</think>\n\n");
        assert_eq!(n, 1);
        assert_eq!(empty, "", "nothing was said, and nothing is what shows");
    }

    /// The other half of the rule, and the reason this is not just
    /// "delete think tags". An OPENER alone still means the text after
    /// it may be a real answer, so it is left exactly where the
    /// pipe-token rule leaves its own unpartnered token.
    #[test]
    fn an_opener_with_no_terminator_is_still_left_alone() {
        let s = "<think> and then I never closed it";
        let (clean, n) = strip_orphan_think_tags(s);
        assert_eq!(n, 0);
        assert_eq!(clean, s);
    }

    /// A real pair belongs to `strip_inline_reasoning`, which demotes
    /// its interior. This pass must not eat the closer out from under
    /// it — that would leave an unpartnered opener and turn a
    /// re-classifiable thought into permanent answer text.
    #[test]
    fn a_partnered_terminator_is_left_for_the_demotion_pass() {
        let s = "<thinking>weighing it up</thinking>the answer";
        let (clean, n) = strip_orphan_think_tags(s);
        assert_eq!(n, 0, "the pair is intact, so there is no orphan");
        assert_eq!(clean, s);

        let (demoted, reasoning) = strip_inline_reasoning(s);
        assert_eq!(demoted, "the answer");
        assert_eq!(reasoning, "weighing it up");
    }

    /// The doc for `strip_inline_reasoning` has promised think-tag
    /// pairs since it was written; only the pipe half existed. A model
    /// that brackets its deliberation this way had it replayed, billed
    /// and rendered as its reply.
    #[test]
    fn think_pairs_are_demoted_like_pipe_pairs_and_both_can_appear() {
        let (clean, reasoning) =
            strip_inline_reasoning("<think>first</think>answer <|...|>second<|...|> done");
        assert_eq!(clean, "answer  done");
        assert_eq!(
            reasoning, "first\nsecond",
            "both shapes move, in the order they were written"
        );
    }

    /// Streaming: the transcript is painted from deltas, so the tag has
    /// to be gone before it is shown — and a chunk boundary can fall
    /// anywhere, including inside the tag. This is the case a
    /// whole-string stripper cannot cover.
    #[test]
    fn a_terminator_split_across_two_chunks_is_still_caught() {
        let mut f = ThinkTagStream::new();
        let mut shown = String::new();
        for chunk in ["\n</thi", "nk>\n\nThe ident", "ity check is dead code."] {
            shown.push_str(&f.push(chunk));
        }
        shown.push_str(&f.finish());
        assert_eq!(shown, "The identity check is dead code.");
    }

    /// …and the fragment it holds back must be released when it turns
    /// out to be ordinary text, or a code block would swallow its own
    /// tail. `<div` is not a prefix of any think tag and must not even
    /// be held.
    #[test]
    fn markup_that_is_not_a_think_tag_streams_through_untouched() {
        let mut f = ThinkTagStream::new();
        let mut shown = String::new();
        for chunk in ["use <div", " class=\"x\">, and ", "compare a < b"] {
            shown.push_str(&f.push(chunk));
        }
        shown.push_str(&f.finish());
        assert_eq!(shown, "use <div class=\"x\">, and compare a < b");

        // A held-back fragment that never completes is text after all.
        let mut g = ThinkTagStream::new();
        let mut out = g.push("the tag is <think");
        out.push_str(&g.finish());
        assert_eq!(out, "the tag is <think");
    }

    /// The stream filter and the whole-string form must agree about a
    /// pair, or the live transcript and the reopened one would differ
    /// by more than the artifact — which is the defect the transcript
    /// spec exists to prevent.
    #[test]
    fn the_stream_filter_leaves_a_pair_exactly_as_the_whole_string_form_does() {
        let s = "<thinking>weighing it up</thinking>the answer";
        let mut f = ThinkTagStream::new();
        let mut shown = f.push(s);
        shown.push_str(&f.finish());
        assert_eq!(shown, s, "a pair survives the display pass intact");
        assert_eq!(strip_orphan_think_tags(s).0, shown);
    }

    #[test]
    fn strip_without_tokens_is_untouched() {
        let (clean, reasoning) = strip_inline_reasoning("plain answer");
        assert_eq!(clean, "plain answer");
        assert!(reasoning.is_empty());
    }
}
