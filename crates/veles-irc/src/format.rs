//! Formatting layer — the "do better" core.
//!
//! Agent output is markdown. IRC speaks formatting codes (\x02 bold,
//! \x1D italic, \x11 monospace, \x1F underline, \x16 reverse, \x03
//! color, \x0F reset). This module converts between them with one
//! non-negotiable rule:
//!
//! **Code content passes through byte-exact.** Fenced code blocks and
//! inline `code` spans are protected BEFORE any conversion runs, so the
//! hermes ircx bug — markdown stripping eating `_` and `*` out of Tcl
//! code sent over IRC — is structurally impossible here.
//!
//! Pipeline (mirrors the telegram MarkdownV2 port's placeholder-stash
//! design):
//!   1. Protect fenced code blocks → stash, render as monospace lines
//!   2. Protect inline code spans → stash, render as monospace
//!   3. Block pass: headings, bullets, tasks, quotes, tables, hr
//!   4. Inline pass on remaining text: **bold**, *italic*, [links](url)
//!   5. Restore stashed code regions
//!
//! `FormatMode::Markdown` skips ALL of this (byte-exact markdown, only
//! when a channel explicitly asks for it). `FormatMode::Plain` strips
//! to clean text.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

// ── IRC formatting codes ─────────────────────────────────────────────

pub const BOLD: char = '\x02';
pub const ITALIC: char = '\x1D';
pub const MONOSPACE: char = '\x11';
pub const UNDERLINE: char = '\x1F';
pub const REVERSE: char = '\x16';
pub const COLOR: char = '\x03';
pub const RESET: char = '\x0F';

/// Active formatting state while walking a formatted string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FmtState {
    bold: bool,
    italic: bool,
    monospace: bool,
    underline: bool,
    reverse: bool,
}

impl FmtState {
    /// Re-emit the active codes in canonical order (used when a line is
    /// split mid-formatting — the continuation line must reopen them).
    fn reopen_codes(&self) -> String {
        let mut out = String::new();
        if self.bold {
            out.push(BOLD);
        }
        if self.italic {
            out.push(ITALIC);
        }
        if self.monospace {
            out.push(MONOSPACE);
        }
        if self.underline {
            out.push(UNDERLINE);
        }
        if self.reverse {
            out.push(REVERSE);
        }
        out
    }
}

/// True when the char is an IRC formatting code.
fn is_format_code(c: char) -> bool {
    matches!(
        c,
        '\x02' | '\x1D' | '\x11' | '\x1F' | '\x16' | '\x03' | '\x0F'
    )
}

/// Visible byte length (formatting codes and their color args excluded).
pub fn visible_len(text: &str) -> usize {
    let mut n = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == COLOR {
            // consume up to two 2-digit numbers (fg,bg)
            let mut digits = 0;
            let mut commas = 0;
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_digit() && digits < 4 {
                    digits += 1;
                    chars.next();
                } else if nc == ',' && commas == 0 && digits > 0 && digits <= 2 {
                    commas += 1;
                    digits = 0;
                    chars.next();
                } else {
                    break;
                }
            }
            continue;
        }
        if is_format_code(c) {
            continue;
        }
        n += c.len_utf8();
    }
    n
}

/// Strip IRC formatting codes → plain visible text.
pub fn strip_irc_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == COLOR {
            // skip the color arguments too
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_digit() || nc == ',' {
                    chars.next();
                } else {
                    break;
                }
            }
            continue;
        }
        if is_format_code(c) {
            continue;
        }
        out.push(c);
    }
    out
}

// ── the converter ────────────────────────────────────────────────────

struct Stash {
    map: HashMap<String, String>,
    n: usize,
}

