//! Pure text/post helpers a public-feed plugin needs, with no core
//! dependency (Phase 3, D2: "the pure helpers"). The gateway's slopdrop
//! safety layer shapes the text it posts; a process plugin that publishes
//! or reposts on the platform's behalf needs the SAME shaping, or the
//! output check would judge one text and the post carry another. These
//! are copied from `veles_core::slopdrop_safety`/`media` and stay byte-
//! identical; Phase 5 converges core onto them.

/// Where the rendered page's URL belongs in a public `/tcl` post: the
/// gateway's safety layer leaves this marker, the plugin substitutes the
/// real link once the upload answers a CID.
pub const URL_PLACEHOLDER: &str = "\u{0}TCL_OUT_URL\u{0}";

/// How much of a `safe` result may go inline on Bluesky, in CHARACTERS.
/// Two ~300-grapheme posts is ~600; the inline head is held under that so
/// the truncation note and the link never push the SECOND post over.
pub const INLINE_MAX_CHARS: usize = 560;

/// The first chunk of an elided result, leaving room for the
/// "…(truncated)" note and the URL inside one 300-grapheme post.
pub const ELISION_HEAD_CHARS: usize = 260;

/// Split `MEDIA:` markers out of a reply body (the hermes convention for
/// attaching files, `MEDIA:/absolute/path` one per line). Returns
/// (paths, body-without-marker-lines); the marker line is consumed.
pub fn extract_media(text: &str) -> (Vec<String>, String) {
    let mut paths = Vec::new();
    let mut kept = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("MEDIA:") {
            let path = rest.trim();
            if !path.is_empty() {
                paths.push(path.to_string());
            }
        } else {
            kept.push(line);
        }
    }
    (paths, kept.join("\n"))
}

/// Image-ish extension (renderable inline where the surface supports it).
pub fn is_image(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// The text a public post carries for one output, and whether it is the
/// elided head of a longer one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicText {
    pub body: String,
    pub elided: bool,
}

/// Drop leading and trailing BLANK lines, keeping every interior line —
/// and the indentation of the first and last content lines. Unlike
/// `str::trim`, this only eats whole lines that are empty or all-
/// whitespace, never the spaces that position an ASCII drawing.
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
    lines[start..end].join("\n")
}

/// Remove mIRC formatting codes and ANSI CSI sequences, keeping the text.
/// `\x03` takes up to two digits of foreground and, only when a digit
/// follows the comma, up to two of background; the ANSI half is
/// `ESC [ params intermediates final`. Any OTHER control byte is left in
/// place for the output check's art gate.
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
                while chars.peek().is_some_and(|c| ('\x20'..='\x3f').contains(c)) {
                    chars.next();
                }
                if chars.peek().is_some_and(|c| ('\x40'..='\x7e').contains(c)) {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// What the public post would carry for `output`: formatting codes
/// stripped, blank lines trimmed, and cut to the [`ELISION_HEAD_CHARS`]
/// head when over [`INLINE_MAX_CHARS`]. ONE function for the output check
/// and the post, so they always judge the very text that lands.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_markers_split_out() {
        let (paths, body) = extract_media("here\nMEDIA:/tmp/a.png\nMEDIA:/tmp/b file.jpg\ndone");
        assert_eq!(paths, vec!["/tmp/a.png", "/tmp/b file.jpg"]);
        assert_eq!(body, "here\ndone");
    }

    #[test]
    fn image_extensions() {
        assert!(is_image("/x/y.PNG"));
        assert!(is_image("a.webp"));
        assert!(!is_image("a.txt"));
        assert!(!is_image("noext"));
    }

    #[test]
    fn blank_lines_trim_but_indentation_stays() {
        assert_eq!(trim_blank_lines("\n\n  hi\n  there\n\n"), "  hi\n  there");
        assert_eq!(trim_blank_lines("   \n \n"), "");
    }

    #[test]
    fn irc_and_ansi_codes_strip() {
        assert_eq!(strip_irc_codes("\x034,12rainbow\x0f"), "rainbow");
        assert_eq!(strip_irc_codes("plain"), "plain");
        assert_eq!(strip_irc_codes("\x1b[1;31mred\x1b[0m"), "red");
        // A lone control byte that is neither stays for the art gate.
        assert_eq!(strip_irc_codes("a\x07b"), "a\x07b");
    }

    #[test]
    fn public_text_elides_over_the_cap() {
        let short = public_text("hello");
        assert!(!short.elided && short.body == "hello");
        let long = public_text(&"x".repeat(INLINE_MAX_CHARS + 1));
        assert!(long.elided && long.body.chars().count() == ELISION_HEAD_CHARS);
    }

    #[test]
    fn inline_post_links_the_full_output() {
        let pt = public_text("hi");
        assert!(inline_post(&pt, "https://v/t/abc").contains("https://v/t/abc"));
        let long = public_text(&"y".repeat(INLINE_MAX_CHARS + 1));
        let s = inline_post(&long, "https://v/t/abc");
        assert!(s.contains("truncated") && s.contains("https://v/t/abc"));
    }
}
