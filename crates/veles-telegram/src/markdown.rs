//! Telegram MarkdownV2 formatting — Rust port of hermes'
//! `format_message` (plugins/platforms/telegram/adapter.py:7620).
//!
//! Pipeline (order matters — identical to hermes):
//!   0. GFM pipe tables → bold-heading + bullet groups
//!   1. Protect fenced code blocks (``` ... ```), escaping \ and ` inside
//!   2. Protect inline code (`...`), escaping \ inside
//!   3. Convert [text](url) links (escape text; ) and \ inside url)
//!   4. Convert headers (## Title → *Title*)
//!   5. Convert bold **x** → *x*
//!   6. Convert italic *x* → _x_ (single-line only, [^*\n])
//!   7. Convert strikethrough ~~x~~ → ~x~
//!   8. Convert spoiler ||x|| → ||x|| (protected from | escaping)
//!   9. Convert blockquotes (> x → \> escaped content)
//!  10. Escape all remaining MarkdownV2 specials
//!  11. Restore placeholders in reverse insertion order
//!  12. Safety net: escape bare ( ) { } outside code, respecting links
//!
//! Placeholder tokens (\x00PH{n}\x00) survive the final escape pass and
//! keep protected regions intact — the same trick hermes uses.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Every character MarkdownV2 requires backslash-escaped outside code.
fn escape_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([_*\[\]()~`>#+\-=|{}.!\\])").unwrap())
}

/// Escape MarkdownV2 specials with a preceding backslash.
pub fn escape_mdv2(text: &str) -> String {
    escape_re().replace_all(text, "\\$1").into_owned()
}

/// Strip MarkdownV2 escapes + formatting markers (plain-text fallback).
pub fn strip_mdv2(text: &str) -> String {
    static UNESC: OnceLock<Regex> = OnceLock::new();
    let unesc = UNESC.get_or_init(|| Regex::new(r"\\([_*\[\]()~`>#+\-=|{}.!\\])").unwrap());
    let mut cleaned = unesc.replace_all(text, "$1").into_owned();
    // **bold** → bold (before MarkdownV2 *bold* stripping)
    static BOLD: OnceLock<Regex> = OnceLock::new();
    let bold = BOLD.get_or_init(|| Regex::new(r"\*\*([^*]+)\*\*").unwrap());
    cleaned = bold.replace_all(&cleaned, "$1").into_owned();
    // *bold* (MarkdownV2) → plain
    static MDV2_BOLD: OnceLock<Regex> = OnceLock::new();
    let mdv2_bold = MDV2_BOLD.get_or_init(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
    cleaned = mdv2_bold.replace_all(&cleaned, "$1").into_owned();
    // _italic_ → plain
    static ITALIC: OnceLock<Regex> = OnceLock::new();
    let italic = ITALIC.get_or_init(|| Regex::new(r"_([^_\n]+)_").unwrap());
    cleaned = italic.replace_all(&cleaned, "$1").into_owned();
    cleaned
}

struct PhStash {
    map: HashMap<String, String>,
    n: usize,
}

impl PhStash {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            n: 0,
        }
    }
    fn put(&mut self, value: String) -> String {
        let key = format!("\x00PH{}\x00", self.n);
        self.n += 1;
        self.map.insert(key.clone(), value);
        key
    }
    /// Restore in reverse insertion order (nested placeholders resolve).
    fn restore(&self, mut text: String) -> String {
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort_by(|a, b| {
            let na = a
                .trim_matches(['\x00', 'P', 'H'])
                .parse::<usize>()
                .unwrap_or(0);
            let nb = b
                .trim_matches(['\x00', 'P', 'H'])
                .parse::<usize>()
                .unwrap_or(0);
            nb.cmp(&na)
        });
        for k in keys {
            text = text.replace(k, &self.map[k]);
        }
        text
    }
}

// ── step 0: tables ───────────────────────────────────────────────────

fn table_sep_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*){1,}\|?\s*$").unwrap())
}

/// Plausibly a table data row (hermes: non-empty + contains '|').
fn is_table_row(line: &str) -> bool {
    let stripped = line.trim();
    !stripped.is_empty() && stripped.contains('|')
}

fn split_table_row(row: &str) -> Vec<String> {
    let r = row.trim();
    let r = r.strip_prefix('|').unwrap_or(r);
    let r = r.strip_suffix('|').unwrap_or(r);
    r.split('|').map(|c| c.trim().to_string()).collect()
}

fn render_table_block(block: &[String]) -> String {
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
        let mut lines = vec![format!("**{}**", heading)];
        lines.extend(bullets);
        groups.push(lines.join("\n"));
    }
    groups.join("\n\n")
}

