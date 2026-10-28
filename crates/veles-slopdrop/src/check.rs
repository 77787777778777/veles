//! The local OUTPUT check for a public `/tcl` reply (plan §2).
//!
//! WHY THIS MODULE EXISTS
//!
//! The prebake rated a proc by its SOURCE and never saw its output — and a
//! third-party endpoint decided it. This module replaces that with a cheap,
//! LOCAL check of the exact text the public post will carry, run on the
//! VPS CPU. No third-party endpoint. When the scorer is off, the check
//! fails CLOSED to link-only, fast: "classifier off" must never mean "skip
//! the check", because the check is the only thing that lets a composed,
//! ad-hoc or stateful result inline at all.
//!
//! The order (the first rule that fails decides):
//!   1. Owner-approved text (sha256 on the allowlist) → pass untouched.
//!      The hash compare is the CALLER's (`slopdrop_safety::allowlisted`);
//!      [`run`] takes the verdict as a bool so the approve path can never
//!      skip the compare by accident.
//!   2. Deterministic gates ([`deterministic`]) — no model call. Art and
//!      shape, links and people, and the folded denylist. Any hit = withheld.
//!   3. The scorer ([`run`]) — the sidecar's sigmoid heads on the text.
//!
//! False negatives (a withheld benign line) are fine; a wrongly inline post
//! is not, so every doubt resolves to withheld.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sha2::Digest as _;

/// The outcome of the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// Why it was withheld (shown to the owner in the withheld question).
    Withheld(String),
}

impl Outcome {
    pub fn pass(&self) -> bool {
        matches!(self, Outcome::Pass)
    }
    pub fn reason(&self) -> Option<&str> {
        match self {
            Outcome::Pass => None,
            Outcome::Withheld(r) => Some(r),
        }
    }
}

/// sha256 hex of the exact text — the allowlist/blocked-list key.
pub fn text_hash(text: &str) -> String {
    let mut h = sha2::Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

// ── the deterministic gates ─────────────────────────────────────────────

/// The 7 toxicity heads and the 9 identity-mention heads the scorer
/// reports, in the sidecar's label order (labels.json).
pub const TOX_HEADS: [&str; 7] = [
    "toxicity",
    "severe_toxicity",
    "obscene",
    "identity_attack",
    "insult",
    "threat",
    "sexual_explicit",
];
pub const ID_HEADS: [&str; 9] = [
    "male",
    "female",
    "homosexual_gay_or_lesbian",
    "christian",
    "jewish",
    "muslim",
    "black",
    "white",
    "psychiatric_or_mental_illness",
];

/// The fold: lowercase, leet map, separators removed. A slur spelled
/// "n 1 g g 3 r" or "N1GG3R" folds to "nigger", so the denylist holds one
/// folded term per slur and catches every spelling.
///
/// MAP FIRST, then drop separators: `@` and `$` are leet letters, not
/// separators. Filtering first threw them away before the map could read
/// them, so "n@zi" folded to "nzi" and walked past the list.
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c.to_ascii_lowercase() {
            '0' => 'o',
            '1' => 'i',
            '3' => 'e',
            '4' => 'a',
            '5' => 's',
            '7' => 't',
            '@' => 'a',
            '$' => 's',
            other => other,
        })
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// The denylist terms: the repo seed (`include_str!`) plus the operator's
/// own file at `<state>/denylist.txt`, each already folded.
fn denylist() -> Vec<String> {
    let mut out: Vec<String> = include_str!("denylist.txt")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(fold)
        .collect();
    let op = crate::data::dir().join("denylist.txt");
    if let Ok(body) = std::fs::read_to_string(op) {
        out.extend(
            body.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(fold),
        );
    }
    out
}