impl Stash {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            n: 0,
        }
    }
    fn put(&mut self, value: String) -> String {
        let key = format!("\x00IRC{n}\x00", n = self.n);
        self.n += 1;
        self.map.insert(key.clone(), value);
        key
    }
    fn restore(&self, mut text: String) -> String {
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort_by(|a, b| {
            let na: usize = a
                .trim_matches(|c: char| {
                    c == '\x00' || c.is_ascii_digit() || c == 'I' || c == 'R' || c == 'C'
                })
                .parse()
                .unwrap_or(0);
            let nb: usize = b
                .trim_matches(|c: char| {
                    c == '\x00' || c.is_ascii_digit() || c == 'I' || c == 'R' || c == 'C'
                })
                .parse()
                .unwrap_or(0);
            nb.cmp(&na)
        });
        for k in keys {
            text = text.replace(k, &self.map[k]);
        }
        text
    }
}

/// Convert markdown to IRC-formatted text. Code content is byte-exact.
pub fn markdown_to_irc(content: &str) -> String {
    if content.trim().is_empty() {
        return content.to_string();
    }
    let mut stash = Stash::new();
    let mut out_lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut fence_lines: Vec<String> = Vec::new();

    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // ── fenced code: byte-exact, rendered as monospace lines ──
        if trimmed.starts_with("```") {
            if !in_fence {
                in_fence = true;
                fence_lines.clear();
                i += 1;
                continue;
            } else {
                in_fence = false;
                out_lines.push(stash_fence(&mut stash, &fence_lines));
                i += 1;
                continue;
            }
        }
        if in_fence {
            fence_lines.push(line.to_string());
            i += 1;
            continue;
        }

        // ── tables: hermes-style bullet groups ──
        if line.contains('|') && i + 1 < lines.len() && table_sep_re().is_match(lines[i + 1]) {
            let mut block = vec![line.to_string(), lines[i + 1].to_string()];
            let mut j = i + 2;
            while j < lines.len() && table_row_re().is_match(lines[j]) {
                block.push(lines[j].to_string());
                j += 1;
            }
            out_lines.push(stash.put(render_table_irc(&block)));
            i = j;
            continue;
        }

        // ── headings → bold ──
        if let Some(rest) = heading_re().captures(line).map(|c| c[1].to_string()) {
            out_lines.push(format!(
                "{BOLD}{}{BOLD}",
                inline_convert(&mut stash, rest.trim().to_string())
            ));
            i += 1;
            continue;
        }

        // ── task lists ──
        if let Some(caps) = task_re().captures(line) {
            let done = &caps[1] == "x" || &caps[1] == "X";
            let marker = if done { "☑" } else { "☐" };
            out_lines.push(format!(
                "{marker} {}",
                inline_convert(&mut stash, caps[2].to_string())
            ));
            i += 1;
            continue;
        }

        // ── bullets ──
        if let Some(rest) = bullet_re().captures(line).map(|c| c[1].to_string()) {
            out_lines.push(format!("• {}", inline_convert(&mut stash, rest)));
            i += 1;
            continue;
        }

        // ── blockquote → italic ──
        if let Some(rest) = quote_re().captures(line).map(|c| c[1].to_string()) {
            out_lines.push(format!(
                "{ITALIC}> {}{ITALIC}",
                inline_convert(&mut stash, rest)
            ));
            i += 1;
            continue;
        }

        // ── horizontal rule → blank ──
        if hr_re().is_match(trimmed) {
            out_lines.push(String::new());
            i += 1;
            continue;
        }

        // ── ordinary line: inline conversion only ──
        out_lines.push(inline_convert(&mut stash, line.to_string()));
        i += 1;
    }

    // Unclosed fence: still render what we collected (defensive — model
    // output frequently forgets the closing backticks).
    if in_fence && !fence_lines.is_empty() {
        out_lines.push(stash_fence(&mut stash, &fence_lines));
    }

    let joined = out_lines.join("\n");
    stash.restore(joined)
}

/// Stash a fenced block as monospace lines. Content is byte-exact
/// (only CR/LF/NUL stripped for protocol safety — the split happens at
/// send time and IRC lines cannot contain newlines).
fn stash_fence(stash: &mut Stash, lines: &[String]) -> String {
    let rendered: Vec<String> = lines
        .iter()
        .map(|l| {
            let safe = crate::sanitize_outbound(l);
            format!("{MONOSPACE}{safe}{MONOSPACE}")
        })
        .collect();
    stash.put(rendered.join("\n"))
}

