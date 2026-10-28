//! Text art as a picture on a Bluesky reply (PLAN-slopdrop-art.md P3).
//!
//! slopdrop's art is written for an IRC client: a monospace grid with
//! mIRC colour, ANSI escapes and box, block and braille characters. A
//! Bluesky post is 300 graphemes in a proportional font, so such a reply
//! arrived as noise, or (the output check reads all of it as art) not at
//! all. Here it is drawn in the IRC font ([`veles_textart`]) and attached
//! as a PNG instead:
//!
//! - a reply whose whole output is a drawing ([`is_art`]) becomes that
//!   picture, and its text is only veles' own notes;
//! - every `bsky::art` the line recorded is one more picture, up to
//!   [`MAX_PICTURES`] together with the line's other images.
//!
//! Each picture is shown to the vision model ([`classify`]), once: the
//! answer is kept by the PNG's hash, and the same drawing always makes
//! the same bytes. What the model sees becomes the post's self-labels, or
//! holds the reply for the owner (`public.rs` decides). The PNG stays in
//! `<state>/art/` for a week, so the check, the owner's question and an
//! approve all read one file.
//!
//! The evaluation's RAW output reaches the policy in an envelope record
//! of the reply spec ([`envelope`]): the answer's text is cut and carries
//! veles' notes, which are no part of a drawing. A record whose kind
//! starts with `_` is veles' own, so one a proc recorded is a forgery and
//! is dropped ([`strip_private`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest as _;

/// The kind of veles' envelope record.
pub const RAW_KIND: &str = "_raw";
/// The envelope carries at most this much of the output. slopdrop sends
/// at most 200 lines to Bluesky, and the grid is cut at 123 rows anyway.
const RAW_MAX_BYTES: usize = 64 * 1024;
/// Bluesky's limit: four images on a post.
pub const MAX_PICTURES: usize = 4;
/// Bluesky's app takes alt text up to 2000 characters.
pub const ALT_MAX_CHARS: usize = 2000;
/// A long text output drawn as a picture is wrapped at this many columns
/// (PLAN-slopdrop-art.md P6): about a phone screen's line at feed width.
pub const TEXT_COLS: usize = 72;
/// How long a drawn PNG is kept.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// The vision route is a cheap flash model; past this the picture is
/// held, as for any other failed check.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(60);

/// The self-label values a post may carry
/// (`com.atproto.label.defs#selfLabels`): the ones Bluesky's app turns
/// into a content warning.
pub const SELF_LABELS: [&str; 4] = ["porn", "sexual", "nudity", "graphic-media"];

/// Every category the image check may name.
pub const CATEGORIES: [&str; 11] = [
    "sexual_explicit",
    "sexual",
    "nudity",
    "gore",
    "hate_symbol",
    "slur",
    "violence_threat",
    "minor",
    "personal_info",
    "harassment",
    "self_harm",
];

/// What holds even the OWNER's picture for a one-tap question (USER
/// 2026-10-12): anyone on IRC can redefine the proc the owner calls.
pub const OWNER_HOLD: [&str; 5] = [
    "hate_symbol",
    "slur",
    "violence_threat",
    "minor",
    "personal_info",
];

/// What withholds anyone else's picture: the owner's set, harassment and
/// self-harm.
pub const PUBLIC_WITHHOLD: [&str; 7] = [
    "hate_symbol",
    "slur",
    "violence_threat",
    "minor",
    "personal_info",
    "harassment",
    "self_harm",
];

fn kind(r: &Value) -> &str {
    r.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// `s`, cut to at most `max` bytes on a character boundary.
fn cap_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── the envelope ────────────────────────────────────────────────────

/// One evaluation's own output, as the bridge received it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Raw {
    /// The output, blank lines trimmed, the first line's indentation kept.
    pub output: String,
    /// What veles added after it (`(state: …)`, `(truncated — …)`).
    pub notes: String,
    /// The bridge's verdict: the output is an error message.
    pub error: bool,
}