/// Rewrite GFM pipe tables into bold-heading + bullet groups.
/// Tables inside fenced code blocks are left alone (hermes parity).
pub fn convert_table_to_bullets(text: &str) -> String {
    if !text.contains('|') || !text.contains('-') {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if in_fence {
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if line.contains('|') && i + 1 < lines.len() && table_sep_re().is_match(lines[i + 1]) {
            let mut block = vec![line.to_string(), lines[i + 1].to_string()];
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j]) {
                block.push(lines[j].to_string());
                j += 1;
            }
            out.push(render_table_block(&block));
            i = j;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    out.join("\n")
}

// ── the main conversion ──────────────────────────────────────────────

/// Convert standard markdown to Telegram MarkdownV2.
/// See module docs for the pipeline; mirrors hermes' format_message.
pub fn format_message(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }
    let mut stash = PhStash::new();

    // 0) tables
    let mut text = convert_table_to_bullets(content);

    // 1) protect fenced code blocks
    static FENCE: OnceLock<Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| Regex::new(r"(?s)(```(?:[^\n]*\n)?[\s\S]*?```)").unwrap());
    text = fence
        .replace_all(&text, |caps: &regex::Captures| {
            let raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let open_end = raw[3..]
                .find('\n')
                .map(|p| p + 4) // offset past opening ```
                .unwrap_or(3);
            let opening = &raw[..open_end];
            let body_and_close = &raw[open_end..];
            let body = &body_and_close[..body_and_close.len().saturating_sub(3)];
            let body = body.replace('\\', "\\\\").replace('`', "\\`");
            stash.put(format!("{opening}{body}```"))
        })
        .into_owned();

    // 2) protect inline code
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let inline = INLINE.get_or_init(|| Regex::new(r"(`[^`]+`)").unwrap());
    text = inline
        .replace_all(&text, |caps: &regex::Captures| {
            let m = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            stash.put(m.replace('\\', "\\\\"))
        })
        .into_owned();

    // 3) links [text](url)
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link =
        LINK.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^()]*(?:\([^()]*\)[^()]*)*)\)").unwrap());
    text = link
        .replace_all(&text, |caps: &regex::Captures| {
            let display = escape_mdv2(&caps[1]);
            let url = caps[2].replace('\\', "\\\\").replace(')', "\\)");
            stash.put(format!("[{display}]({url})"))
        })
        .into_owned();

    // 4) headers → bold
    static HEADER: OnceLock<Regex> = OnceLock::new();
    let header = HEADER.get_or_init(|| Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap());
    text = header
        .replace_all(&text, |caps: &regex::Captures| {
            let mut inner = caps[1].trim().to_string();
            static INNER_BOLD: OnceLock<Regex> = OnceLock::new();
            let inner_bold = INNER_BOLD.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
            inner = inner_bold.replace_all(&inner, "$1").into_owned();
            stash.put(format!("*{}*", escape_mdv2(&inner)))
        })
        .into_owned();

    // 5) bold **x** → *x*
    static BOLD: OnceLock<Regex> = OnceLock::new();
    let bold = BOLD.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    text = bold
        .replace_all(&text, |caps: &regex::Captures| {
            stash.put(format!("*{}*", escape_mdv2(&caps[1])))
        })
        .into_owned();

    // 6) italic *x* → _x_ (single-line)
    static ITALIC: OnceLock<Regex> = OnceLock::new();
    let italic = ITALIC.get_or_init(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
    text = italic
        .replace_all(&text, |caps: &regex::Captures| {
            stash.put(format!("_{}_", escape_mdv2(&caps[1])))
        })
        .into_owned();

    // 7) strikethrough ~~x~~ → ~x~
    static STRIKE: OnceLock<Regex> = OnceLock::new();
    let strike = STRIKE.get_or_init(|| Regex::new(r"~~(.+?)~~").unwrap());
    text = strike
        .replace_all(&text, |caps: &regex::Captures| {
            stash.put(format!("~{}~", escape_mdv2(&caps[1])))
        })
        .into_owned();

    // 8) spoiler ||x|| → ||x||
    static SPOILER: OnceLock<Regex> = OnceLock::new();
    let spoiler = SPOILER.get_or_init(|| Regex::new(r"\|\|(.+?)\|\|").unwrap());
    text = spoiler
        .replace_all(&text, |caps: &regex::Captures| {
            stash.put(format!("||{}||", escape_mdv2(&caps[1])))
        })
        .into_owned();

    // 9) blockquotes (> x, **> expandable)
    static QUOTE: OnceLock<Regex> = OnceLock::new();
    let quote = QUOTE.get_or_init(|| Regex::new(r"(?m)^((?:\*\*)?>{1,3}) (.+)$").unwrap());
    text = quote
        .replace_all(&text, |caps: &regex::Captures| {
            let prefix = &caps[1];
            let content = &caps[2];
            if prefix.starts_with("**") && content.ends_with("||") {
                stash.put(format!(
                    "{prefix} {}||",
                    escape_mdv2(&content[..content.len() - 2])
                ))
            } else {
                stash.put(format!("{prefix} {}", escape_mdv2(content)))
            }
        })
        .into_owned();

    // 10) escape remaining specials
    text = escape_mdv2(&text);

    // 11) restore placeholders (reverse insertion order)
    text = stash.restore(text);

    // 12) safety net: escape bare ( ) { } outside code, link-aware
    text = safety_net(text);

    text
}