fn inline_convert(stash: &mut Stash, text: String) -> String {
    // Protect inline code FIRST (byte-exact).
    static INLINE_CODE: OnceLock<Regex> = OnceLock::new();
    let inline_code = INLINE_CODE.get_or_init(|| Regex::new(r"`([^`\n]+)`").unwrap());
    let text = inline_code
        .replace_all(&text, |caps: &regex::Captures| {
            let code = crate::sanitize_outbound(&caps[1]);
            stash.put(format!("{MONOSPACE}{code}{MONOSPACE}"))
        })
        .into_owned();

    // Links → text (url) — IRC clients auto-linkify URLs.
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
    let text = link
        .replace_all(&text, |caps: &regex::Captures| {
            format!("{} ({})", &caps[1], &caps[2])
        })
        .into_owned();

    // Bold **x** → \x02x\x02
    static BOLD_RE: OnceLock<Regex> = OnceLock::new();
    let bold_re = BOLD_RE.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    let text = bold_re
        .replace_all(&text, |caps: &regex::Captures| {
            format!("{BOLD}{}{BOLD}", &caps[1])
        })
        .into_owned();

    // Italic *x* (single star; bold pairs already consumed above).
    // Rust's regex crate has no lookarounds, so boundary checks happen
    // via capture groups that get re-emitted.
    static ITALIC_RE: OnceLock<Regex> = OnceLock::new();
    let italic_re =
        ITALIC_RE.get_or_init(|| Regex::new(r"(^|[^*\w])\*([^*\n]+)\*([^*\w]|$)").unwrap());
    let text = italic_re
        .replace_all(&text, |caps: &regex::Captures| {
            format!("{}{ITALIC}{}{ITALIC}{}", &caps[1], &caps[2], &caps[3])
        })
        .into_owned();

    // Italic _x_ at word boundaries (never snake_case_identifiers!):
    // the boundaries are captured and re-emitted.
    static UNDER_RE: OnceLock<Regex> = OnceLock::new();
    let under_re = UNDER_RE.get_or_init(|| Regex::new(r"(^|[^\w_])_([^_\n]+)_($|[^\w_])").unwrap());
    let text = under_re
        .replace_all(&text, |caps: &regex::Captures| {
            format!("{}{ITALIC}{}{ITALIC}{}", &caps[1], &caps[2], &caps[3])
        })
        .into_owned();

    // Strikethrough ~~x~~ → x (IRC has no strikethrough code)
    static STRIKE_RE: OnceLock<Regex> = OnceLock::new();
    let strike_re = STRIKE_RE.get_or_init(|| Regex::new(r"~~(.+?)~~").unwrap());
    strike_re
        .replace_all(&text, |caps: &regex::Captures| caps[1].to_string())
        .into_owned()
}

// ── tables (hermes convert_table_to_bullets, IRC-flavored) ──────────

fn table_sep_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*){1,}\|?\s*$").unwrap())
}

fn table_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r".*\|.*").unwrap())
}

fn split_table_row(row: &str) -> Vec<String> {
    let r = row.trim();
    let r = r.strip_prefix('|').unwrap_or(r);
    let r = r.strip_suffix('|').unwrap_or(r);
    r.split('|').map(|c| c.trim().to_string()).collect()
}