/// Looks like a URL, a bare domain, an @handle, an email, a phone-like
/// digit run, or an IPv4 address. The public post must not carry a link or
/// a way to reach a person.
///
/// `mentions` are the `@handle`s a Bluesky reply's spec may carry
/// (`bsky::validate_spec`: the caller and the thread's own participants —
/// people already in this conversation). Exactly those words pass; every
/// other handle still withholds. Before this the gate read every
/// `@handle` as a stranger, so `bsky::mention` could never go out inline
/// (found 2026-10-11 with the test account).
fn link_or_contact(text: &str, mentions: &[String]) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("www.")
        || lower.contains("ftp.")
    {
        return true;
    }
    for w in text.split_whitespace() {
        let w = w.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '.' && c != '@' && c != '-' && c != '#'
        });
        if w.is_empty() {
            continue;
        }
        // email
        if w.matches('@').count() == 1 && w.contains('.') && !w.starts_with('@') {
            return true;
        }
        // @handle
        if w.starts_with('@') && w.len() > 1 {
            let bare = w.trim_end_matches('.');
            if mentions.iter().any(|m| m.eq_ignore_ascii_case(bare)) {
                continue;
            }
            return true;
        }
        // bare domain: label.tld with a known-ish tld
        if !w.contains('@') && w.matches('.').count() >= 1 {
            let tld = w.rsplit('.').next().unwrap_or("");
            let head = w.split('.').next().unwrap_or("");
            if tld.len() >= 2
                && tld.chars().all(|c| c.is_ascii_alphabetic())
                && head.len() >= 2
                && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return true;
            }
        }
        // IPv4
        let octets: Vec<&str> = w.split('.').collect();
        if octets.len() == 4 && octets.iter().all(|o| o.parse::<u8>().is_ok()) {
            return true;
        }
        // phone-like: 7+ digits with only phone punctuation
        let digits = w.chars().filter(|c| c.is_ascii_digit()).count();
        if digits >= 7
            && w.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | '(' | ')' | '+' | '.' | ' '))
        {
            return true;
        }
    }
    false
}

/// The deterministic gates. Returns `Withheld(reason)` on the first gate
/// that trips, else `Pass`. Pure, no network — the only IO is the optional
/// denylist operator file.
pub fn deterministic(text: &str) -> Outcome {
    deterministic_with(text, &[])
}

/// [`deterministic`], with the `@handle`s a reply may carry (see
/// `link_or_contact`).
pub fn deterministic_with(text: &str, mentions: &[String]) -> Outcome {
    // Art and shape.
    if text
        .chars()
        .any(|c| !c.is_ascii() || (c.is_control() && c != '\n' && c != '\t'))
    {
        return Outcome::Withheld("art or non-ascii".into());
    }
    if text.lines().count() > 20 {
        return Outcome::Withheld("too many lines".into());
    }
    let total = text.chars().count();
    if total > 0 {
        let good = text
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
            .count();
        if (good * 4) < (total * 3) {
            return Outcome::Withheld("mostly symbols".into());
        }
    }
    let mut run = (None::<char>, 0usize);
    for c in text.chars() {
        if !c.is_ascii_alphanumeric() && !c.is_whitespace() {
            if Some(c) == run.0 {
                run.1 += 1;
                if run.1 >= 3 {
                    return Outcome::Withheld("symbol run".into());
                }
            } else {
                run = (Some(c), 1);
            }
        } else {
            run = (None, 0);
        }
    }
    // Links and people.
    if link_or_contact(text, mentions) {
        return Outcome::Withheld("link or contact".into());
    }
    if denylist_hit(text) {
        return Outcome::Withheld("denylist".into());
    }
    Outcome::Pass
}

/// The denylist, on the whole folded text and on the acrostic (the folded
/// first letters of each line).
pub fn denylist_hit(text: &str) -> bool {
    let terms = denylist();
    if terms.is_empty() {
        return false;
    }
    let whole = fold(text);
    let acrostic: String = text
        .lines()
        .filter_map(|l| l.trim_start().chars().next())
        .collect::<String>();
    let acrostic = fold(&acrostic);
    terms.iter().filter(|t| !t.is_empty()).any(|t| {
        whole.contains(t.as_str()) || (!acrostic.is_empty() && acrostic.contains(t.as_str()))
    })
}

/// The checks that read WORDS, not shape (PLAN-slopdrop-art.md P5): links
/// and contact details, the denylist, then the scorer. For text that is
/// not itself the post: the words a picture shows, the text beside it, a
/// quoted post, a long text drawn as a picture. The art and shape gates
/// would read a drawing's words or a line of code as noise. Nothing to
/// read passes. The scorer reads at most 512 tokens and a longer text
/// comes back `truncated` (withheld), so the text is scored in pieces of
/// [`SCORE_CHUNK_CHARS`]; the first piece that fails decides.
pub async fn words_detailed(
    endpoints: &[String],
    text: &str,
    tox: f64,
    id: f64,
    mentions: &[String],
) -> Checked {
    let plain = |outcome| Checked {
        outcome,
        top: None,
        ms: None,
    };
    if link_or_contact(text, mentions) {
        return plain(Outcome::Withheld("link or contact".into()));
    }
    if denylist_hit(text) {
        return plain(Outcome::Withheld("denylist".into()));
    }
    if text.trim().is_empty() {
        return plain(Outcome::Pass);
    }
    let mut passed = plain(Outcome::Pass);
    for piece in score_chunks(text, SCORE_CHUNK_CHARS) {
        let c = score(endpoints, piece, tox, id).await;
        if !c.outcome.pass() {
            return c;
        }
        let higher = match (&c.top, &passed.top) {
            (Some((_, a)), Some((_, b))) => a > b,
            (Some(_), None) => true,
            _ => false,
        };
        if higher {
            passed = c;
        }
    }
    passed
}