/// Step 12 — escape unescaped parens/braces outside code segments,
/// never breaking MarkdownV2 link syntax.
fn safety_net(text: String) -> String {
    static SPLIT: OnceLock<Regex> = OnceLock::new();
    let split = SPLIT.get_or_init(|| Regex::new(r"(?s)(```[\s\S]*?```|`[^`]+`)").unwrap());
    let mut parts = Vec::new();
    let mut last_end = 0usize;
    let mut seg_idx = 0usize; // parity with python's odd/even index
    for caps in split.captures_iter(&text) {
        let m = caps.get(1).unwrap();
        let seg = &text[last_end..m.start()];
        parts.push(escape_bare(seg, seg_idx % 2 == 1));
        parts.push(m.as_str().to_string()); // code segment untouched
        seg_idx += 2;
        last_end = m.end();
    }
    parts.push(escape_bare(&text[last_end..], seg_idx % 2 == 1));
    parts.join("")
}

fn escape_bare(seg: &str, _is_code: bool) -> String {
    // We only receive non-code segments here (code is split out), so
    // escape bare ( ) { } with link-aware skipping. The placeholder
    // dance (restore happens BEFORE this step) means restored links look
    // like [text](url) again — so the whole URL body must be skipped,
    // including its closing paren, or Telegram rejects the message.
    let chars: Vec<char> = seg.chars().collect();
    let mut out = String::with_capacity(seg.len() + 8);
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        // '(' that opens a MarkdownV2 link: preceded by ']' → skip the
        // whole link body (balanced parens, backslash-escaped chars) and
        // emit it untouched. This also consumes the link's closing ')'.
        if ch == '(' && i > 0 && chars[i - 1] == ']' {
            out.push(ch);
            i += 1;
            let mut depth = 1usize;
            while i < chars.len() && depth > 0 {
                let c = chars[i];
                if c == '\\' && i + 1 < chars.len() {
                    out.push(c);
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                }
                out.push(c);
                i += 1;
            }
            continue;
        }
        match ch {
            '(' | ')' | '{' | '}' => {
                // already escaped
                if i > 0 && chars[i - 1] == '\\' {
                    out.push(ch);
                } else {
                    out.push('\\');
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_specials() {
        assert_eq!(escape_mdv2("hello_world!"), "hello\\_world\\!");
        assert_eq!(escape_mdv2("a.b-c"), "a\\.b\\-c");
    }

    #[test]
    fn bold_converts() {
        let out = format_message("This is **bold** text");
        assert!(out.contains("*bold*"), "got: {out}");
        assert!(!out.contains("**"), "got: {out}");
    }

    #[test]
    fn italic_converts() {
        let out = format_message("some *italic* word");
        assert!(out.contains("_italic_"), "got: {out}");
    }

    #[test]
    fn header_becomes_bold() {
        let out = format_message("## Title here");
        assert!(out.contains("*Title here*"), "got: {out}");
        assert!(!out.contains('#'), "got: {out}");
    }

    #[test]
    fn code_protected() {
        // backticks and specials inside code survive untouched
        let out = format_message("run `x_y[0]` now");
        assert!(out.contains("`x_y[0]`"), "got: {out}");
        assert!(!out.contains("x\\_y"), "got: {out}");
    }

    #[test]
    fn fenced_block_protected() {
        let md = "```rust\nlet x = 1_2;\n```";
        let out = format_message(md);
        assert!(out.contains("let x = 1_2;"), "got: {out}");
        assert!(!out.contains("1\\_2"), "got: {out}");
    }

    #[test]
    fn link_converts() {
        let out = format_message("see [docs](https://example.com/x) ok");
        assert!(out.contains("[docs](https://example.com/x)"), "got: {out}");
    }

    #[test]
    fn table_to_bullets() {
        let md = "| Name | Age |\n|---|---|\n| Ann | 30 |";
        // Layer 0 only: raw markdown bold-heading + bullets
        let bullets = convert_table_to_bullets(md);
        assert!(bullets.contains("**Ann**"), "got: {bullets}");
        assert!(bullets.contains("• Name: Ann"), "got: {bullets}");
        assert!(bullets.contains("• Age: 30"), "got: {bullets}");
        // Full pipeline: the **heading** becomes MarkdownV2 *heading*
        let out = format_message(md);
        assert!(out.contains("*Ann*"), "got: {out}");
        assert!(out.contains("• Name: Ann"), "got: {out}");
        assert!(!out.contains('|'), "table pipes must be gone: {out}");
    }

    #[test]
    fn blockquote_converts() {
        let out = format_message("> quoted line");
        assert!(out.contains("> quoted line"), "got: {out}");
    }

    #[test]
    fn plain_text_escaped() {
        let out = format_message("1. price: $5.00 (wow)");
        assert!(out.contains("\\."), "got: {out}");
        assert!(out.contains("\\("), "got: {out}");
    }

    #[test]
    fn strip_roundtrip_plain() {
        let formatted = format_message("**bold** and *italic*");
        let stripped = strip_mdv2(&formatted);
        assert!(stripped.contains("bold"));
        assert!(!stripped.contains('\\'));
    }

    #[test]
    fn empty_passthrough() {
        assert_eq!(format_message(""), "");
    }
}