fn render_table_irc(block: &[String]) -> String {
    if block.len() < 3 {
        return block.join("\n");
    }
    let headers = split_table_row(&block[0]);
    if headers.len() < 2 {
        return block.join("\n");
    }
    let first_data = split_table_row(&block[2]);
    let has_row_label = first_data.len() == headers.len() + 1;

    let mut groups = Vec::new();
    for (idx, row) in block[2..].iter().enumerate() {
        let cells = split_table_row(row);
        let (heading, mut data_cells) = if has_row_label {
            let h = cells
                .first()
                .filter(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {}", idx + 1));
            (h, cells[1..].to_vec())
        } else {
            let h = cells
                .iter()
                .find(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {}", idx + 1));
            (h, cells.clone())
        };
        if data_cells.len() < headers.len() {
            data_cells.extend(std::iter::repeat_n(
                String::new(),
                headers.len() - data_cells.len(),
            ));
        } else if data_cells.len() > headers.len() {
            data_cells.truncate(headers.len());
        }
        let mut bullets = Vec::new();
        for (header, value) in headers.iter().zip(data_cells.iter()) {
            if !has_row_label && value == header {
                continue;
            }
            bullets.push(format!("• {}: {}", header, value));
        }
        let mut lines = vec![format!("{BOLD}{}{BOLD}", heading)];
        lines.extend(bullets);
        groups.push(lines.join("\n"));
    }
    groups.join("\n\n")
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#{1,6}\s+(.+)$").unwrap())
}

fn bullet_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*+]\s+(.+)$").unwrap())
}

fn task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*+]\s+\[([ xX])\]\s+(.+)$").unwrap())
}

fn quote_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^>\s+(.+)$").unwrap())
}

fn hr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(-{3,}|\*{3,}|_{3,})$").unwrap())
}

// ── plain-text strip (FormatMode::Plain) ─────────────────────────────