/// A piece of text the scorer reads whole: about 150 tokens of English,
/// and a line of symbols tokenises at about one per character, so 600
/// stays under the sidecar's 512 either way.
pub const SCORE_CHUNK_CHARS: usize = 600;

/// `text` in pieces of at most `max` characters, split at line ends where
/// a line fits and inside a line where one does not. Every character is in
/// exactly one piece.
pub fn score_chunks(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0; // byte offset of the current piece
    let mut chars = 0; // its length in characters
    let mut last_break = None; // byte offset just past its last newline
    for (i, c) in text.char_indices() {
        if chars == max {
            let cut = last_break.filter(|&b| b > start).unwrap_or(i);
            out.push(&text[start..cut]);
            chars = text[cut..i].chars().count();
            start = cut;
            last_break = None;
        }
        chars += 1;
        if c == '\n' {
            last_break = Some(i + 1);
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

// ── the scorer ──────────────────────────────────────────────────────────

/// An endpoint that errored is skipped until this long has passed, so a
/// dead box costs nothing on later calls.
const ENDPOINT_SKIP: std::time::Duration = std::time::Duration::from_secs(30);

fn failed_endpoints() -> &'static Mutex<HashMap<String, std::time::Instant>> {
    static M: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What one check found: the outcome, plus what the gateway logs about
/// it (never the text): the highest toxicity head the scorer reported and
/// how long the scorer took. Both `None` when no scorer answered (or none
/// was asked: the allowlist or a deterministic gate decided).
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub outcome: Outcome,
    pub top: Option<(String, f64)>,
    pub ms: Option<u64>,
}

/// The full check: allowlist → deterministic → scorer. `allowlisted` is the
/// caller's verdict on the text's hash (the compare happens there, never
/// skipped here). Endpoints are tried in order; no answer = withheld.
pub async fn run(
    endpoints: &[String],
    text: &str,
    allowlisted: bool,
    tox: f64,
    id: f64,
) -> Outcome {
    run_detailed(endpoints, text, allowlisted, tox, id, &[])
        .await
        .outcome
}

/// [`run`], with the scorer's top head and latency for the log line.
pub async fn run_detailed(
    endpoints: &[String],
    text: &str,
    allowlisted: bool,
    tox: f64,
    id: f64,
    mentions: &[String],
) -> Checked {
    let plain = |outcome| Checked {
        outcome,
        top: None,
        ms: None,
    };
    if allowlisted {
        return plain(Outcome::Pass);
    }
    let det = deterministic_with(text, mentions);
    if !det.pass() {
        return plain(det);
    }
    score(endpoints, text, tox, id).await
}

fn mark_failed(ep: &str) {
    failed_endpoints()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(ep.to_string(), std::time::Instant::now());
}

/// POST the text to each live endpoint in order; the first answer decides.
///
/// Only a SUCCESSFUL answer can pass. A transport error, a 5xx or an
/// unreadable body marks the endpoint failed (skipped 30 s) and the next
/// one is tried; a 4xx is the scorer refusing THIS text, so it withholds
/// at once. An error reply used to pass: its `{"error":…}` body has no
/// heads, and every missing head read as 0.0.
async fn score(endpoints: &[String], text: &str, tox: f64, id: f64) -> Checked {
    let unavailable = Checked {
        outcome: Outcome::Withheld("classifier unavailable".into()),
        top: None,
        ms: None,
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(300))
        .timeout(std::time::Duration::from_millis(1500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return unavailable,
    };
    for ep in endpoints {
        let ep = ep.trim_end_matches('/');
        if ep.is_empty() {
            continue;
        }
        // Skip an endpoint that failed within the last 30 s.
        {
            let failed = failed_endpoints().lock().unwrap_or_else(|p| p.into_inner());
            if let Some(at) = failed.get(ep) {
                if at.elapsed() < ENDPOINT_SKIP {
                    continue;
                }
            }
        }
        let t0 = std::time::Instant::now();
        let resp = match client
            .post(format!("{ep}/score"))
            .json(&serde_json::json!({"text": text}))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => {
                mark_failed(ep);
                continue;
            }
        };
        let status = resp.status();
        if status.is_server_error() {
            mark_failed(ep);
            continue;
        }
        if !status.is_success() {
            return Checked {
                outcome: Outcome::Withheld(format!("classifier refused ({})", status.as_u16())),
                top: None,
                ms: Some(t0.elapsed().as_millis() as u64),
            };
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => {
                mark_failed(ep);
                continue;
            }
        };
        let mut out = judge(&body, tox, id);
        out.ms = Some(t0.elapsed().as_millis() as u64);
        return out;
    }
    unavailable
}

/// Read one endpoint's answer: fail on a toxicity head ≥ tox or an identity
/// head ≥ id. There is deliberately NO tokens-per-word guard: it kept
/// false-positiving on benign output (a terse "340" = 3.0 t/w, then
/// "a small test." x10 = 2.0 t/w, both withheld live), and the class it was
/// meant to catch — non-English — is already rejected by the deterministic
/// non-ASCII gate, so it added only false positives. False negatives are
/// fine, false positives are dangerous (USER 2026-10-06/07).
///
/// An answer is only READ when it is whole: all 16 heads present as
/// numbers, and `truncated: false` (the sidecar scored every token). A
/// missing head is not a zero, and an unscored tail is not a clean one —
/// either is withheld "scorer reply incomplete".
fn judge(body: &serde_json::Value, tox: f64, id: f64) -> Checked {
    let incomplete = Checked {
        outcome: Outcome::Withheld("scorer reply incomplete".into()),
        top: None,
        ms: None,
    };
    if body.get("truncated").and_then(|v| v.as_bool()) != Some(false) {
        return incomplete;
    }
    let Some(scores) = body.get("scores") else {
        return incomplete;
    };
    let mut heads: Vec<(&str, f64)> = Vec::new();
    for h in TOX_HEADS.iter().chain(ID_HEADS.iter()) {
        match scores.get(*h).and_then(|v| v.as_f64()) {
            Some(p) if p.is_finite() => heads.push((h, p)),
            _ => return incomplete,
        }
    }
    let top = heads[..TOX_HEADS.len()]
        .iter()
        .copied()
        .fold(None::<(&str, f64)>, |best, (h, p)| match best {
            Some((_, b)) if b >= p => best,
            _ => Some((h, p)),
        })
        .map(|(h, p)| (h.to_string(), p));
    let outcome = if let Some((h, _)) = heads[..TOX_HEADS.len()].iter().find(|(_, p)| *p >= tox) {
        Outcome::Withheld(format!("toxicity head {h}"))
    } else if let Some((h, _)) = heads[TOX_HEADS.len()..].iter().find(|(_, p)| *p >= id) {
        Outcome::Withheld(format!("identity head {h}"))
    } else {
        Outcome::Pass
    };
    Checked {
        outcome,
        top,
        ms: None,
    }
}

// ── the whole public decision ───────────────────────────────────────────

/// What the gateway posts for one public `/tcl` result, and the line it
/// logs about the decision (never the text).
pub struct PublicDecision {
    pub shaped: crate::safety::ShapedPublic,
    pub log: String,
}

/// Decide one public `/tcl` reply end to end: the owner's lists, the bad
/// list through what the line reaches, the output check on the EXACT text
/// the post would carry, then the shaping. `idx` is the bridge's proc
/// index; `None` (the bridge did not answer) fails closed.
///
/// This lives here, not in the gateway, so it can be tested whole: the
/// first version checked the RAW output (with its color codes) while the
/// post carried something else, and let a line that wraps a bad-list proc
/// reach the scorer alone — both in glue no test covered.
pub async fn decide_public(
    cfg: &crate::settings::SlopdropSection,
    code: &str,
    output: &str,
    hash: Option<&str>,
    idx: Option<&crate::safety::ProcIndex>,
) -> PublicDecision {
    decide_public_with(cfg, code, output, hash, idx, &[]).await
}

/// [`decide_public`] for a reply whose spec may carry `mentions` (the
/// `@handle`s `bsky::allowed_mentions` returns): the people gate lets
/// exactly those through. Everything else is decided the same.
pub async fn decide_public_with(
    cfg: &crate::settings::SlopdropSection,
    code: &str,
    output: &str,
    hash: Option<&str>,
    idx: Option<&crate::safety::ProcIndex>,
    mentions: &[String],
) -> PublicDecision {
    use crate::safety;
    let out_hash = text_hash(output);
    let allowlisted = safety::allowlisted(&out_hash);
    let blocked_output = safety::blocked(&out_hash);
    let bad_reach = match idx {
        Some(i) => i
            .bad_reach(code, &safety::verdicts())
            .map(|n| format!("reaches bad-list proc {n}")),
        None => Some("the proc index is unavailable".to_string()),
    };
    let checked = if blocked_output || allowlisted || bad_reach.is_some() {
        None
    } else {
        let pt = safety::public_text(output);
        Some(
            run_detailed(
                &cfg.output_classifier,
                &pt.body,
                false,
                cfg.output_classifier_tox.unwrap_or(0.30),
                cfg.output_classifier_id.unwrap_or(0.25),
                mentions,
            )
            .await,
        )
    };
    let log = match &checked {
        Some(c) => format!(
            "tcl: output check {} ({}), top {}, scorer {}",
            if c.outcome.pass() { "pass" } else { "withheld" },
            c.outcome.reason().unwrap_or("clean"),
            c.top
                .as_ref()
                .map(|(h, p)| format!("{h}={p:.3}"))
                .unwrap_or_else(|| "-".into()),
            c.ms.map(|m| format!("{m} ms"))
                .unwrap_or_else(|| "not asked".into()),
        ),
        None => format!(
            "tcl: output check not run ({})",
            if blocked_output {
                "blocked output".to_string()
            } else if allowlisted {
                "allowlisted output".to_string()
            } else {
                bad_reach.clone().unwrap_or_default()
            }
        ),
    };
    let check = if checked.as_ref().is_some_and(|c| c.outcome.pass()) {
        safety::CheckOutcome::Pass
    } else {
        safety::CheckOutcome::Withheld
    };
    // Shaping reads the verdict store and renders the hosted page: off the
    // async workers.
    let (code_s, out_s, hash_s) = (
        code.to_string(),
        output.to_string(),
        hash.map(str::to_string),
    );
    let shaped = veles_core::rt::spawn_blocking(move || {
        safety::shape_for_public(
            &code_s,
            &out_s,
            &safety::ShapeInputs {
                hash: hash_s.as_deref(),
                blocked_output,
                allowlisted,
                bad_reach,
            },
            check,
        )
    })
    .await;
    match shaped {
        Ok(shaped) => PublicDecision { shaped, log },
        // Fail CLOSED: a shaping task that died must never put the RAW
        // output on a public feed. The owner is asked, as for any other
        // reply that was not let out.
        Err(e) => PublicDecision {
            shaped: safety::ShapedPublic {
                text: "Output withheld: the content check failed.".to_string(),
                artifacts: None,
                kind: crate::withheld::PostKind::Withheld,
                reason: "the content check failed".to_string(),
                proc_hash: None,
            },
            log: format!("{log}; shaping failed: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_benign_ascii_one_liner_passes() {
        assert!(deterministic("the build is green, 42 tests pass").pass());
    }

    #[test]
    fn art_and_shape_gate_trips() {
        assert!(!deterministic("┌───┐\n│ x │\n└───┘").pass(), "box drawing");
        assert!(!deterministic("hello 🎉").pass(), "emoji");
        assert!(!deterministic("esc\x1b[31m here").pass(), "ESC");
        // 21+ lines still trips; the cap is 20 after the 2026-10-07 relax.
        let many = (0..21)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!deterministic(&many).pass(), "21 lines");
        assert!(
            !deterministic("!@#$%^&*()_+ !@#$%^&*()").pass(),
            "mostly symbols"
        );
        assert!(!deterministic("draw ==== here").pass(), "symbol run");
        assert!(!deterministic("nіgger").pass(), "a homoglyph is non-ascii");
    }

    /// 2026-10-07 relax: a benign multi-line pyramid (the `puppies` case)
    /// and an indented block pass the shape gates now — the non-ASCII gate
    /// still catches real art, and the scorer judges the words. False
    /// negatives fine; false positives are the danger.
    #[test]
    fn a_benign_pyramid_and_an_indented_block_pass_the_shape_gates() {
        // The real `each [.. 15] ... puppies` output: line 0 "puppies",
        // line n = n leading spaces + the letters joined by n spaces.
        let mut pyramid = String::from("puppies");
        for n in 1..15usize {
            let pad = " ".repeat(n);
            pyramid.push('\n');
            pyramid.push_str(&pad);
            let letters: Vec<String> = "puppies".chars().map(|c| c.to_string()).collect();
            pyramid.push_str(&letters.join(&pad));
        }
        assert_eq!(pyramid.lines().count(), 15, "15 lines, under the 20 cap");
        assert!(
            deterministic(&pyramid).pass(),
            "a letter+space pyramid is not withheld by shape"
        );
        assert!(
            deterministic("code:\n    indented line").pass(),
            "a 4-space indent no longer trips the gate"
        );
    }

    #[test]
    fn link_and_contact_gate_trips() {
        assert!(!deterministic("see https://example.com/x").pass(), "url");
        assert!(
            !deterministic("go to example.com now").pass(),
            "bare domain"
        );
        assert!(!deterministic("@someone.bsky.social hi").pass(), "handle");
        assert!(!deterministic("write foo@example.com").pass(), "email");
        assert!(!deterministic("call 555-123-4567").pass(), "phone");
        assert!(!deterministic("host 192.168.0.1 up").pass(), "ipv4");
    }

    #[test]
    fn the_fold_catches_leet_spaced_and_acrostic() {
        assert!(
            !deterministic("n@zi").pass(),
            "@ is a leet a, not a separator"
        );
        assert!(!deterministic("N@Z1").pass(), "mixed leet");
        assert_eq!(fold("$h1t @ll"), "shitall", "$ and @ map before the filter");
        assert!(!deterministic("n1gg3r").pass(), "leet");
        assert!(!deterministic("n i g g e r").pass(), "spaced");
        // acrostic: first letters of the lines spell a denylist term
        assert!(
            !deterministic("Never\nalways\nZones\nInside").pass(),
            "acrostic nazi"
        );
    }

    /// The polarity the relax must never cross (USER 2026-10-07): a false
    /// negative (an innocent post withheld) is recoverable by approve; a
    /// false POSITIVE (hate on the public feed) is the failure. These must
    /// stay withheld no matter how the shape gates / thresholds loosen.
    #[test]
    fn hate_and_hate_symbols_are_always_withheld() {
        assert!(!deterministic("卐").pass(), "a swastika is non-ascii art");
        assert!(!deterministic("a 卐 b").pass(), "swastika in context");
        assert!(!deterministic("kill all niggers").pass(), "a slur");
        assert!(
            !deterministic("N1GG3RS\nD13\nN0W").pass(),
            "leet slur across lines"
        );
    }

    #[test]
    fn text_hash_is_stable_and_hex64() {
        let h = text_hash("hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, text_hash("hello"));
        assert_ne!(h, text_hash("hello!"));
    }

    /// The people gate lets through exactly the `@handle`s a reply's spec
    /// may carry, a sentence's full stop included, and nothing else.
    #[test]
    fn an_allowed_mention_passes_the_people_gate_and_nothing_else_does() {
        let m = vec!["@tester.example".to_string()];
        assert!(deterministic_with("hi @tester.example", &m).pass());
        assert!(deterministic_with("hi @Tester.Example.", &m).pass());
        assert!(
            !deterministic("hi @tester.example").pass(),
            "no list, no pass"
        );
        assert!(
            !deterministic_with("hi @tester.example and @someone.else", &m).pass(),
            "another handle still withholds"
        );
        assert!(
            !deterministic_with("@tester.example https://evil.test", &m).pass(),
            "a link still withholds"
        );
        assert!(
            !deterministic_with("mail tester@example.com", &m).pass(),
            "an email is not a mention"
        );
    }

    /// Serve one HTTP POST with a fixed JSON body; return the bound port.
    fn stub_scorer(body: &'static str) -> u16 {
        stub_scorer_status("200 OK", body)
    }

    /// [`stub_scorer`] with a chosen status line (an error reply).
    fn stub_scorer_status(status: &'static str, body: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        port
    }

    fn heads(tox: f64) -> String {
        let mut m = serde_json::Map::new();
        for h in TOX_HEADS {
            m.insert(h.to_string(), serde_json::json!(tox));
        }
        for h in ID_HEADS {
            m.insert(h.to_string(), serde_json::json!(0.0));
        }
        serde_json::json!({"model":"m","scores":m,"tokens":5,"truncated":false,"ms":1}).to_string()
    }

    /// The scorer's ERROR replies never pass. A 4xx/5xx body carries no
    /// heads, and every missing head used to read as 0.0 — so the sidecar
    /// refusing a text (400) or failing on it (500) posted it inline.
    #[tokio::test]
    async fn an_error_reply_from_the_scorer_is_withheld() {
        let port = stub_scorer_status("400 Bad Request", r#"{"error":"bad request: x"}"#);
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "have a nice day", false, 0.30, 0.25).await;
        assert!(!out.pass(), "a 400 is not a pass: {out:?}");

        let port = stub_scorer_status("500 Internal Server Error", r#"{"error":"boom"}"#);
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "have a nice day", false, 0.30, 0.25).await;
        assert_eq!(
            out.reason(),
            Some("classifier unavailable"),
            "a 500: {out:?}"
        );
    }

    /// A 200 that is not a whole answer is withheld too: a missing head
    /// is not a zero, and a text the sidecar truncated was not fully read.
    #[tokio::test]
    async fn an_incomplete_or_truncated_answer_is_withheld() {
        let mut v: serde_json::Value = serde_json::from_str(&heads(0.0)).unwrap();
        v["scores"].as_object_mut().unwrap().remove("threat");
        let port = stub_scorer(Box::leak(v.to_string().into_boxed_str()));
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "have a nice day", false, 0.30, 0.25).await;
        assert_eq!(
            out.reason(),
            Some("scorer reply incomplete"),
            "missing head: {out:?}"
        );

        let mut v: serde_json::Value = serde_json::from_str(&heads(0.0)).unwrap();
        v["truncated"] = serde_json::json!(true);
        let port = stub_scorer(Box::leak(v.to_string().into_boxed_str()));
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "have a nice day", false, 0.30, 0.25).await;
        assert_eq!(
            out.reason(),
            Some("scorer reply incomplete"),
            "truncated: {out:?}"
        );

        let mut v: serde_json::Value = serde_json::from_str(&heads(0.0)).unwrap();
        v.as_object_mut().unwrap().remove("truncated");
        let port = stub_scorer(Box::leak(v.to_string().into_boxed_str()));
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "have a nice day", false, 0.30, 0.25).await;
        assert!(
            !out.pass(),
            "an answer that does not say it read everything: {out:?}"
        );
    }

    #[tokio::test]
    async fn a_clean_text_passes_and_a_toxic_text_is_withheld() {
        let port = stub_scorer(Box::leak(heads(0.9).into_boxed_str()));
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "you are a complete idiot", false, 0.05, 0.25).await;
        assert!(!out.pass(), "a high toxicity score is withheld: {out:?}");
    }

    #[tokio::test]
    async fn a_clean_text_passes_the_scorer() {
        let port = stub_scorer(Box::leak(heads(0.0).into_boxed_str()));
        let eps = vec![format!("http://127.0.0.1:{port}")];
        let out = run(&eps, "have a nice day", false, 0.05, 0.25).await;
        assert!(out.pass(), "a benign text passes: {out:?}");
    }

    /// There is NO tokens-per-word guard: terse ("340") and repetitive
    /// ("a small test." x10) outputs both false-positived live. The heads
    /// alone decide; a clean score passes regardless of token density. One
    /// stub serves ONE request, so each call gets its own.
    #[tokio::test]
    async fn a_terse_or_repetitive_output_is_not_judged_by_token_density() {
        let one = |text: &'static str| async move {
            // tokens=42 mimics the repetitive-string score that tripped it.
            let port = stub_scorer(Box::leak(heads(0.0).into_boxed_str()));
            let eps = vec![format!("http://127.0.0.1:{port}")];
            run(&eps, text, false, 0.05, 0.25).await
        };
        assert!(one("340").await.pass(), "a terse number passes");
        assert!(one("ok").await.pass(), "a terse ok passes");
        assert!(one("one two three four").await.pass(), "low density passes");
    }

    /// [`stub_scorer`], keeping the request it served.
    fn stub_scorer_capture(body: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<String>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let keep = seen.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = vec![0u8; 65536];
                let n = s.read(&mut buf).unwrap_or(0);
                *keep.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (port, seen)
    }

    fn cfg_with(endpoint: String) -> crate::settings::SlopdropSection {
        crate::settings::SlopdropSection {
            output_classifier: vec![endpoint],
            ..Default::default()
        }
    }

    /// The scorer judges the text the post CARRIES: a colored result is
    /// checked — and posted — with its codes stripped. The first version
    /// checked the raw output, so every colored result failed the
    /// control-character gate as "art".
    #[tokio::test]
    async fn the_public_decision_checks_the_posted_text() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let (port, seen) = stub_scorer_capture(Box::leak(heads(0.0).into_boxed_str()));
        let cfg = cfg_with(format!("http://127.0.0.1:{port}"));
        let idx = crate::safety::ProcIndex::default();
        let d = decide_public(
            &cfg,
            "rainbow hello",
            "\x034hello\x0f there",
            None,
            Some(&idx),
        )
        .await;
        assert!(
            d.shaped.text.starts_with("hello there\n(with formatting:"),
            "{:?} / {}",
            d.shaped.text,
            d.log
        );
        let req = seen.lock().unwrap().clone();
        assert!(
            req.contains("hello there"),
            "the scorer read the stripped text: {req}"
        );
        assert!(!req.contains("\\u0003"), "and no color code: {req}");
    }

    /// A line that wraps a bad-list proc is held by the bad list, never
    /// left to the scorer (which is not even asked).
    #[tokio::test]
    async fn the_public_decision_holds_a_line_that_wraps_a_bad_list_proc() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        crate::safety::record("hbad1", crate::safety::Verdict::Unsafe).unwrap();
        let idx = crate::safety::ProcIndex::from_json(
            &serde_json::json!([{"name":"badproc","args":"","body":"return x","hash":"hbad1"}]),
            &serde_json::Value::Null,
        );
        let cfg = cfg_with("http://127.0.0.1:0".into());
        for code in ["return [badproc]", "badproc;", "string toupper [badproc]"] {
            let d = decide_public(&cfg, code, "a harmless looking line", None, Some(&idx)).await;
            assert!(
                d.shaped.text.starts_with("Output withheld"),
                "{code:?}: {}",
                d.shaped.text
            );
            assert_eq!(d.shaped.reason, "reaches bad-list proc badproc");
            assert!(d.log.contains("not run"), "{}", d.log);
        }
        // No index to tell (the bridge did not answer): fail closed.
        let d = decide_public(&cfg, "expr 1", "1", None, None).await;
        assert!(
            d.shaped.text.starts_with("Output withheld"),
            "{}",
            d.shaped.text
        );
    }

    #[tokio::test]
    async fn a_dead_endpoint_means_withheld_not_pass() {
        // A port that is closed: withheld "classifier unavailable", fast.
        let eps = vec!["http://127.0.0.1:0".to_string()];
        let t0 = std::time::Instant::now();
        let out = run(&eps, "hello", false, 0.05, 0.25).await;
        assert!(!out.pass());
        assert_eq!(out.reason(), Some("classifier unavailable"));
        assert!(t0.elapsed() < std::time::Duration::from_secs(5));
        // Second call: the failed endpoint is skipped (30 s), so it is faster.
        let t1 = std::time::Instant::now();
        let _ = run(&eps, "hello again", false, 0.05, 0.25).await;
        assert!(t1.elapsed() < std::time::Duration::from_millis(500));
    }

    #[tokio::test]
    async fn an_allowlisted_text_passes_untouched() {
        let eps = vec!["http://127.0.0.1:0".to_string()]; // no scorer needed
        let out = run(&eps, "┌───┐ art", true, 0.05, 0.25).await;
        assert!(out.pass(), "allowlist short-circuits every gate");
    }

    /// `run` must NOT decide the allowlist itself: it trusts the bool the
    /// caller computed from the hash. A non-allowlisted art text with no
    /// scorer is withheld by the deterministic gate, never passed.
    #[tokio::test]
    async fn a_non_allowlisted_art_text_is_not_passed_by_run() {
        let eps = vec!["http://127.0.0.1:0".to_string()];
        let out = run(&eps, "┌───┐ art", false, 0.05, 0.25).await;
        assert!(!out.pass(), "no allowlist → the art gate decides: {out:?}");
        assert_eq!(out.reason(), Some("art or non-ascii"));
    }

    /// A text is scored in pieces the scorer reads whole: at most `max`
    /// characters each, cut at a line end when one is in the piece, inside
    /// a line when not, every character in exactly one piece.
    #[test]
    fn a_long_text_is_scored_in_pieces() {
        let text = "aaaa\nbbbb\ncccccccccccc\nééé\n";
        let pieces = super::score_chunks(text, 7);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|p| p.chars().count() <= 7), "{pieces:?}");
        assert_eq!(pieces[0], "aaaa\n", "cut at the line end");
        assert!(
            pieces.contains(&"ccccccc"),
            "a long line is cut inside: {pieces:?}"
        );
        assert_eq!(super::score_chunks("", 7), Vec::<&str>::new());
        assert_eq!(super::score_chunks("short", 600), vec!["short"]);
    }
}