/// The envelope record for one evaluation.
pub fn envelope(output: &str, notes: &str, error: bool) -> Value {
    json!({
        "kind": RAW_KIND,
        "output": cap_bytes(output, RAW_MAX_BYTES),
        "notes": notes,
        "error": error,
    })
}

/// Drop every record whose kind starts with `_` (veles' own kinds) from a
/// spec the CODE recorded.
pub fn strip_private(spec: &mut Option<Value>) {
    if let Some(Value::Array(recs)) = spec.as_mut() {
        recs.retain(|r| !kind(r).starts_with('_'));
    }
}

/// Split a spec into veles' envelope and the code's own records (`None`
/// when none are left).
pub fn take_raw(spec: Option<&Value>) -> (Option<Raw>, Option<Value>) {
    let Some(Value::Array(recs)) = spec else {
        return (None, spec.cloned());
    };
    let mut raw = None;
    let mut rest = Vec::with_capacity(recs.len());
    for r in recs {
        if kind(r) == RAW_KIND {
            let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            raw = Some(Raw {
                output: s("output"),
                notes: s("notes"),
                error: r.get("error").and_then(Value::as_bool).unwrap_or(false),
            });
        } else if !kind(r).starts_with('_') {
            rest.push(r.clone());
        }
    }
    (raw, (!rest.is_empty()).then_some(Value::Array(rest)))
}

// ── what is a drawing ───────────────────────────────────────────────

/// Box drawing, block elements, geometric shapes, braille, and the
/// legacy-computing blocks.
fn is_drawing_char(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x25FF | 0x2800..=0x28FF | 0x1FB00..=0x1FBFF)
}

/// Four of one symbol in a row (`____`, `====`, `||||`); dots are prose.
fn has_symbol_run(line: &str) -> bool {
    let (mut prev, mut n) = (None, 0);
    for c in line.chars() {
        if !c.is_alphanumeric() && !c.is_whitespace() && c != '.' {
            if Some(c) == prev {
                n += 1;
                if n >= 4 {
                    return true;
                }
            } else {
                (prev, n) = (Some(c), 1);
            }
        } else {
            (prev, n) = (None, 0);
        }
    }
    false
}

/// Is this output a drawing? Colour or reverse video (mIRC `\x03`,
/// `\x04`, `\x16`, any ANSI escape) always is: coloured text means
/// nothing once the colour is gone. Otherwise it takes two or more lines
/// that
/// - use a drawing character (box, block, braille), or
/// - have more symbols than letters and digits (`/__\`, `(oo)`), or
/// - carry a run of four of one symbol (`____`, `====`).
///
/// One line of anything stays text: emoji, non-ASCII prose, bold.
pub fn is_art(s: &str) -> bool {
    if s.chars()
        .any(|c| matches!(c, '\x03' | '\x04' | '\x16' | '\x1b'))
    {
        return true;
    }
    let text = veles_textart::visible(s);
    if text.lines().filter(|l| !l.trim().is_empty()).count() < 2 {
        return false;
    }
    if text.chars().any(is_drawing_char) {
        return true;
    }
    let letters = text.chars().filter(|c| c.is_alphanumeric()).count();
    let symbols = text
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .count();
    symbols > letters || text.lines().any(has_symbol_run)
}

// ── what a reply draws ──────────────────────────────────────────────

/// One picture to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub text: String,
    /// The alt text the code gave (`bsky::art -alt`); empty = ours.
    pub alt: String,
    pub theme: veles_textart::Theme,
}

/// What a reply draws, and the self-labels its code asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The output itself first when it is a drawing, then each
    /// `bsky::art` in the order recorded.
    pub sources: Vec<Source>,
    /// The output is the first source: the post's text is only the notes.
    pub output_is_art: bool,
    /// `bsky::label` values, only [`SELF_LABELS`].
    pub labels: Vec<String>,
    /// `bsky::magick` image macros (P6), in the order recorded, then the
    /// `magick_scheme` drawings.
    pub macros: Vec<crate::magick::Macro>,
    /// The `magick:<n>` handles the `magick_scheme` calls returned: the
    /// output names its drawings by them, and they are not post text.
    pub handles: Vec<String>,
    /// What the code recorded that could not be used, as reply notes.
    pub notes: Vec<String>,
}

