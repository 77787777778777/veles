//! The TOS-safe output layer for the slopdrop bridge on a PUBLIC surface.
//!
//! WHY THIS MODULE EXISTS
//!
//! On IRC the answer to a `tcl` line goes to the channel and the worst
//! case is an over-long drawing — `cut_to` in `tcleval` is a courtesy,
//! not a guard. On Bluesky the reply is a PUBLIC post under the bot's
//! account, and slopdrop's accumulated state is a decade of IRC channel
//! building: procs named `!unknown` and `%niggeria`, harassment
//! templates, drawings of things a public feed must not carry. Posting
//! that inline is a Terms-of-Service exposure on the operator's own
//! domain account.
//!
//! So the Bluesky surface never posts the raw output of a `/tcl` eval
//! without the checks answered HERE ([`shape_for_public`], plan §3):
//!
//!   1. **The bad list.** A hash-keyed verdict store (the old prebake:
//!      `safe`, `borderline`, `unsafe`, plus the owner's `approved` /
//!      `blocked`). A line that REACHES a bad-list proc — names it, or
//!      names a stored proc whose body reaches it ([`ProcIndex::bad_reach`])
//!      — is link-only, however benign this one output looks. The output
//!      check cannot stand in for it: replayed on the calibration corpus,
//!      28 of 71 bad-list proc outputs pass the gates and the scorer.
//!   2. **The output check** (`slopdrop_check`), run by the gateway on the
//!      exact text the post will carry ([`public_text`]): deterministic
//!      gates plus the local scorer. Only a pass goes inline.
//!   3. **Is it short?** A passing result past two posts is elided: the
//!      head goes inline, the whole thing is published, and the post
//!      links to it.
//!
//! WHAT THE LINK POINTS AT. Both a raw markdown blob and a RENDERED HTML
//! page are built by [`build_artifacts`] — the reader gets the page, the
//! raw source stays available. The `.md` is written as a fenced CODE
//! block (not reflowed prose) because #coven's procs draw and a renderer
//! that reflows a drawing corrupts it. The page renders the OUTPUT
//! through an mIRC control-code renderer (`\x03` color, `\x02` bold) so
//! IRC formatting shows as real CSS — the post carries the text with the
//! codes stripped ([`strip_irc_codes`]). Raw HTML in the source is
//! dropped by the renderer, never served live.
//!
//! WHERE IT PUBLISHES. The caller (the atproto adapter) uploads both
//! artifacts as BLOBS on our own PDS and creates one owner-pointer record
//! (`online.7778777.tcl.output`) that references them, so the PDS's
//! unreferenced-blob sweep keeps them. This module is IO-free by design:
//! [`shape_for_public`] builds the artifacts and leaves
//! [`URL_PLACEHOLDER`] in the text, and the adapter substitutes the real
//! link once the upload answers the CID.

use crate::withheld::PostKind;
use std::path::PathBuf;

/// How much of a `safe` result may go inline on Bluesky, in CHARACTERS.
///
/// The user's ceiling was "two posts" — the adapter splits a reply into
/// a self-replying thread at 300 graphemes each, so two posts is ~600.
/// We hold the inline head under that so the truncation note and the
/// link never push the SECOND post over its own cap. Anything longer is
/// elided: head + link, the rest on the host.
pub const INLINE_MAX_CHARS: usize = 560;

/// The first chunk of an elided result, leaving room for the
/// "…(truncated — full output: …)" note and the URL inside one
/// 300-grapheme post: the note is 29 characters, the short link 41
/// (`https://7778777.online/t/` and 16), so 220 leaves 10 to spare. At
/// 260 the link always went to a second post.
pub const ELISION_HEAD_CHARS: usize = 220;

/// The classification of one stored proc, or the owner override on it.
///
/// `safe` no longer grants anything on its own (the output check decides
/// inline now); the BAD list — `unsafe` | `borderline` — is what the
/// shaping reads. `Approved`/`Blocked` are the owner's per-proc override,
/// set by `/slopdrop approve|delete`: `Approved` lifts a proc OFF the bad
/// list (its later outputs still go through the output check); `Blocked`
/// pins it link-only for good, no hosted copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Rated benign by the old prebake. Grants nothing on its own.
    Safe,
    /// Link-only. On the bad list.
    Borderline,
    /// Never inline. On the bad list.
    Unsafe,
    /// Owner override: lifted off the bad list (output check still runs).
    Approved,
    /// Owner override: link-only for good, no hosted copy.
    Blocked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Safe => "safe",
            Verdict::Borderline => "borderline",
            Verdict::Unsafe => "unsafe",
            Verdict::Approved => "approved",
            Verdict::Blocked => "blocked",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "safe" => Some(Verdict::Safe),
            "borderline" => Some(Verdict::Borderline),
            "unsafe" => Some(Verdict::Unsafe),
            "approved" => Some(Verdict::Approved),
            "blocked" => Some(Verdict::Blocked),
            _ => None,
        }
    }
    /// Is this on the bad list (link-only)? `safe` and `approved` are not;
    /// `blocked` is, and additionally gets no hosted copy (the caller reads
    /// that off the variant, not off this).
    pub fn bad_list(self) -> bool {
        matches!(
            self,
            Verdict::Borderline | Verdict::Unsafe | Verdict::Blocked
        )
    }
}

/// The hash-keyed verdict store: `<state>/safety.tsv` (data.rs).
///
/// Keyed by CONTENT hash (slopdrop's own `hash` field on `/api/procs`)
/// rather than by NAME: a proc redefined from the channel keeps its name
/// but gets a new hash, so a name-keyed store would happily apply the old
/// verdict to the new body. Hash-keyed means a redefinition simply MISSES
/// (unknown → fail closed → re-classify), which is exactly the behaviour
/// the prebake-update rule wants.
fn store_path() -> PathBuf {
    crate::data::dir().join("safety.tsv")
}

/// Look up the verdict for one content hash. `None` = unknown (fail
/// closed at the caller).
pub fn verdict_for(hash: &str) -> Option<Verdict> {
    let body = std::fs::read_to_string(store_path()).ok()?;
    for line in body.lines() {
        let mut it = line.splitn(2, '\t');
        let (Some(h), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        if h == hash {
            return Verdict::parse(v);
        }
    }
    None
}

/// Every recorded verdict, by content hash (one read of the store, for a
/// caller that looks up many — [`ProcIndex::bad_reach`]).
pub fn verdicts() -> std::collections::HashMap<String, Verdict> {
    std::fs::read_to_string(store_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (h, v) = l.split_once('\t')?;
            Some((h.to_string(), Verdict::parse(v)?))
        })
        .collect()
}