/// Strip markdown to clean plain text (no IRC codes, no markers).
pub fn strip_markdown(content: &str) -> String {
    static FENCE_STRIP: OnceLock<Regex> = OnceLock::new();
    let fence = FENCE_STRIP.get_or_init(|| Regex::new(r"(?s)```[^\n]*\n([\s\S]*?)```").unwrap());
    let text = fence
        .replace_all(content, |caps: &regex::Captures| caps[1].to_string())
        .into_owned();
    static INLINE_STRIP: OnceLock<Regex> = OnceLock::new();
    let inline = INLINE_STRIP.get_or_init(|| Regex::new(r"`([^`\n]+)`").unwrap());
    let text = inline.replace_all(&text, "$1").into_owned();
    static LINK_STRIP: OnceLock<Regex> = OnceLock::new();
    let link = LINK_STRIP.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
    let text = link.replace_all(&text, "$1 ($2)").into_owned();
    static BOLD_STRIP: OnceLock<Regex> = OnceLock::new();
    let bold = BOLD_STRIP.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    let text = bold.replace_all(&text, "$1").into_owned();
    static ITALIC_STRIP: OnceLock<Regex> = OnceLock::new();
    let italic =
        ITALIC_STRIP.get_or_init(|| Regex::new(r"(^|[^*\w])\*([^*\n]+)\*([^*\w]|$)").unwrap());
    let text = italic.replace_all(&text, "$1$2$3").into_owned();
    static HEADING_STRIP: OnceLock<Regex> = OnceLock::new();
    let heading = HEADING_STRIP.get_or_init(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
    let text = heading.replace_all(&text, "").into_owned();
    static BULLET_STRIP: OnceLock<Regex> = OnceLock::new();
    let bullet = BULLET_STRIP.get_or_init(|| Regex::new(r"(?m)^(\s*)[-*+]\s+").unwrap());
    let text = bullet.replace_all(&text, "$1• ").into_owned();
    text
}

// ── format-aware splitting (slopdrop lesson) ─────────────────────────

/// Split formatted text into chunks whose VISIBLE length ≤ max_visible.
/// Formatting codes are carried across chunk boundaries (the
/// continuation reopens active codes), so no chunk renders with
/// half-open formatting. Word-boundary splits preferred.
pub fn split_formatted(text: &str, max_visible: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    for line in text.split('\n') {
        if visible_len(line) <= max_visible {
            chunks.push(line.to_string());
            continue;
        }
        // Walk the line, tracking formatting state and visible bytes.
        let mut cur = String::new();
        let mut cur_visible = 0usize;
        let mut state = FmtState::default();
        let mut last_space_byte: Option<usize> = None; // split candidate
        let mut last_space_state = FmtState::default();
        let mut last_space_visible = 0usize;

        let mut chars = line.char_indices().peekable();
        while let Some((byte_idx, c)) = chars.next() {
            if c == COLOR {
                cur.push(c);
                // copy the color args along with the code
                while let Some(&(ni, nc)) = chars.peek() {
                    if nc.is_ascii_digit() || nc == ',' {
                        cur.push(nc);
                        let _ = ni;
                        chars.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
            if is_format_code(c) {
                cur.push(c);
                match c {
                    '\x02' => state.bold = !state.bold,
                    '\x1D' => state.italic = !state.italic,
                    '\x11' => state.monospace = !state.monospace,
                    '\x1F' => state.underline = !state.underline,
                    '\x16' => state.reverse = !state.reverse,
                    '\x0F' => state = FmtState::default(),
                    _ => {}
                }
                continue;
            }

            // Visible char.
            let len = c.len_utf8();
            if cur_visible + len > max_visible {
                // Flush at the last word boundary when possible.
                if let Some(sp) = last_space_byte {
                    let head = cur[..sp].to_string();
                    let tail_state = last_space_state;
                    let leftover = cur[sp + 1..].to_string();
                    if !head.is_empty() {
                        chunks.push(head);
                    }
                    cur = tail_state.reopen_codes() + &leftover;
                    cur_visible -= last_space_visible;
                    state = tail_state;
                } else {
                    // No boundary — hard cut, reopen codes.
                    chunks.push(std::mem::take(&mut cur));
                    cur = state.reopen_codes();
                    cur_visible = 0;
                }
                last_space_byte = None;
            }
            if c == ' ' {
                last_space_byte = Some(cur.len());
                last_space_state = state;
                last_space_visible = cur_visible;
            }
            cur.push(c);
            cur_visible += len;
            let _ = byte_idx;
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hostmask is the pasteable half of `/acl explain`'s answer, and
    /// it is full of markdown metacharacters: `*!*@host` is an emphasis
    /// run, so it went out as `\x1d!\x1d@host` and an operator copying
    /// the mask copied italic codes into their config. The ACL output
    /// backticks every identity for exactly this reason; this pins that
    /// the protection is real.
    #[test]
    fn a_hostmask_in_backticks_survives_byte_exact() {
        for mask in [
            "*!*@*network-default-masking",
            "*!*@*",
            "re:^vau.*!*@*",
            "jimmy!_asshole_@444.example",
        ] {
            let out = markdown_to_irc(&format!("`{mask}` → guest"));
            assert!(
                out.contains(mask),
                "{mask:?} was mangled to {out:?} — a mask must paste back"
            );
        }
        // …and without the backticks it is NOT safe, which is the
        // reason the backticks are there. If this ever stops holding,
        // the ACL output can drop them.
        let bare = markdown_to_irc("*!*@host → guest");
        assert!(
            !bare.contains("*!*@host"),
            "emphasis stopped eating a bare mask ({bare:?}) — the backticks in \
             acl.rs::describe are now belt-and-braces rather than load-bearing"
        );
    }

    #[test]
    fn code_block_byte_exact() {
        // THE Tcl bug: underscores and asterisks in code must survive.
        let md = "```\n{{_} {. \"val: $_\"}}\nrand()*26\n```";
        let out = markdown_to_irc(md);
        assert!(
            out.contains("{{_} {. \"val: $_\"}}"),
            "underscore eaten: {out:?}"
        );
        assert!(out.contains("rand()*26"), "asterisk eaten: {out:?}");
        // and it's wrapped monospace
        assert!(out.contains('\x11'), "no monospace code: {out:?}");
    }

    #[test]
    fn inline_code_byte_exact() {
        let out = markdown_to_irc("run `x_y[0]*2` now");
        assert!(out.contains("x_y[0]*2"), "got: {out:?}");
        assert!(out.contains('\x11'));
    }

    #[test]
    fn bold_and_heading() {
        let out = markdown_to_irc("## Title\n\nsome **bold** text");
        assert!(out.contains("\x02Title\x02"), "heading: {out:?}");
        assert!(out.contains("\x02bold\x02"), "bold: {out:?}");
    }

    #[test]
    fn italic_variants() {
        let out = markdown_to_irc("an *italic* word and _another_");
        assert!(out.contains("\x1Ditalic\x1D"), "star italic: {out:?}");
        assert!(out.contains("\x1Danother\x1D"), "under italic: {out:?}");
    }

    #[test]
    fn snake_case_not_italicized() {
        // The _x_ rule must NOT fire inside identifiers.
        let out = markdown_to_irc("use my_var_name here");
        assert!(!out.contains('\x1D'), "identifier italicized: {out:?}");
        assert!(out.contains("my_var_name"));
    }

    #[test]
    fn link_rendering() {
        let out = markdown_to_irc("see [the docs](https://example.com/x)");
        assert!(
            out.contains("the docs (https://example.com/x)"),
            "got: {out:?}"
        );
    }

    #[test]
    fn bullets_and_tasks() {
        let out = markdown_to_irc("- item one\n- [x] done\n- [ ] todo");
        assert!(out.contains("• item one"), "bullet: {out:?}");
        assert!(out.contains("☑ done"), "task done: {out:?}");
        assert!(out.contains("☐ todo"), "task todo: {out:?}");
    }

    #[test]
    fn quote_italic() {
        let out = markdown_to_irc("> thinking");
        assert!(out.contains("\x1D> thinking\x1D"), "got: {out:?}");
    }

    #[test]
    fn table_to_bullets() {
        let md = "| Name | Age |\n|---|---|\n| Ann | 30 |";
        let out = markdown_to_irc(md);
        assert!(out.contains("\x02Ann\x02"), "heading: {out:?}");
        assert!(out.contains("• Name: Ann"), "bullets: {out:?}");
        assert!(out.contains("• Age: 30"), "bullets: {out:?}");
    }

    #[test]
    fn unclosed_fence_survives() {
        let md = "```rust\nlet x = 1;";
        let out = markdown_to_irc(md);
        assert!(out.contains("let x = 1;"), "got: {out:?}");
    }

    #[test]
    fn strip_markdown_clean() {
        let md = "## T\n**bold** and `code` and [x](http://y.z)";
        let out = strip_markdown(md);
        assert!(!out.contains('*'));
        assert!(!out.contains('#'));
        assert!(!out.contains('`'));
        assert!(out.contains("bold"));
        assert!(out.contains("code"));
        assert!(out.contains("x (http://y.z)"));
    }

    #[test]
    fn visible_len_ignores_codes() {
        assert_eq!(visible_len("\x02hello\x02"), 5);
        assert_eq!(visible_len("\x034,2colored\x0F"), 7);
        assert_eq!(visible_len("plain"), 5);
        assert_eq!(visible_len("héllo"), 6); // é = 2 bytes
    }

    #[test]
    fn strip_irc_codes_roundtrip() {
        assert_eq!(strip_irc_codes("\x02hi\x02"), "hi");
        assert_eq!(strip_irc_codes("\x034red\x0F"), "red");
        assert_eq!(strip_irc_codes("\x1Dit\x11mono\x1D"), "itmono");
    }

    #[test]
    fn split_formatted_word_boundary() {
        let text = "\x02hello world foo bar\x02";
        let chunks = split_formatted(text, 11);
        assert!(chunks.len() >= 2, "chunks: {chunks:?}");
        for c in &chunks {
            assert!(visible_len(c) <= 11, "over-limit chunk: {c:?}");
        }
        // continuation reopens bold
        assert!(chunks[1].starts_with('\x02'), "no reopen: {chunks:?}");
        // content preserved (collect each stripped chunk, then join)
        let stripped: Vec<String> = chunks.iter().map(|c| strip_irc_codes(c)).collect();
        let joined: String = stripped.concat();
        assert!(joined.contains("hello"));
        assert!(joined.contains("world"));
        assert!(joined.contains("foo"));
        assert!(joined.contains("bar"));
    }

    #[test]
    fn split_formatted_hard_cut_long_word() {
        let text = "x".repeat(100);
        let chunks = split_formatted(&text, 30);
        assert!(chunks.iter().all(|c| visible_len(c) <= 30));
        let total: usize = chunks.iter().map(|c| visible_len(c)).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn split_formatted_preserves_short_lines() {
        let text = "short line\nanother";
        let chunks = split_formatted(text, 100);
        assert_eq!(chunks, vec!["short line", "another"]);
    }

    #[test]
    fn crlf_nul_stripped_in_code() {
        // protocol safety: even byte-exact code loses line-breakers
        let out = markdown_to_irc("```\na\rb\0c\n```");
        assert!(out.contains("abc"), "got: {out:?}");
    }

    /// The quota report has to survive IRC, which is the surface with
    /// the hard constraint: `markdown_to_irc` rewrites what it
    /// recognises, and `split_formatted` re-wraps anything wider than
    /// the ISUPPORT line length. `/context`'s aligned 30-column table is
    /// unreadable there for exactly that reason, which is why
    /// `format_quota_report` keeps its rows short — this pins that the
    /// rows really are short, in the crate that does the splitting.
    ///
    /// The sample is the live report's shape, verbatim — including the
    /// kimi section and the four-plan banner (2026-09-20). The personal
    /// rows went MONTHLY on 2026-09-23 (see `tokenplan::WINDOW_FAMILIES`)
    /// and the sample follows the wire.
    #[test]
    fn a_quota_report_is_one_irc_line_per_row() {
        let report = "\
plan quota · read just now · cache ttl 5m

team
  credits   111,650 of 250,000 credits remaining — 55.3% used
  cycle     2026-09-03 09:00 UTC → 2026-10-03 09:00 UTC · 17d left
  (plus 1 released seat with no quota of their own)
  seats     1 of 1 member(s) seated
  spend     2026-09 USD 253.93 (206.45 pretax + 47.48 tax)

personal
  month     0.5% used — resets 2026-10-03 16:00 UTC · 10d left
  free tier qwen3.8-flash 535,626 of 1,000,000 — expires 2026-11-24

zai ← active
  plan      pro
  5-hour    1.0% used — resets 2026-09-15 15:48 UTC · elapsed
  weekly    60.0% used — resets 2026-09-20 20:28 UTC · 4h left
  tools     69 of 1,000 calls — 6.9% used
  by tool   search-prime 54 · web-reader 15 · zread 0

kimi
  5-hour    100.0% used — resets 2026-09-20 15:22 UTC · 1m left
  month     5.5% used — resets 2026-10-20 00:00 UTC · 29d left
  month · code   not reported by the API — resets 2026-10-20 00:00 UTC · 29d left";

        let converted = markdown_to_irc(report);

        // 400 is a conservative stand-in for a real ISUPPORT budget
        // (512 minus the prefix a server prepends).
        for chunk in split_formatted(&converted, 400) {
            assert!(
                visible_len(&chunk) <= 400,
                "a chunk exceeded the line budget: {chunk:?}"
            );
        }

        // Every row must survive as its OWN line: a row that got
        // re-wrapped is a row that reads as two half-facts in a
        // channel.
        let lines: Vec<&str> = report.lines().filter(|l| !l.trim().is_empty()).collect();
        // Blank chunks are what `privmsg_paced` skips rather than
        // sending, so they are not lines anybody sees.
        let chunks: Vec<String> = split_formatted(&converted, 400)
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .collect();
        assert_eq!(
            chunks.len(),
            lines.len(),
            "each report row should be one IRC line; got {} chunks for {} rows:\n{chunks:#?}",
            chunks.len(),
            lines.len()
        );

        // And the figures must come through intact — no markdown
        // construct in the report may eat a digit or a separator.
        let joined = chunks.join("\n");
        for fact in [
            "111,650 of 250,000",
            "55.3% used",
            "not reported by the API",
            // The personal month row — the live shape since
            // 2026-09-23, when the plan moved off 5-hour/weekly.
            "0.5% used",
            "2026-10-03 16:00 UTC",
            // The kimi rows, added 2026-09-20 — the fourth pool's
            // widest label is the one at risk of wrapping.
            "100.0% used",
            "month · code",
            "2026-10-20 00:00 UTC",
        ] {
            assert!(joined.contains(fact), "{fact:?} did not survive: {joined}");
        }
    }
}