/// The [`Plan`] for one evaluation. `raw` is `None` off the `/tcl` path
/// (a trigger's answer), which never draws its output.
pub fn plan(raw: Option<&Raw>, spec: Option<&Value>) -> Plan {
    let mut p = Plan::default();
    if let Some(r) = raw.filter(|r| !r.error && is_art(&r.output)) {
        p.output_is_art = true;
        p.sources.push(Source {
            text: r.output.clone(),
            alt: String::new(),
            theme: veles_textart::Theme::Dark,
        });
    }
    let mut programs: Vec<(crate::scheme::Program, String)> = Vec::new();
    for r in spec.and_then(Value::as_array).into_iter().flatten() {
        let f = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
        match kind(r) {
            "magick" if r.get("scheme").is_some() => {
                let id = f("id").trim();
                if id.is_empty() || id.len() > 4 || !id.bytes().all(|b| b.is_ascii_digit()) {
                    p.notes
                        .push("(an image macro was left out: no handle)".to_string());
                } else if programs.len() >= crate::scheme::MAX_PROGRAMS {
                    p.notes.push(format!(
                        "(at most {} magick_scheme programs in one reply)",
                        crate::scheme::MAX_PROGRAMS
                    ));
                } else {
                    p.handles.push(crate::scheme::handle(id));
                    programs.push((
                        crate::scheme::Program {
                            id: id.to_string(),
                            text: cap_bytes(f("scheme"), crate::scheme::MAX_PROGRAM_BYTES)
                                .to_string(),
                        },
                        f("alt").trim().chars().take(ALT_MAX_CHARS).collect(),
                    ));
                }
            }
            "art" if !f("text").trim().is_empty() => p.sources.push(Source {
                text: cap_bytes(f("text"), RAW_MAX_BYTES).to_string(),
                alt: f("alt").trim().chars().take(ALT_MAX_CHARS).collect(),
                theme: veles_textart::Theme::parse(f("theme")).unwrap_or_default(),
            }),
            "magick" => match crate::magick::parse(f("src"), f("ops"), f("alt")) {
                Ok(m) => p.macros.push(m),
                Err(e) => p.notes.push(format!("(an image macro was left out: {e})")),
            },
            "label" => {
                let v = f("val").trim().to_ascii_lowercase();
                if SELF_LABELS.contains(&v.as_str()) && !p.labels.contains(&v) {
                    p.labels.push(v);
                }
            }
            _ => {}
        }
    }
    let progs: Vec<crate::scheme::Program> = programs.iter().map(|(p, _)| p.clone()).collect();
    for d in crate::scheme::roots(&progs) {
        let alt = programs
            .iter()
            .find(|(p, _)| p.id == d.root().id)
            .map(|(_, a)| a.clone())
            .unwrap_or_default();
        p.macros.push(crate::magick::Macro {
            body: crate::magick::Body::Scheme(d),
            alt,
        });
    }
    p
}