/// Record (or overwrite) the verdict for one content hash.
///
/// Read-modify-write under a process-global lock; the store is small
/// (hundreds of rows) and written rarely (a re-classification, the
/// prebake), so a whole-file rewrite is the honest implementation.
pub fn record(hash: &str, verdict: Verdict) -> Result<(), String> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("safety store dir: {e}"))?;
    }
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let mut rows: Vec<(String, String)> = body
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, '\t');
            match (it.next(), it.next()) {
                (Some(h), Some(v)) => Some((h.to_string(), v.to_string())),
                _ => None,
            }
        })
        .collect();
    rows.retain(|(h, _)| h != hash);
    rows.push((hash.to_string(), verdict.as_str().to_string()));
    rows.sort();
    let out = rows
        .iter()
        .map(|(h, v)| format!("{h}\t{v}\n"))
        .collect::<String>();
    std::fs::write(&path, out).map_err(|e| format!("safety store write: {e}"))
}

// ── the owner allowlist and blocked list ────────────────────────────────
//
// Two hash sets beside the verdict store, one output sha256 per line:
//  - allowlist: this EXACT text passes the output check untouched (the
//    owner approved it once — plan §2 rule 1). Keyed by the text's hash so
//    a one-character change misses and goes through the check again.
//  - blocked: this exact text is never hosted again (the owner deleted it).
//    Read as rule 2 in `shape_for_public`.

fn allowlist_path() -> PathBuf {
    crate::data::dir().join("allowlist.txt")
}

fn blocklist_path() -> PathBuf {
    crate::data::dir().join("blocked.txt")
}

fn hash_set_read(path: &std::path::Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn hash_set_write(
    path: &std::path::Path,
    set: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("hash set dir: {e}"))?;
    }
    let mut rows: Vec<&String> = set.iter().collect();
    rows.sort();
    let body = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("hash set write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("hash set rename: {e}"))
}

/// Is this output hash on the owner allowlist (passes untouched)?
pub fn allowlisted(hash: &str) -> bool {
    hash_set_read(&allowlist_path()).contains(hash)
}

/// Is this output hash blocked (never hosted again)?
pub fn blocked(hash: &str) -> bool {
    hash_set_read(&blocklist_path()).contains(hash)
}

/// Add an output hash to the owner allowlist.
pub fn allowlist_add(hash: &str) -> Result<(), String> {
    let p = allowlist_path();
    let mut s = hash_set_read(&p);
    s.insert(hash.to_string());
    hash_set_write(&p, &s)
}

/// Take an output hash off the allowlist (a delete undoes an approve).
pub fn allowlist_remove(hash: &str) -> Result<(), String> {
    let p = allowlist_path();
    let mut s = hash_set_read(&p);
    s.remove(hash);
    hash_set_write(&p, &s)
}

/// Add an output hash to the blocked list.
pub fn block_add(hash: &str) -> Result<(), String> {
    let p = blocklist_path();
    let mut s = hash_set_read(&p);
    s.insert(hash.to_string());
    hash_set_write(&p, &s)
}

/// Take an output hash off the blocked list (an approve undoes a delete).
pub fn block_remove(hash: &str) -> Result<(), String> {
    let p = blocklist_path();
    let mut s = hash_set_read(&p);
    if !s.remove(hash) {
        return Ok(());
    }
    hash_set_write(&p, &s)
}

/// Build the two artifacts for one result: the raw markdown (fenced, so
/// a drawing keeps its columns) and the rendered HTML page (input as a
/// markdown code block, output through the mIRC control-code renderer so
/// IRC color/emphasis shows). Returns `(markdown, html, mime_md,
/// mime_html)`. Pure: no IO, no config — the ATProto adapter publishes
/// them as blobs on our own PDS.
pub fn build_artifacts(code: &str, output: &str) -> (String, String) {
    // The .md, fenced like `publish::code_block` (a drawing keeps its
    // columns only inside a fence). Longest-run-of-backticks + 1.
    let longest = output
        .lines()
        .filter_map(|l| {
            let t = l.trim_start();
            t.starts_with("```")
                .then(|| t.chars().take_while(|c| *c == '`').count())
        })
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let md = format!(
        "# `tcl` output\n\n## input\n\n{fence}\n{code}\n{fence}\n\n## output\n\n{fence}\n{output}\n{fence}\n"
    );
    // The rendered page. The INPUT stays markdown-rendered; the OUTPUT is
    // rendered by the mIRC control-code renderer instead (USER 2026-10-06:
    // "we can definitely show irc colours in the rendering"). Raw HTML in
    // the source is dropped by both renderers — never passed through live.
    let html = page_shell(&render_page(code, output));
    (md, html)
}

/// [`build_artifacts`] for a reply with pictures (PLAN-slopdrop-art.md
/// P4): the page shows each drawing as the picture the post carries, above
/// the output. The page is one blob, so each PNG is inline as a data URI;
/// the bytes are the module's own drawing, base64 only, never markup from
/// the output. The viewer (`deploy/atproto/tcl-viewer.html`) shows the
/// page in a script-less sandboxed frame.
pub fn build_artifacts_with(code: &str, output: &str, pictures: &[Vec<u8>]) -> (String, String) {
    use base64::Engine as _;
    let (md, html) = build_artifacts(code, output);
    if pictures.is_empty() {
        return (md, html);
    }
    let mut imgs = String::from("<h2>pictures</h2>");
    for p in pictures {
        imgs.push_str(&format!(
            "<p><img alt=\"the drawing\" style=\"max-width:100%;image-rendering:pixelated\" \
             src=\"data:image/png;base64,{}\"></p>",
            base64::engine::general_purpose::STANDARD.encode(p)
        ));
    }
    let html = html.replacen("<h2>output</h2>", &format!("{imgs}<h2>output</h2>"), 1);
    (md, html)
}

/// Render markdown to an HTML fragment. Raw HTML in the source is
/// DROPPED, never passed through — this is untrusted channel-built
/// output, and a `<script>` that survived into the hosted page would run
/// under the operator's own domain. pulldown-cmark emits `Event::Html`
/// for raw markup; we filter those events out (the markdown structure
/// survives, the live markup does not).
fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{Event, Parser};
    let parser = Parser::new(md).filter(|ev| !matches!(ev, Event::Html(_) | Event::InlineHtml(_)));
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

/// The mIRC color palette (the 16 classic colors, 0–15), indexed by the
/// number after the `\x03` control byte.
const IRC_COLORS: [&str; 16] = [
    "#ffffff", // 0 white
    "#000000", // 1 black
    "#00007f", // 2 blue (navy)
    "#009300", // 3 green
    "#ff0000", // 4 red
    "#7f0000", // 5 brown (maroon)
    "#9c009c", // 6 purple
    "#fc7f00", // 7 orange
    "#ffff00", // 8 yellow
    "#00fc00", // 9 light green (lime)
    "#009393", // 10 cyan (teal)
    "#00ffff", // 11 light cyan (aqua)
    "#0000fc", // 12 light blue (royal)
    "#ff00ff", // 13 pink (magenta)
    "#7f7f7f", // 14 grey
    "#d2d2d2", // 15 light grey (silver)
];

/// Render the eval page: the input as a markdown code block, the output
/// through the mIRC control-code renderer so IRC color/emphasis shows.
fn render_page(code: &str, output: &str) -> String {
    let mut s = String::from("<h1>tcl output</h1><h2>input</h2>");
    s.push_str(&render_markdown(&format!("```\n{code}\n```\n")));
    s.push_str("<h2>output</h2>");
    s.push_str(&render_irc(output));
    s
}

/// Render a string with mIRC control codes into HTML. `\x03` color,
/// `\x02` bold, `\x1d` italic, `\x1f` underline, `\x1e` strikethrough,
/// `\x11` monospace, `\x0f` reset, `\x16` reverse (swapped fg/bg). Every
/// literal byte is HTML-escaped; the control codes become `<span>` /
/// `<b>`/`<i>`/`<u>`/`<s>` tags, never raw markup from the source.
///
/// This is the one place a `tcl` result's IRC formatting is VISIBLE: the
/// Bluesky post flattens it, the raw `.md` keeps the bytes, and this page
/// shows the color.
fn render_irc(input: &str) -> String {
    #[derive(Default, Clone)]
    struct St {
        fg: Option<usize>,
        bg: Option<usize>,
        bold: bool,
        italic: bool,
        underline: bool,
        strike: bool,
        mono: bool,
        reverse: bool,
    }
    let mut st = St::default();
    let mut open = false; // is a styled <span> currently open
    let mut out = String::from("<pre class=\"irc\">");
    let mut chars = input.chars().peekable();
    // ALL styling is span-based (color + font-weight + font-style +
    // text-decoration), so a toggle just closes and re-opens ONE span and
    // the markup always nests validly — no interleaved <b>…</span> spans.
    macro_rules! restyle {
        () => {{
            if open {
                out.push_str("</span>");
                open = false;
            }
            let (mut fg, mut bg) = (st.fg, st.bg);
            if st.reverse {
                std::mem::swap(&mut fg, &mut bg);
                // Reverse with no explicit colors = inverse video: light on dark.
                if fg.is_none() && bg.is_none() {
                    fg = Some(0);
                    bg = Some(1);
                }
            }
            let mut style: Vec<String> = Vec::new();
            if let Some(f) = fg {
                style.push(format!("color:{}", IRC_COLORS[f % 16]));
            }
            if let Some(b) = bg {
                style.push(format!("background:{}", IRC_COLORS[b % 16]));
            }
            if st.bold {
                style.push("font-weight:bold".into());
            }
            if st.italic {
                style.push("font-style:italic".into());
            }
            let mut deco: Vec<&str> = Vec::new();
            if st.underline {
                deco.push("underline");
            }
            if st.strike {
                deco.push("line-through");
            }
            if !deco.is_empty() {
                style.push(format!("text-decoration:{}", deco.join(" ")));
            }
            if !style.is_empty() {
                out.push_str(&format!("<span style=\"{}\">", style.join(";")));
                open = true;
            }
        }};
    }
    while let Some(c) = chars.next() {
        match c {
            '\x02' => {
                st.bold = !st.bold;
                restyle!();
            }
            '\x1d' => {
                st.italic = !st.italic;
                restyle!();
            }
            '\x1f' => {
                st.underline = !st.underline;
                restyle!();
            }
            '\x1e' => {
                st.strike = !st.strike;
                restyle!();
            }
            '\x11' => {
                st.mono = !st.mono;
                // <pre> already is monospace; this is a no-op visually.
            }
            '\x16' => {
                st.reverse = !st.reverse;
                restyle!();
            }
            '\x0f' => {
                // reset: close the open span and drop every flag.
                st = St::default();
                restyle!();
            }
            '\x03' => {
                // color: \x03[fg[,bg]] — one or two digits each.
                let mut fg = String::new();
                for _ in 0..2 {
                    if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                        fg.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                let mut bg = String::new();
                if chars.peek() == Some(&',') {
                    // peek ahead: a digit must follow the comma.
                    let mut it = chars.clone();
                    it.next();
                    if it.next().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next(); // consume ','
                        for _ in 0..2 {
                            if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                                bg.push(chars.next().unwrap());
                            } else {
                                break;
                            }
                        }
                    }
                }
                st.fg = fg.parse::<usize>().ok();
                st.bg = bg.parse::<usize>().ok();
                restyle!();
            }
            // Escape every literal byte — the source is untrusted.
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    if open {
        out.push_str("</span>");
    }
    out.push_str("</pre>");
    out
}

/// The minimal static page. Dark, monospace, no scripts, no external
/// resources — a reader opens one file and gets the answer.
fn page_shell(body: &str) -> String {
    // The font stack matters: this page renders ASCII art and mIRC-colored
    // output, so EVERY glyph must be one cell wide. `ui-monospace`/
    // `Menlo` are absent on most Linux boxes and the generic `monospace`
    // fallback can be a proportional or CJK-mixed-width face (USER
    // 2026-10-06: "looks weird, not monospace"). Name concrete, widely
    // installed mono faces first, then the generic. `pre.irc` is the
    // output block — style it explicitly, not just bare `pre`, so the
    // mIRC renderer's <pre class="irc"> picks up the same box.
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>tcl output</title>\
<style>:root{{color-scheme:dark}}\
body{{background:#0b0e14;color:#d7e0ee;\
font-family:\"JetBrains Mono\",\"DejaVu Sans Mono\",\"Liberation Mono\",Menlo,Consolas,\"Courier New\",monospace;\
font-variant-ligatures:none;\
max-width:52rem;margin:2rem auto;padding:0 1rem;line-height:1.5}}\
pre,pre.irc{{background:#12161f;border:1px solid #232a3a;border-radius:6px;padding:1rem;\
overflow-x:auto;white-space:pre;\
font-family:inherit}}\
h1,h2{{color:#7aa2f7;font-weight:600}}code{{color:#9ece6a}}</style>\
</head><body>{body}</body></html>"
    )
}

/// The public-post shaping of a `/tcl` result for the Bluesky surface.
///
/// The token the shaped text carries where the published URL belongs.
/// The atproto adapter replaces it with the real blob URL after it has
/// uploaded the artifacts (the upload returns the CID the URL needs, so
/// the URL cannot be known until the PDS answers). If publishing fails,
/// the adapter swaps in a plain "not kept" note instead.
pub const URL_PLACEHOLDER: &str = "\u{0}TCL_OUT_URL\u{0}";

/// Drop leading and trailing BLANK lines, keeping every interior line —
/// and, critically, the indentation of the first and last content lines.
/// Unlike `str::trim`, which removes ALL leading whitespace (including the
/// spaces that position the first line of an ASCII drawing), this only
/// eats whole lines that are empty or all-whitespace.
fn trim_blank_lines(s: &str) -> String {
    let is_blank = |l: &str| l.trim().is_empty();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| !is_blank(l))
        .map(|i| i + 1)
        .unwrap_or(start);
    if start >= end {
        return String::new();
    }
    // Rejoin the kept lines verbatim — each keeps its own leading spaces.
    lines[start..end].join("\n")
}

/// Remove mIRC formatting codes and ANSI CSI sequences, keeping the text.
///
/// The mIRC grammar is [`render_irc`]'s: `\x03` takes up to two digits of
/// foreground and, only when a digit follows the comma, up to two of
/// background — so `\x034,` keeps its comma and `\x0304,123` keeps the
/// `3`. The ANSI half is `ESC [ params intermediates final`. Any OTHER
/// control byte is left in place for the output check's art gate.
///
/// This is what the public post carries and what the output check reads:
/// a colored `rainbow` result used to fail the control-character gate as
/// "art" before a word of it was judged.
pub fn strip_irc_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x02' | '\x1d' | '\x1f' | '\x1e' | '\x11' | '\x16' | '\x0f' => {}
            '\x03' => {
                for _ in 0..2 {
                    if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&',') {
                    let mut it = chars.clone();
                    it.next();
                    if it.next().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next(); // the comma
                        for _ in 0..2 {
                            if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                // parameters and intermediates, then one final byte
                while chars.peek().is_some_and(|c| ('\x20'..='\x3f').contains(c)) {
                    chars.next();
                }
                if chars.peek().is_some_and(|c| ('\x40'..='\x7e').contains(c)) {
                    chars.next();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The text a public post carries for one output, and whether it is the
/// elided head of a longer one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicText {
    pub body: String,
    pub elided: bool,
}

/// What the public post would carry for `output`: formatting codes
/// stripped, blank lines trimmed (never the first line's indentation), and
/// cut to the [`ELISION_HEAD_CHARS`] head when over [`INLINE_MAX_CHARS`].
///
/// ONE function for every place that needs it — the gateway's output
/// check, [`shape_for_public`], and the approve repost — so the check
/// always judges the very text that gets posted.
pub fn public_text(output: &str) -> PublicText {
    let text = trim_blank_lines(&strip_irc_codes(output));
    if text.chars().count() <= INLINE_MAX_CHARS {
        PublicText {
            body: text,
            elided: false,
        }
    } else {
        PublicText {
            body: text.chars().take(ELISION_HEAD_CHARS).collect(),
            elided: true,
        }
    }
}

/// The inline post for a [`PublicText`]: the text plus the formatting
/// link, or the head plus the full-output link.
pub fn inline_post(pt: &PublicText, link: &str) -> String {
    if pt.elided {
        format!("{} …(truncated — full output: {link})", pt.body)
    } else {
        format!("{}\n(with formatting: {link})", pt.body)
    }
}

/// The shaped post text plus the artifacts to publish, when the verdict
/// calls for a hosted copy. `artifacts` is `Some((markdown, html))`
/// whenever the post references a hosted copy (withheld, elided, or the
/// always-linked formatting page); `None` for a `Blocked` result (no
/// hosted copy is kept) and never otherwise. The text contains
/// [`URL_PLACEHOLDER`] exactly where the rendered page's URL belongs.
pub struct ShapedPublic {
    pub text: String,
    pub artifacts: Option<(String, String)>,
    /// What the post is. The gateway records EVERY public reply in the
    /// `slopdrop_withheld` ledger under this kind, so the owner can delete
    /// any of them; only a `Withheld` one becomes a Telegram question
    /// (approve/delete/keep/keep and stop asking).
    pub kind: PostKind,
    /// Why it was withheld, or what let it out (the owner reads this in
    /// the ledger).
    pub reason: String,
    /// The one stored proc the line called with literal words, if any: an
    /// approve marks it Approved, a delete marks it Blocked.
    pub proc_hash: Option<String>,
}

/// A withheld reply: a note and the link to the hosted copy of `output`
/// (with its `pictures`, PNG bytes), asked about. The one wording every
/// withheld reply posts.
pub fn withheld_with_link(
    code: &str,
    output: &str,
    pictures: &[Vec<u8>],
    reason: String,
    proc_hash: Option<String>,
) -> ShapedPublic {
    ShapedPublic {
        text: format!("Output withheld (content policy) — full result: {URL_PLACEHOLDER}"),
        artifacts: Some(build_artifacts_with(code, output, pictures)),
        kind: PostKind::Withheld,
        reason,
        proc_hash,
    }
}

/// A blocked reply: the owner deleted this output (or proc) before, so it
/// is a note with no hosted copy, and never asked about again.
pub fn blocked_note(proc_hash: Option<String>) -> ShapedPublic {
    ShapedPublic {
        text: "Output withheld (content policy).".to_string(),
        artifacts: None,
        kind: PostKind::Blocked,
        reason: "the owner deleted this output before".to_string(),
        proc_hash,
    }
}

/// Does this `/tcl` line do more than call ONE stored proc with literal
/// words? A command substitution (`safeproc [unsafeproc]`), a variable
/// (`$x`), a separator (`;`), a quoted or braced word, a backslash, or a
/// second line all mean the output is NOT just the named proc's. slopdrop's
/// `invoked_hash` names only the FIRST word; this is the half that tells a
/// single literal call apart from a composed line (a composed line can go
/// inline now, but only on its OWN output-check pass, never on the named
/// proc's bad-list verdict).
pub fn composes(code: &str) -> bool {
    code.trim().chars().any(|c| {
        matches!(
            c,
            '[' | ']' | '$' | ';' | '{' | '}' | '"' | '\\' | '\n' | '\r'
        )
    })
}

/// The output check's verdict on the exact text the post will carry,
/// already run by the gateway (`slopdrop_check::run`) and handed in here.
/// Shaping is sync and IO-free; the check is async, so it is awaited
/// BEFORE this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The output passed the local check (allowlist, gates + scorer).
    Pass,
    /// Withheld. The reason is the gateway's, shown to the owner.
    Withheld,
}

/// What the gateway knows about one result besides its text, for
/// [`shape_for_public`].
#[derive(Debug, Clone, Default)]
pub struct ShapeInputs<'a> {
    /// slopdrop's `invoked_hash`: the stored proc the line's FIRST word
    /// named. It speaks for the bad-list verdict only when the line is
    /// that one literal call ([`composes`] is false).
    pub hash: Option<&'a str>,
    /// The output's sha256 is on the owner's blocked list.
    pub blocked_output: bool,
    /// The output's sha256 is on the owner's allowlist: the owner saw this
    /// exact text and approved it.
    pub allowlisted: bool,
    /// Why the line is on the bad list through what it REACHES
    /// ([`ProcIndex::bad_reach`]), or that the index to tell was missing.
    /// `None` = it reaches no bad-list proc.
    pub bad_reach: Option<String>,
}

/// Given the eval's code, its output, what the gateway knows about it, and
/// the output check's outcome, decide what the public post may carry. This
/// is the ONE function the adapter shapes through; the safety decision
/// lives here so no other surface answers it differently.
///
/// The order of rules (the first that decides wins):
///   1. The output hash or the single-called proc is `Blocked` → withheld,
///      NO hosted copy.
///   2. The owner allowlisted this exact output → inline.
///   3. The line is on the bad list — one stored proc called with literal
///      words whose verdict is bad, or a line that reaches a bad-list proc
///      by name or through stored bodies → withheld + link.
///   4. The output check passed → inline (text + formatting link, or head
///      + link when over the cap).
///   5. Anything else → withheld + link, and a withheld entry.
pub fn shape_for_public(
    code: &str,
    output: &str,
    inputs: &ShapeInputs<'_>,
    check: CheckOutcome,
) -> ShapedPublic {
    let single_call = inputs.hash.filter(|h| !h.is_empty() && !composes(code));
    let verdict = single_call.and_then(verdict_for);
    // The hosted copy keeps the formatting; strip only leading/trailing
    // BLANK lines — never the first content line's indentation (USER
    // 2026-10-06).
    let hosted = trim_blank_lines(output);
    let proc_hash = single_call.map(str::to_string);
    let withheld_link =
        |reason: String| withheld_with_link(code, &hosted, &[], reason, proc_hash.clone());
    let inline = |reason: &str| ShapedPublic {
        text: inline_post(&public_text(output), URL_PLACEHOLDER),
        artifacts: Some(build_artifacts(code, &hosted)),
        kind: PostKind::Inline,
        reason: reason.to_string(),
        proc_hash: proc_hash.clone(),
    };

    // 1. Blocked: no hosted copy, ever — and no question: the owner
    //    already decided this one (a delete), so it is never asked again.
    if inputs.blocked_output || matches!(verdict, Some(Verdict::Blocked)) {
        return blocked_note(proc_hash);
    }
    // 2. The owner approved this exact text.
    if inputs.allowlisted {
        return inline("the owner approved this text");
    }
    // 3. The bad list: the single call's own verdict, then what it reaches.
    if let Some(v) = verdict.filter(|v| v.bad_list()) {
        return withheld_link(format!("proc on the bad list ({})", v.as_str()));
    }
    if let Some(why) = &inputs.bad_reach {
        return withheld_link(why.clone());
    }
    // 4. The output check passed.
    if check == CheckOutcome::Pass {
        return inline("passed the output check");
    }
    // 5. Anything else.
    withheld_link(if composes(code) {
        "a composed line whose output did not pass the local check".to_string()
    } else {
        "output did not pass the local check".to_string()
    })
}

/// slopdrop's stored state, as the safety layer reads it: every proc by
/// content hash, and the persistent variables. Fetched from the bridge
/// (`/api/procs`, `/api/vars`) and cached for [`INDEX_TTL`].
#[derive(Debug, Default)]
pub struct ProcIndex {
    /// hash → (name, args, body).
    pub procs: std::collections::HashMap<String, (String, String, String)>,
    /// Every stored proc name (what a body can call).
    pub names: std::collections::HashSet<String>,
    /// Persistent variable name → value.
    pub vars: std::collections::HashMap<String, String>,
    /// The names of the BASE library's procs (`state/stolen-treasure.tcl`,
    /// listed by `/api/procs?base=1` since slopdrop fd19d6f), which no
    /// tracked proc overrides. Reached one hop only (see
    /// [`ProcIndex::bad_reach`]).
    pub base: std::collections::HashSet<String>,
}

/// How long a fetched [`ProcIndex`] is trusted. A redefinition changes the
/// hash, which misses the verdict store anyway (fail closed), so the
/// index only has to be fresh enough to know a NEW proc's body.
pub const INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// The words a Tcl line or body is made of, split on whitespace and the
/// syntax characters a call or a substitution sits between. A leading
/// `::` (a fully qualified call) is dropped.
fn body_words(body: &str) -> impl Iterator<Item = &str> {
    body.split(|c: char| {
        c.is_whitespace() || matches!(c, '[' | ']' | '{' | '}' | ';' | '"' | '$' | '(' | ')')
    })
    .map(|w| w.trim_start_matches("::"))
    .filter(|w| !w.is_empty())
}

/// The `ST*` stamp a word names, the way slopdrop's `<x>say` handler and
/// `stampsay -s <x>` read it: `fedoragoonsay` and `fedoragoonthink` are
/// `STfedoragoon`. `stampsay` and `stampthink` themselves name none.
fn stamp_of(word: &str) -> Option<String> {
    let w = word.trim_start_matches("::");
    let x = w.strip_suffix("say").or_else(|| w.strip_suffix("think"))?;
    (!x.is_empty() && x != "stamp").then(|| format!("ST{x}"))
}

/// Does this Tcl call `stampsay` or `stampthink` with no `-s`, which
/// draws a random stamp (any of the bad ones)? Read per piece of Tcl, so
/// one `-s` anywhere in it counts for every call: the safe side would be
/// per command, and a body that names its stamp once and draws a random
/// one beside it is not one slopdrop's library has.
fn random_stamp(text: &str) -> bool {
    let words: Vec<&str> = body_words(text).collect();
    words.iter().any(|w| *w == "stampsay" || *w == "stampthink") && !words.contains(&"-s")
}

/// The stamps a piece of Tcl names: every `<x>say`/`<x>think` word, and
/// the word after each `-s` (`stampsay -l -s fedoragoon …`).
fn stamps_named(text: &str) -> Vec<String> {
    let words: Vec<&str> = body_words(text).collect();
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if let Some(s) = stamp_of(w) {
            out.push(s);
        }
        if *w == "-s" {
            if let Some(next) = words.get(i + 1) {
                out.push(format!("ST{next}"));
            }
        }
    }
    out
}

impl ProcIndex {
    /// Does this line REACH a proc on the bad list? The stored procs it
    /// names, and every stored proc reachable from those through their
    /// bodies, breadth-first; the first one whose verdict is on the bad
    /// list is returned by name. An `approved` proc is the owner's
    /// override: it is not on the list and its body is not followed.
    ///
    /// Why: the bad list used to speak only for a line that IS one literal
    /// call. `return [badproc]`, `badproc;` and `x [badproc]` are composed,
    /// so only the output check judged them — and the output check misses
    /// much of what the bad list holds (28 of 71 bad-list proc outputs in
    /// the calibration corpus pass it). A name that only appears as a
    /// string literal is a false hit: withheld, which is the safe side.
    ///
    /// The BASE library is reached one hop (PLAN-slopdrop-art.md P5): a
    /// base proc the line or a tracked body names is judged by its own
    /// verdict, and its body is read for the stamps it names, never for
    /// its calls (a base body fans out to hundreds of helpers). Stamps are
    /// read the way slopdrop's handler reads them: `<x>say`, `<x>think`
    /// and `-s <x>` reach `ST<x>`, so `nazisay hi` reaches `STnazi` though
    /// no proc of that name exists. A line that calls `stampsay` or
    /// `stampthink` with no `-s` picks a random stamp, which can be any of
    /// the bad ones: it is on the list.
    pub fn bad_reach(
        &self,
        code: &str,
        verdicts: &std::collections::HashMap<String, Verdict>,
    ) -> Option<String> {
        let by_name: std::collections::HashMap<String, (&str, &str)> = self
            .procs
            .iter()
            .map(|(h, (n, _a, b))| {
                (
                    n.trim_start_matches("::").to_string(),
                    (h.as_str(), b.as_str()),
                )
            })
            .collect();
        if random_stamp(code) {
            return Some("stampsay (a random stamp)".to_string());
        }
        let mut queue: std::collections::VecDeque<String> = body_words(code)
            .map(str::to_string)
            .chain(stamps_named(code))
            .filter(|w| by_name.contains_key(w))
            .collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some((hash, body)) = by_name.get(&name) else {
                continue;
            };
            match verdicts.get(*hash) {
                Some(v) if v.bad_list() => return Some(name),
                Some(Verdict::Approved) => continue,
                _ => {}
            }
            let base = self.base.contains(&name);
            // A tracked body that draws a random stamp is the line doing
            // it; a base one is judged by its verdict (the prebake's).
            if !base && random_stamp(body) {
                return Some(format!("{name} (a random stamp)"));
            }
            let calls = body_words(body)
                .filter(|_| !base)
                .map(str::to_string)
                .chain(stamps_named(body));
            for w in calls {
                if by_name.contains_key(&w) && !seen.contains(&w) {
                    queue.push_back(w);
                }
            }
        }
        None
    }

    /// Build from the bridge's `/api/procs` and `/api/vars` answers.
    pub fn from_json(procs: &serde_json::Value, vars: &serde_json::Value) -> Self {
        let mut idx = ProcIndex::default();
        for p in procs.as_array().into_iter().flatten() {
            let (Some(name), Some(hash)) = (p["name"].as_str(), p["hash"].as_str()) else {
                continue;
            };
            let args = p["args"].as_str().unwrap_or_default().to_string();
            let body = p["body"].as_str().unwrap_or_default().to_string();
            idx.names.insert(name.to_string());
            if p["origin"].as_str() == Some("base") {
                idx.base.insert(name.trim_start_matches("::").to_string());
            }
            idx.procs
                .insert(hash.to_string(), (name.to_string(), args, body));
        }
        for v in vars.as_array().into_iter().flatten() {
            if let Some(name) = v["name"].as_str() {
                let value = v["value"].as_str().unwrap_or_default().to_string();
                idx.vars.insert(name.to_string(), value);
            }
        }
        idx
    }

    /// The stored proc's NAME for a content hash, when the index knows it.
    /// The bad list is hash-keyed; the name is what the owner's withheld
    /// question and `/slopdrop approve proc <name>` show.
    pub fn name_for(&self, hash: &str) -> Option<&str> {
        self.procs.get(hash).map(|(n, _, _)| n.as_str())
    }

    /// The content hash for a proc NAME (the lookup `/slopdrop approve proc
    /// <name>` and the withheld display do by name).
    pub fn hash_for_name(&self, name: &str) -> Option<&str> {
        self.procs
            .iter()
            .find(|(_, (n, _, _))| n == name)
            .map(|(h, _)| h.as_str())
    }
}