/// `text` without the `magick_scheme` handles the reply's drawings came
/// from (`magick:3`, never `magick:31` for `magick:3`): the drawing is
/// the answer, not its handle. What is left is the post's text; only
/// whitespace and the braces Tcl quotes a list with is none.
pub fn without_handles(text: &str, handles: &[String]) -> String {
    if handles.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while !rest.is_empty() {
        for h in handles {
            if let Some(after) = rest.strip_prefix(h.as_str()) {
                if !after.starts_with(|c: char| c.is_ascii_digit()) {
                    rest = after;
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    if out
        .chars()
        .all(|c| c.is_whitespace() || c == '{' || c == '}')
    {
        String::new()
    } else {
        out
    }
}

// ── drawing ─────────────────────────────────────────────────────────

/// One drawn picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub png: Vec<u8>,
    /// `<state>/art/<sha>.png`.
    pub path: PathBuf,
    /// sha256 of the PNG: the classifier's cache key.
    pub sha: String,
    pub width: u32,
    pub height: u32,
    /// The code's alt text, or empty.
    pub alt: String,
    /// What a reader can make out in it (`veles_textart::words`).
    pub words: String,
    /// Its text as shown, formatting gone, punctuation kept
    /// (`veles_textart::visible`): what the link and contact checks read,
    /// since `words` drops the `://` of a link.
    pub visible: String,
    /// The drawing was larger than the grid and was cut.
    pub truncated: bool,
    /// sha256 of the drawing's text (`check::text_hash`): what the
    /// owner's allowlist and blocked list key it by.
    pub text_hash: String,
}

impl Picture {
    /// The ledger's reference to this picture, with its alt text.
    pub fn reference(&self, alt: &str) -> crate::withheld::PictureRef {
        crate::withheld::PictureRef {
            sha: self.sha.clone(),
            alt: alt.to_string(),
            width: self.width,
            height: self.height,
            text_hash: self.text_hash.clone(),
        }
    }
}

/// A kept picture's PNG bytes, by its sha (`None` once the week is up).
pub fn kept_png(sha: &str) -> Option<Vec<u8>> {
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    std::fs::read(art_dir().join(format!("{sha}.png"))).ok()
}

/// Where the drawn PNGs are kept.
pub fn art_dir() -> PathBuf {
    crate::data::dir().join("art")
}

/// Draw one source and keep the PNG in [`art_dir`]. CPU-bound (a large
/// drawing is tens of milliseconds): call it off the async workers.
pub fn draw(src: &Source) -> Result<Picture, String> {
    let opts = veles_textart::Options {
        theme: src.theme,
        ..Default::default()
    };
    let png = veles_textart::render(&src.text, &opts).map_err(|e| e.to_string())?;
    let visible = veles_textart::visible(&src.text);
    let words = veles_textart::words(&src.text);
    keep(png.into(), &src.text, &src.alt, words, visible)
}

/// A long text output as one picture (P6), word-wrapped at
/// [`TEXT_COLS`], its alt text the text itself. `visible` is the text as
/// wrapped and cut in the picture: what the word checks read.
pub fn draw_text(text: &str) -> Result<Picture, String> {
    let opts = veles_textart::Options::default();
    let png = veles_textart::render_text(text, TEXT_COLS, &opts).map_err(|e| e.to_string())?;
    let visible = veles_textart::visible_wrapped(text, TEXT_COLS);
    let words = veles_textart::words(text);
    let mut alt: String = text.trim().chars().take(ALT_MAX_CHARS).collect();
    if png.truncated || text.trim().chars().count() > ALT_MAX_CHARS {
        let note = " … (cut; the full text is at the link)";
        let keep_chars = ALT_MAX_CHARS - note.chars().count();
        alt = alt.chars().take(keep_chars).collect::<String>() + note;
    }
    keep(png.into(), text, &alt, words, visible)
}

/// A PNG ready to keep: drawn here, or an image macro's
/// (`magick::render`).
pub struct Drawn {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Cut to fit (a drawing past the grid).
    pub truncated: bool,
}

impl From<veles_textart::Png> for Drawn {
    fn from(p: veles_textart::Png) -> Self {
        Drawn {
            bytes: p.bytes,
            width: p.width,
            height: p.height,
            truncated: p.truncated,
        }
    }
}

/// Keep a PNG in [`art_dir`] and describe it. `text` is what the ledger
/// hashes for the owner's approve and delete; `words` and `visible` are
/// what a reader can make out in it.
pub fn keep(
    png: Drawn,
    text: &str,
    alt: &str,
    words: String,
    visible: String,
) -> Result<Picture, String> {
    let sha = format!("{:x}", sha2::Sha256::digest(&png.bytes));
    let dir = art_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{sha}.png"));
    if !path.exists() {
        let tmp = dir.join(format!(".{sha}.png.tmp"));
        std::fs::write(&tmp, &png.bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    sweep(&dir);
    Ok(Picture {
        path,
        sha,
        width: png.width,
        height: png.height,
        alt: alt.to_string(),
        words,
        visible,
        truncated: png.truncated,
        text_hash: crate::check::text_hash(text),
        png: png.bytes,
    })
}

/// Remove kept PNGs older than [`KEEP`], at most once an hour.
fn sweep(dir: &std::path::Path) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.ends_with(".png") && !name.ends_with(".png.tmp") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The alt text a picture goes out with: the code's, else the one the
/// image check wrote, else the words it shows. A cut drawing says so.
pub fn alt_for(pic: &Picture, seen: Option<&Seen>) -> String {
    let mut alt = if !pic.alt.is_empty() {
        pic.alt.clone()
    } else if let Some(a) = seen.map(|s| s.alt.trim()).filter(|a| !a.is_empty()) {
        a.to_string()
    } else if pic.words.is_empty() {
        "Text art".to_string()
    } else {
        format!("Text art: {}", pic.words)
    };
    if pic.truncated {
        alt.push_str(" (the drawing was cut to fit)");
    }
    alt.chars().take(ALT_MAX_CHARS).collect()
}

/// The spec record that carries a picture to the atproto plugin.
pub fn record(pic: &Picture, alt: &str) -> Value {
    record_of(&pic.png, alt, pic.width, pic.height)
}

/// [`record`] from a kept PNG's bytes (an approve's repost).
pub fn record_of(png: &[u8], alt: &str, width: u32, height: u32) -> Value {
    use base64::Engine as _;
    json!({
        "kind": "art",
        "png": base64::engine::general_purpose::STANDARD.encode(png),
        "alt": alt,
        "w": width.to_string(),
        "h": height.to_string(),
    })
}

// ── the image check ─────────────────────────────────────────────────

/// What the vision model saw in one picture.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// Only [`CATEGORIES`] names.
    pub categories: Vec<String>,
    #[serde(default)]
    pub reason: String,
    /// Its alt text for the picture.
    #[serde(default)]
    pub alt: String,
}

impl Seen {
    /// The self-labels what it saw calls for.
    pub fn labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for c in &self.categories {
            let l = match c.as_str() {
                "sexual_explicit" => "porn",
                "sexual" => "sexual",
                "nudity" => "nudity",
                "gore" => "graphic-media",
                _ => continue,
            };
            if !out.contains(&l) {
                out.push(l);
            }
        }
        out
    }

    /// The categories that hold this picture: [`OWNER_HOLD`] for the
    /// owner, [`PUBLIC_WITHHOLD`] for anyone else.
    pub fn holds(&self, owner: bool) -> Vec<&str> {
        let set: &[&str] = if owner { &OWNER_HOLD } else { &PUBLIC_WITHHOLD };
        self.categories
            .iter()
            .map(String::as_str)
            .filter(|c| set.contains(c))
            .collect()
    }
}

/// The question. The categories are named and defined so the answer is
/// one of them or nothing; the alt text is asked for in the same call.
pub const PROMPT: &str = "You check one picture before a bot posts it publicly on Bluesky. \
The picture is IRC text art (ASCII, ANSI or mIRC art) drawn in a monospace font: read every \
word in it as well as looking at the drawing.\n\
Answer with ONE JSON object and nothing else:\n\
{\"categories\": [], \"reason\": \"\", \"alt\": \"\"}\n\
\"categories\" lists every one of these names that applies, and nothing else:\n\
- \"sexual_explicit\": sexual acts or genitals shown explicitly\n\
- \"sexual\": sexual content that is not explicit\n\
- \"nudity\": nudity that is not sexual\n\
- \"gore\": graphic injury, blood or mutilation\n\
- \"hate_symbol\": a hate symbol (swastika, SS runes, KKK hood, ...) or imagery that demeans a group\n\
- \"slur\": a slur against a race, ethnicity, religion, sexuality, gender or disability\n\
- \"violence_threat\": a threat of violence against a person or a group\n\
- \"minor\": a child shown in a sexual, suggestive or abusive way\n\
- \"personal_info\": a home address, phone number, email or other private detail of a person\n\
- \"harassment\": insults or mockery aimed at a real, named person\n\
- \"self_harm\": encouraging or showing suicide or self-harm\n\
Leave the list empty when none apply: crude humour, swearing, cartoon violence and \
mockery of no one in particular are fine.\n\
\"reason\": one short sentence saying why each listed name applies (\"\" when none do).\n\
\"alt\": alt text for a blind reader, at most 300 characters: what the picture shows, then \
its words verbatim.";

/// Read the model's answer: the last JSON object in it that has a
/// `categories` list (a thinking model may write one in its reasoning, or
/// wrap the answer in a code fence). An unknown category fails: what the
/// model meant is unknown, so the picture is held.
pub fn parse_answer(answer: &str) -> Result<Seen, String> {
    let answer = match answer.rfind("</think>") {
        Some(i) => &answer[i + "</think>".len()..],
        None => answer,
    };
    let starts: Vec<usize> = answer.match_indices('{').map(|(i, _)| i).collect();
    let mut found = None;
    for &i in starts.iter().rev() {
        let mut it = serde_json::Deserializer::from_str(&answer[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            if v.get("categories").is_some() {
                found = Some(v);
                break;
            }
        }
    }
    let v = found.ok_or("the image check's answer has no JSON verdict")?;
    let cats = v
        .get("categories")
        .and_then(Value::as_array)
        .ok_or("the image check's categories are not a list")?;
    let mut categories: Vec<String> = Vec::new();
    for c in cats {
        let c = c
            .as_str()
            .ok_or("the image check named a category that is not a string")?
            .trim()
            .to_ascii_lowercase();
        if c.is_empty() || c == "none" {
            continue;
        }
        if !CATEGORIES.contains(&c.as_str()) {
            return Err(format!("the image check named an unknown category {c:?}"));
        }
        if !categories.contains(&c) {
            categories.push(c);
        }
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Ok(Seen {
        categories,
        reason: s("reason").chars().take(500).collect(),
        alt: s("alt").chars().take(ALT_MAX_CHARS).collect(),
    })
}

fn seen_path() -> PathBuf {
    art_dir().join("seen.json")
}

fn seen_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|p| p.into_inner())
}

fn seen_read() -> HashMap<String, Seen> {
    std::fs::read_to_string(seen_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The verdict kept for this PNG, if it was checked before.
pub fn cached(sha: &str) -> Option<Seen> {
    let _g = seen_lock();
    seen_read().remove(sha)
}

/// Keep a verdict. A failed write costs one more model call next time.
pub fn remember(sha: &str, seen: &Seen) {
    let _g = seen_lock();
    let mut all = seen_read();
    all.insert(sha.to_string(), seen.clone());
    let path = seen_path();
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_vec(&all)
        .ok()
        .and_then(|b| std::fs::write(&tmp, b).ok())
        .and_then(|_| std::fs::rename(&tmp, &path).ok());
    if ok.is_none() {
        veles_core::module::log(
            tracing::Level::WARN,
            &format!(
                "art: keeping the image check's verdict in {} failed",
                path.display()
            ),
        );
    }
}

/// Show one picture to the `[vision]` route and read its verdict (or the
/// kept one). Any failure is an `Err`, and the caller holds the picture.
pub async fn classify(
    cfg: &veles_core::config::VelesConfig,
    pic: &Picture,
) -> Result<Seen, String> {
    classify_kept(cfg, &pic.sha, &pic.path).await
}

/// [`classify`] for a kept PNG, by its sha and path.
pub async fn classify_kept(
    cfg: &veles_core::config::VelesConfig,
    sha: &str,
    path: &std::path::Path,
) -> Result<Seen, String> {
    if let Some(s) = cached(sha) {
        return Ok(s);
    }
    let path = path.to_string_lossy().to_string();
    let answer = tokio::time::timeout(
        CLASSIFY_TIMEOUT,
        veles_core::vision::describe_url(cfg, &path, PROMPT),
    )
    .await
    .map_err(|_| format!("the image check took over {}s", CLASSIFY_TIMEOUT.as_secs()))?
    .map_err(|e| format!("the image check failed: {e}"))?;
    let seen = parse_answer(&answer)?;
    remember(sha, &seen);
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEDORA: &str = "        _____,          hello world\n       /_ ___\\\\      /             \n   ___|________|___                \n     (--[ .]-[ .]                      \n     (_______O__)                  \n   ,\"`          \\                  \n  /____/         \\                 \n  |   |    _______\\                \n  \\    \\_-`_____.__)               \n   `-_ /          |                \n      |      ,`   |                \n      |      |    |                \n      |______|____|                \n      |______,_____,               ";
    const COW: &str =
        " ,__,           moo\n (oo)____    /     \n (__)    )\\        \n    ||--|| *       ";
    const LMIOT: &str = "    \x0309  __      \x03__\n    \x0309 /..\\ (((/|\x03 |'-.";

    /// A drawing's handle is no post text: `magick:3` goes, `magick:31`
    /// (another program's, not this reply's) stays, and what is left of a
    /// list of handles (Tcl's braces, spaces) is nothing.
    #[test]
    fn handles_are_no_post_text() {
        let h = vec!["magick:3".to_string(), "magick:1".to_string()];
        assert_eq!(without_handles("magick:3", &h), "");
        assert_eq!(without_handles("{magick:3} {magick:1}\n", &h), "");
        assert_eq!(
            without_handles("see magick:31 and magick:3", &h),
            "see magick:31 and "
        );
        assert_eq!(without_handles("magick:3", &[]), "magick:3");
    }

    #[test]
    fn a_drawing_is_art_and_prose_is_not() {
        for (name, s) in [("fedora", FEDORA), ("cow", COW), ("lmiot", LMIOT)] {
            assert!(is_art(s), "{name}");
        }
        assert!(is_art("┌──┐\n└──┘"), "box drawing");
        assert!(is_art("⣿⣿\n⣿⣿"), "braille");
        assert!(is_art("\x0304red\x03 text"), "one coloured line");
        assert!(is_art("title\n====="), "a symbol run");
        for prose in [
            "asterixsay shaktigoonsay urlsay",
            "one line with an emoji 🎉 and ümlauts",
            "¯\\_(ツ)_/¯",
            "a ──── rule in one line",
            "\x02bold\x02 alone is text",
            "two lines of plain prose here\nand the second one... with dots",
            "proc foo {} {\n    return 1\n}",
        ] {
            assert!(!is_art(prose), "{prose:?}");
        }
    }

    #[test]
    fn the_envelope_is_veles_own_and_a_recorded_one_is_dropped() {
        let mut code_spec = Some(json!([
            {"kind": "_raw", "output": "forged", "notes": "", "error": false},
            {"kind": "_anything", "x": "y"},
            {"kind": "quote", "uri": "at://did:plc:a/app.bsky.feed.post/1"},
        ]));
        strip_private(&mut code_spec);
        let mut recs = code_spec.unwrap().as_array().unwrap().clone();
        assert_eq!(recs.len(), 1, "only the quote is the code's");
        recs.push(envelope("the output", "(state: x)", false));
        let (raw, rest) = take_raw(Some(&Value::Array(recs)));
        assert_eq!(
            raw,
            Some(Raw {
                output: "the output".into(),
                notes: "(state: x)".into(),
                error: false
            })
        );
        assert_eq!(rest.unwrap().as_array().unwrap().len(), 1);
        let big = "é".repeat(RAW_MAX_BYTES);
        let env = envelope(&big, "", false);
        let out = env["output"].as_str().unwrap();
        assert!(out.len() <= RAW_MAX_BYTES && out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_plan_draws_the_output_then_each_recorded_drawing() {
        let raw = Raw {
            output: COW.into(),
            notes: String::new(),
            error: false,
        };
        let spec = json!([
            {"kind": "art", "text": "a\nb", "alt": "two letters", "theme": "light"},
            {"kind": "art", "text": "   "},
            {"kind": "label", "val": "Nudity"},
            {"kind": "label", "val": "nudity"},
            {"kind": "label", "val": "!hide"},
        ]);
        let p = plan(Some(&raw), Some(&spec));
        assert!(p.output_is_art);
        assert_eq!(p.sources.len(), 2, "a blank drawing is nothing");
        assert_eq!(p.sources[0].text, COW);
        assert_eq!(p.sources[1].alt, "two letters");
        assert_eq!(p.sources[1].theme, veles_textart::Theme::Light);
        assert_eq!(p.labels, ["nudity"], "only self-label values, once");
        let err = Raw { error: true, ..raw };
        assert!(!plan(Some(&err), None).output_is_art, "an error is text");
        assert!(
            plan(None, None).sources.is_empty(),
            "a trigger draws nothing"
        );
    }

    #[test]
    fn the_verdict_is_read_strictly() {
        let ok = parse_answer(
            "<think>maybe {\"categories\": [\"slur\"]}</think>```json\n\
             {\"categories\": [\"nudity\", \"None\", \"gore\"], \"reason\": \"r\", \"alt\": \"a man\"}\n```",
        )
        .unwrap();
        assert_eq!(ok.categories, ["nudity", "gore"]);
        assert_eq!(ok.labels(), ["nudity", "graphic-media"]);
        assert_eq!(ok.alt, "a man");
        assert!(ok.holds(true).is_empty() && ok.holds(false).is_empty());
        let hate = parse_answer(r#"{"categories": ["hate_symbol", "harassment"]}"#).unwrap();
        assert_eq!(hate.holds(true), ["hate_symbol"]);
        assert_eq!(hate.holds(false), ["hate_symbol", "harassment"]);
        let bully = parse_answer(r#"{"categories": ["harassment"]}"#).unwrap();
        assert!(
            bully.holds(true).is_empty(),
            "the owner's set has no harassment"
        );
        for bad in [
            "it looks fine",
            r#"{"reason": "no list"}"#,
            r#"{"categories": "slur"}"#,
            r#"{"categories": ["weird"]}"#,
            r#"{"categories": [3]}"#,
        ] {
            assert!(parse_answer(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_drawing_is_kept_once_and_its_verdict_is_remembered() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let src = Source {
            text: COW.into(),
            alt: String::new(),
            theme: veles_textart::Theme::Dark,
        };
        let a = draw(&src).unwrap();
        let b = draw(&src).unwrap();
        assert_eq!(a.sha, b.sha, "the same drawing, the same bytes");
        assert_eq!(std::fs::read(&a.path).unwrap(), a.png);
        assert_eq!(a.words, "moo oo", "the cow's eyes read as a word");
        assert_eq!(alt_for(&a, None), "Text art: moo oo");
        let seen = Seen {
            categories: vec![],
            reason: String::new(),
            alt: "a cow says moo".into(),
        };
        assert_eq!(alt_for(&a, Some(&seen)), "a cow says moo");
        assert!(cached(&a.sha).is_none());
        remember(&a.sha, &seen);
        assert_eq!(cached(&a.sha), Some(seen));
        let r = record(&a, "alt");
        assert_eq!(r["kind"], "art");
        assert_eq!(r["w"], a.width.to_string());
        use base64::Engine as _;
        let back = base64::engine::general_purpose::STANDARD
            .decode(r["png"].as_str().unwrap())
            .unwrap();
        assert_eq!(back, a.png);
    }
}