/// The cached [`ProcIndex`] for this gateway's bridge, fetched when older
/// than [`INDEX_TTL`]. `None` when the bridge cannot answer. The index is
/// read for NAME lookup only now (the bad list is hash-keyed in the store);
/// the DeepSeek reach/stateful scan is gone with the proc rating it fed.
type CachedIndex = Option<(std::time::Instant, std::sync::Arc<ProcIndex>)>;

fn index_cache() -> &'static std::sync::Mutex<CachedIndex> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<CachedIndex>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Drop the cached index: a test whose stub bridge lists other procs.
#[cfg(test)]
pub fn forget_index() {
    *index_cache().lock().unwrap_or_else(|p| p.into_inner()) = None;
}

pub async fn proc_index(
    cfg: &crate::settings::SlopdropSection,
) -> Option<std::sync::Arc<ProcIndex>> {
    use std::sync::Arc;
    let cache = index_cache();
    if let Some((at, idx)) = cache.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        if at.elapsed() < INDEX_TTL {
            return Some(idx.clone());
        }
    }
    // `?base=1` adds the base library (slopdrop fd19d6f); an older
    // slopdrop ignores the parameter and lists the tracked procs alone.
    let procs = crate::bridge::get_json(cfg, "/api/procs?base=1")
        .await
        .ok()?;
    let vars = crate::bridge::get_json(cfg, "/api/vars")
        .await
        .unwrap_or(serde_json::Value::Null);
    let idx = Arc::new(ProcIndex::from_json(&procs, &vars));
    *cache.lock().unwrap_or_else(|p| p.into_inner()) =
        Some((std::time::Instant::now(), idx.clone()));
    Some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store is keyed by content hash, and every verdict round-trips —
    /// including the two owner overrides.
    #[test]
    fn a_verdict_is_recorded_and_read_back_by_hash() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        for v in [
            Verdict::Safe,
            Verdict::Borderline,
            Verdict::Unsafe,
            Verdict::Approved,
            Verdict::Blocked,
        ] {
            record("deadbeefcafe", v).expect("recorded");
            assert_eq!(verdict_for("deadbeefcafe"), Some(v));
        }
        assert_eq!(verdict_for("other"), None, "a different hash is unknown");
        let body = std::fs::read_to_string(store_path()).unwrap();
        assert_eq!(body.matches("deadbeefcafe").count(), 1, "one row per hash");
    }

    /// The bad list is unsafe | borderline | blocked; safe and approved are
    /// not on it (an approved proc's outputs still go through the check).
    #[test]
    fn the_bad_list_is_read_off_the_verdict() {
        assert!(Verdict::Unsafe.bad_list());
        assert!(Verdict::Borderline.bad_list());
        assert!(Verdict::Blocked.bad_list());
        assert!(!Verdict::Safe.bad_list());
        assert!(!Verdict::Approved.bad_list());
    }

    /// Rule 2: a blocked OUTPUT hash is withheld with NO hosted copy, even
    /// when the check passed and nothing else trips.
    #[test]
    fn a_blocked_output_gets_no_hosted_copy() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let s = shape_for_public(
            "answer",
            "42",
            &ShapeInputs {
                blocked_output: true,
                ..Default::default()
            },
            CheckOutcome::Pass,
        );
        assert_eq!(s.text, "Output withheld (content policy).");
        assert!(s.artifacts.is_none(), "no hosted copy for a blocked output");
        assert!(!s.text.contains(URL_PLACEHOLDER), "and no link");
        assert_eq!(
            s.kind,
            PostKind::Blocked,
            "no question: the owner already deleted it"
        );
    }

    /// Rule 2: a proc the owner Blocked is withheld with no hosted copy,
    /// even when its output would pass the check.
    #[test]
    fn a_blocked_proc_gets_no_hosted_copy() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        record("blockedhash1", Verdict::Blocked).unwrap();
        let s = shape_for_public(
            "badproc",
            "42",
            &ShapeInputs {
                hash: Some("blockedhash1"),
                ..Default::default()
            },
            CheckOutcome::Pass,
        );
        assert!(s.artifacts.is_none(), "no hosted copy");
        assert!(!s.text.contains("42"), "and nothing inline");
    }

    /// Rule 1: one stored proc, literal words, on the bad list is withheld
    /// WITH a link — even when its output passes the check (the bad list
    /// wins over a benign-looking output).
    #[test]
    fn a_bad_list_proc_stays_withheld_even_when_its_output_passes() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        for (hash, v) in [
            ("unsafehash01", Verdict::Unsafe),
            ("borderhash01", Verdict::Borderline),
        ] {
            record(hash, v).unwrap();
            let s = shape_for_public(
                "badproc",
                "42",
                &ShapeInputs {
                    hash: Some(hash),
                    ..Default::default()
                },
                CheckOutcome::Pass,
            );
            assert!(s.text.contains("withheld"), "{v:?}: {}", s.text);
            assert!(s.text.contains(URL_PLACEHOLDER), "{v:?} carries a link");
            assert!(!s.text.starts_with("42"), "{v:?} never inlines");
        }
    }

    /// An approved proc is OFF the bad list: a passing output goes inline.
    #[test]
    fn an_approved_proc_inlines_a_passing_output() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        record("approvedhash", Verdict::Approved).unwrap();
        let s = shape_for_public(
            "proc",
            "42",
            &ShapeInputs {
                hash: Some("approvedhash"),
                ..Default::default()
            },
            CheckOutcome::Pass,
        );
        assert!(
            s.text.starts_with("42"),
            "approved + pass inlines: {}",
            s.text
        );
        assert_eq!(s.kind, PostKind::Inline);
    }

    /// Rule 3: a passing result goes inline with its formatting link, and a
    /// passing result over the cap is elided head + link.
    #[test]
    fn a_passing_result_goes_inline_and_elides_over_the_cap() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let s = shape_for_public("answer", "42", &ShapeInputs::default(), CheckOutcome::Pass);
        assert!(s.text.starts_with("42"), "{}", s.text);
        assert!(
            s.text.contains(URL_PLACEHOLDER),
            "the formatting page is linked"
        );
        assert_eq!(s.kind, PostKind::Inline);

        let big = "x".repeat(INLINE_MAX_CHARS + 200);
        let s = shape_for_public("bigproc", &big, &ShapeInputs::default(), CheckOutcome::Pass);
        assert!(s.text.contains("truncated"), "elided: {}", s.text);
        assert!(s.text.contains(URL_PLACEHOLDER));
        let (md, _h) = s.artifacts.expect("artifacts");
        assert!(
            md.contains(&big),
            "the WHOLE output is published, not the head"
        );
    }

    /// Rule 4: a composed line whose output did NOT pass is withheld + link.
    /// (A composed line that passes goes inline — the whole point of the
    /// output check — covered by `a_passing_result_goes_inline…`.)
    #[test]
    fn a_composed_line_that_fails_the_check_is_withheld() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        for code in ["rainbow [x]", "rainbow; x", "rainbow $::v"] {
            let s = shape_for_public(
                code,
                "SLUR",
                &ShapeInputs {
                    hash: Some("somehash"),
                    ..Default::default()
                },
                CheckOutcome::Withheld,
            );
            assert!(s.text.contains("withheld"), "{code:?}: {}", s.text);
            assert!(!s.text.contains("SLUR"));
            assert_eq!(s.kind, PostKind::Withheld, "asked of the owner");
        }
    }

    /// The first output line's indentation is part of the drawing; blank
    /// LINES are dropped, the first content line keeps its leading spaces.
    #[test]
    fn the_first_drawn_line_keeps_its_indentation() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let art = "      __________\n  (--[ .]-[ .]\n   (_______O__)\n";
        let s = shape_for_public("draw", art, &ShapeInputs::default(), CheckOutcome::Withheld);
        let (md, html) = s.artifacts.expect("artifacts");
        assert!(
            md.contains("      __________"),
            "indent survives md: {md:?}"
        );
        assert!(
            html.contains("      __________"),
            "indent survives html: {html:?}"
        );
    }

    /// build_artifacts: the .md fences the output; the .html renders IRC
    /// color and drops raw HTML. Both hold the FULL result.
    #[test]
    fn artifacts_hold_the_full_result_and_render_color() {
        let (md, html) = build_artifacts("tcl fart", "\x034red\x0f and a drawing\n/^^\\");
        assert!(md.contains("```"), "the md is fenced");
        assert!(md.contains("/^^\\"), "the md keeps the drawing");
        assert!(html.contains("color:#ff0000"), "the html shows red: {html}");
        assert!(html.contains("/^^\\"), "the html keeps the drawing");
    }

    /// The post carries the text with mIRC and ANSI codes stripped — the
    /// grammar render_irc reads — and anything else a control byte stays
    /// for the art gate.
    #[test]
    fn formatting_codes_are_stripped_and_nothing_else() {
        assert_eq!(strip_irc_codes("\x034red\x0f \x02bold\x02"), "red bold");
        assert_eq!(strip_irc_codes("\x0304,12on blue"), "on blue");
        assert_eq!(
            strip_irc_codes("\x034,x"),
            ",x",
            "a comma with no digit stays"
        );
        assert_eq!(
            strip_irc_codes("\x0304,123"),
            "3",
            "two digits of background, at most"
        );
        assert_eq!(strip_irc_codes("\x1b[1;31mred\x1b[0m"), "red", "ANSI CSI");
        assert_eq!(
            strip_irc_codes("bell\x07"),
            "bell\x07",
            "other control bytes stay"
        );
    }

    /// public_text is what the post carries AND what the check reads: the
    /// codes stripped, blank lines trimmed with the first line's indent
    /// kept, and the head of an over-long output.
    #[test]
    fn public_text_is_the_posted_text() {
        let pt = public_text("\n\n   \x034indented\x0f red\nnext\n\n");
        assert_eq!(pt.body, "   indented red\nnext");
        assert!(!pt.elided);
        let big = "y".repeat(INLINE_MAX_CHARS + 1);
        let pt = public_text(&big);
        assert!(pt.elided);
        assert_eq!(pt.body.chars().count(), ELISION_HEAD_CHARS);
        // the inline post of a colored result is the stripped text
        let s = shape_for_public(
            "rainbow hi",
            "\x034hi\x0f",
            &ShapeInputs::default(),
            CheckOutcome::Pass,
        );
        assert!(s.text.starts_with("hi\n(with formatting:"), "{:?}", s.text);
        let (md, _) = s.artifacts.expect("hosted");
        assert!(md.contains("\x034hi"), "the hosted copy keeps the color");
    }

    /// The base library is reached one hop: a base proc the line names is
    /// judged by its verdict and its body is read only for the stamps it
    /// names. `<x>say`, `<x>think` and `-s <x>` reach `ST<x>` (slopdrop's
    /// handler draws `nazisay` with no proc of that name); `stampsay` with
    /// no `-s` is a random stamp, on the list.
    #[test]
    fn the_base_library_is_reached_one_hop_and_stamps_by_their_say_names() {
        let procs = serde_json::json!([
            {"name":"STbad","args":"","body":"return drawing","hash":"hst","origin":"base"},
            {"name":"STfine","args":"","body":"return drawing","hash":"hfine","origin":"base"},
            {"name":"badsay","args":"args","body":"stampsay -l -s bad [join $args]",
             "hash":"hsay","origin":"base"},
            {"name":"helper","args":"","body":"evilproc","hash":"hhelp","origin":"base"},
            {"name":"evilproc","args":"","body":"return awful","hash":"hevil"},
            {"name":"mine","args":"","body":"badsay hi","hash":"hmine"},
            {"name":"lucky","args":"","body":"stampsay hi","hash":"hlucky"},
            {"name":"randart","args":"","body":"stampsay x","hash":"hrand","origin":"base"},
        ]);
        let idx = ProcIndex::from_json(&procs, &serde_json::Value::Null);
        assert!(idx.base.contains("STbad") && !idx.base.contains("evilproc"));
        let mut v = std::collections::HashMap::new();
        v.insert("hst".to_string(), Verdict::Unsafe);
        v.insert("hfine".to_string(), Verdict::Safe);
        v.insert("hevil".to_string(), Verdict::Unsafe);
        for code in [
            "badsay hi",
            "badthink hi",
            "stampsay -l -s bad hi",
            "mine",
            "bsky_quote [bsky_get_parent] {{t} badsay $t}",
        ] {
            assert_eq!(
                idx.bad_reach(code, &v).as_deref(),
                Some("STbad"),
                "{code:?}"
            );
        }
        assert_eq!(idx.bad_reach("finesay hi", &v), None);
        assert_eq!(idx.bad_reach("stampsay -s fine hi", &v), None);
        assert_eq!(
            idx.bad_reach("stampsay hi", &v).as_deref(),
            Some("stampsay (a random stamp)")
        );
        assert_eq!(
            idx.bad_reach("helper", &v),
            None,
            "a base body's calls are not followed"
        );
        assert_eq!(idx.bad_reach("evilproc", &v).as_deref(), Some("evilproc"));
        assert_eq!(
            idx.bad_reach("lucky", &v).as_deref(),
            Some("lucky (a random stamp)"),
            "a tracked body's random stamp is the line's"
        );
        assert_eq!(
            idx.bad_reach("randart", &v),
            None,
            "a base proc's random stamp is its verdict's to judge"
        );
        v.insert("hrand".to_string(), Verdict::Unsafe);
        assert_eq!(idx.bad_reach("randart", &v).as_deref(), Some("randart"));
    }

    /// A line that REACHES a bad-list proc — by name inside a composed
    /// line, or through a stored body — is on the bad list too. Before,
    /// only a single literal call was, so `return [badproc]` went to the
    /// output check alone.
    #[test]
    fn a_line_that_reaches_a_bad_list_proc_is_on_the_bad_list() {
        let procs = serde_json::json!([
            {"name":"badproc","args":"","body":"return awful","hash":"hbad"},
            {"name":"wrapper","args":"","body":"return [string toupper [badproc]]","hash":"hwrap"},
            {"name":"clean","args":"x","body":"return $x","hash":"hclean"},
            {"name":"blessed","args":"","body":"badproc","hash":"hblessed"},
        ]);
        let idx = ProcIndex::from_json(&procs, &serde_json::Value::Null);
        let mut v = std::collections::HashMap::new();
        v.insert("hbad".to_string(), Verdict::Unsafe);
        v.insert("hwrap".to_string(), Verdict::Safe);
        v.insert("hclean".to_string(), Verdict::Safe);
        v.insert("hblessed".to_string(), Verdict::Approved);
        for code in [
            "return [badproc]",
            "badproc;",
            "x [::badproc]",
            "wrapper",
            "clean [wrapper]",
        ] {
            assert_eq!(
                idx.bad_reach(code, &v).as_deref(),
                Some("badproc"),
                "{code:?}"
            );
        }
        assert_eq!(idx.bad_reach("clean hello", &v), None);
        assert_eq!(idx.bad_reach("expr {10*34}", &v), None);
        assert_eq!(
            idx.bad_reach("blessed", &v),
            None,
            "an approved proc is the owner's override: not followed"
        );
        // and shaping holds it link-only even when the output passes
        let (_home, _env) = veles_core::testenv::scratch_home();
        let s = shape_for_public(
            "return [badproc]",
            "looks fine",
            &ShapeInputs {
                bad_reach: Some("reaches bad-list proc badproc".into()),
                ..Default::default()
            },
            CheckOutcome::Pass,
        );
        assert!(s.text.starts_with("Output withheld"), "{}", s.text);
        assert!(s.text.contains(URL_PLACEHOLDER));
        assert_eq!(s.reason, "reaches bad-list proc badproc");
    }

    /// The owner's allowlist outranks the bad list (they saw that exact
    /// text) but never a block.
    #[test]
    fn an_allowlisted_output_goes_inline_but_a_blocked_one_never() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        record("unsafeh2", Verdict::Unsafe).unwrap();
        let allow = ShapeInputs {
            hash: Some("unsafeh2"),
            allowlisted: true,
            bad_reach: Some("reaches bad-list proc x".into()),
            ..Default::default()
        };
        let s = shape_for_public("badproc", "42", &allow, CheckOutcome::Withheld);
        assert!(s.text.starts_with("42"), "{}", s.text);
        let s = shape_for_public(
            "badproc",
            "42",
            &ShapeInputs {
                blocked_output: true,
                ..allow
            },
            CheckOutcome::Pass,
        );
        assert!(s.artifacts.is_none() && !s.text.contains("42"));
    }

    /// composes: a literal call vs a composed line.
    #[test]
    fn a_composed_line_is_spelled_out() {
        assert!(!composes("rainbow hello"));
        for code in ["a [b]", "a\nb", "a; b", "a $v", "a {x}", "a \"x\"", "a \\x"] {
            assert!(composes(code), "{code:?}");
        }
    }

    /// The index: name lookup both ways (the withheld display and
    /// `/slopdrop approve proc <name>`).
    #[test]
    fn the_index_looks_names_up_both_ways() {
        let procs = serde_json::json!([
            {"name":"pure","args":"x","body":"return $x","hash":"h1"},
            {"name":"other","args":"","body":"return 1","hash":"h2"},
        ]);
        let idx = ProcIndex::from_json(&procs, &serde_json::Value::Null);
        assert_eq!(idx.name_for("h1"), Some("pure"));
        assert_eq!(idx.name_for("nope"), None);
        assert_eq!(idx.hash_for_name("other"), Some("h2"));
        assert_eq!(idx.hash_for_name("nope"), None);
    }
}
