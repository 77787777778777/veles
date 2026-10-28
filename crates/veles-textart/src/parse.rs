//! IRC text → a grid of styled cells.
//!
//! The grammar is the one IRC clients draw:
//! - mIRC: `\x02` bold, `\x1d` italic, `\x1f` underline, `\x1e` strike,
//!   `\x16` reverse (toggles), `\x0f` reset, `\x11` monospace (ignored:
//!   every cell is monospace here), `\x03` colour: up to two digits of
//!   foreground and, only when a digit follows the comma, up to two of
//!   background (`\x034,` keeps its comma, `\x0304,123` keeps the `3`); a
//!   bare `\x03` drops both colours. `\x04` the same with six hex digits.
//! - ANSI: `ESC [ … m` (SGR): bold, italic, underline, reverse, strike and
//!   their offs, 8/16 colours, `38;5;n`/`48;5;n`, `38;2;r;g;b`. Any other
//!   escape sequence is dropped. Bold brightens an ANSI colour 0–7, as the
//!   BBS art this comes from expects.
//! - `\n` and a form feed (`bsky::newpost`'s post boundary) end a row; a
//!   tab moves to the next multiple of 8; `\r` and other control
//!   characters are dropped.
//! - Width is the terminal's (`unicode-width`): CJK, fullwidth forms and
//!   U+3000 take two cells. A zero-width combining mark is drawn over the
//!   cell before it (zalgo keeps its marks); format characters (joiners,
//!   variation selectors, BOM) draw nothing.
//!
//! Text is never "fixed": a drawing is reproduced cell for cell, padding
//! quirks and all.

use unicode_width::UnicodeWidthChar;

use crate::palette::{ansi256, mirc, Rgb};

/// A colour as written: mIRC and hex colours are resolved here, an ANSI
/// colour keeps its index so bold can brighten it at draw time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colour {
    Rgb(Rgb),
    Ansi(u8),
}

impl Colour {
    /// The sRGB value, brightened when bold and an ANSI 0–7.
    pub fn rgb(self, bold: bool) -> Rgb {
        match self {
            Colour::Rgb(c) => c,
            Colour::Ansi(n) if bold && n < 8 => ansi256(n + 8),
            Colour::Ansi(n) => ansi256(n),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Colour>,
    pub bg: Option<Colour>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub reverse: bool,
}

/// Which part of a character a cell holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// A one-cell character.
    Whole,
    /// The left half of a two-cell character: the glyph is drawn here.
    Lead,
    /// The right half: background only.
    Tail,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    pub part: Part,
    /// Combining marks drawn over this cell, in order (capped).
    pub marks: Vec<char>,
}

/// The parsed picture: rows of cells, ragged on the right.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grid {
    pub rows: Vec<Vec<Cell>>,
    /// Something was cut to fit [`Limits`].
    pub truncated: bool,
}

impl Grid {
    /// The widest row, in cells.
    pub fn width(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
    pub fn height(&self) -> usize {
        self.rows.len()
    }
    /// Nothing visible: no cells, or only blanks with no background.
    pub fn is_blank(&self) -> bool {
        self.rows.iter().flatten().all(|c| {
            (c.ch == ' ' || c.ch == '\u{a0}' || c.part == Part::Tail)
                && c.marks.is_empty()
                && c.style.bg.is_none()
                && !c.style.reverse
                && !c.style.underline
                && !c.style.strike
        })
    }
}

/// How big a grid may get.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_cols: usize,
    pub max_rows: usize,
    /// Word-wrap at `max_cols` instead of cutting the row (long text as a
    /// picture). `false` for art: a drawing is cut, never reflowed.
    pub wrap: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_cols: 248,
            max_rows: 123,
            wrap: false,
        }
    }
}

/// At most this many marks stack on one cell (zalgo writes dozens).
pub const MAX_MARKS: usize = 16;

/// Format characters: zero width and nothing to draw.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}'
        | '\u{17B4}' | '\u{17B5}' | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}'
        | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0000}'..='\u{E0FFF}')
}

struct Builder {
    grid: Grid,
    row: Vec<Cell>,
    style: Style,
    limits: Limits,
    done: bool,
}

impl Builder {
    fn end_row(&mut self) {
        if self.done {
            return;
        }
        if self.grid.rows.len() >= self.limits.max_rows {
            self.grid.truncated = true;
            self.done = true;
            self.row.clear();
            return;
        }
        let row = std::mem::take(&mut self.row);
        self.grid.rows.push(row);
    }

    /// Wrap the current row: move the last word (after the last blank) to
    /// a new row, or break hard when the row is one word.
    fn wrap_row(&mut self) {
        let cut = self
            .row
            .iter()
            .rposition(|c| c.ch == ' ' && c.part == Part::Whole)
            .filter(|&i| i > 0 && i + 1 < self.row.len())
            .map(|i| i + 1);
        match cut {
            Some(i) => {
                let rest = self.row.split_off(i);
                // The blank the break replaced goes with the old row's end.
                while self.row.last().is_some_and(|c| c.ch == ' ') {
                    self.row.pop();
                }
                self.end_row();
                if !self.done {
                    self.row = rest;
                }
            }
            None => self.end_row(),
        }
    }

    fn push(&mut self, ch: char, width: usize) {
        if self.done {
            return;
        }
        if self.row.len() + width > self.limits.max_cols {
            if self.limits.wrap {
                self.wrap_row();
                if self.done {
                    return;
                }
                if self.row.len() + width > self.limits.max_cols {
                    self.grid.truncated = true;
                    return;
                }
            } else {
                self.grid.truncated = true;
                return;
            }
        }
        if width == 2 {
            self.row.push(Cell {
                ch,
                style: self.style,
                part: Part::Lead,
                marks: Vec::new(),
            });
            self.row.push(Cell {
                ch: ' ',
                style: self.style,
                part: Part::Tail,
                marks: Vec::new(),
            });
        } else {
            self.row.push(Cell {
                ch,
                style: self.style,
                part: Part::Whole,
                marks: Vec::new(),
            });
        }
    }

    fn mark(&mut self, ch: char) {
        if self.done {
            return;
        }
        let Some(cell) = self.row.iter_mut().rev().find(|c| c.part != Part::Tail) else {
            return;
        };
        if cell.marks.len() < MAX_MARKS {
            cell.marks.push(ch);
        }
    }
}

/// Read up to `n` decimal digits.
fn digits(s: &[char], i: &mut usize, n: usize) -> Option<u8> {
    let start = *i;
    while *i < s.len() && *i - start < n && s[*i].is_ascii_digit() {
        *i += 1;
    }
    (*i > start).then(|| {
        s[start..*i]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or(99)
    })
}

/// Read exactly six hex digits.
fn hex6(s: &[char], i: &mut usize) -> Option<Rgb> {
    if *i + 6 > s.len() || !s[*i..*i + 6].iter().all(char::is_ascii_hexdigit) {
        return None;
    }
    let h: String = s[*i..*i + 6].iter().collect();
    *i += 6;
    let v = u32::from_str_radix(&h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn sgr(style: &mut Style, params: &str) {
    let ps: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let mut k = 0;
    while k < ps.len() {
        let p = ps[k];
        match p {
            0 => *style = Style::default(),
            1 => style.bold = true,
            3 => style.italic = true,
            4 | 21 => style.underline = true,
            7 => style.reverse = true,
            9 => style.strike = true,
            22 => style.bold = false,
            23 => style.italic = false,
            24 => style.underline = false,
            27 => style.reverse = false,
            29 => style.strike = false,
            30..=37 => style.fg = Some(Colour::Ansi((p - 30) as u8)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(Colour::Ansi((p - 40) as u8)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(Colour::Ansi((p - 90 + 8) as u8)),
            100..=107 => style.bg = Some(Colour::Ansi((p - 100 + 8) as u8)),
            38 | 48 => {
                let c = match ps.get(k + 1) {
                    Some(5) => {
                        let c = ps.get(k + 2).map(|&n| Colour::Ansi(n.min(255) as u8));
                        k += 2;
                        c
                    }
                    Some(2) => {
                        let ch = |j: usize| ps.get(k + j).map(|&v| v.min(255) as u8);
                        let c = match (ch(2), ch(3), ch(4)) {
                            (Some(r), Some(g), Some(b)) => Some(Colour::Rgb([r, g, b])),
                            _ => None,
                        };
                        k += 4;
                        c
                    }
                    _ => None,
                };
                if let Some(c) = c {
                    if p == 38 {
                        style.fg = Some(c);
                    } else {
                        style.bg = Some(c);
                    }
                }
            }
            _ => {}
        }
        k += 1;
    }
}

/// Parse IRC text into a [`Grid`].
pub fn parse(input: &str, limits: Limits) -> Grid {
    let s: Vec<char> = input.chars().collect();
    let mut b = Builder {
        grid: Grid::default(),
        row: Vec::new(),
        style: Style::default(),
        limits,
        done: false,
    };
    let mut i = 0;
    while i < s.len() && !b.done {
        let c = s[i];
        i += 1;
        match c {
            '\x02' => b.style.bold = !b.style.bold,
            '\x1d' => b.style.italic = !b.style.italic,
            '\x1f' => b.style.underline = !b.style.underline,
            '\x1e' => b.style.strike = !b.style.strike,
            '\x16' => b.style.reverse = !b.style.reverse,
            '\x0f' => b.style = Style::default(),
            '\x11' => {}
            '\x03' => {
                let fg = digits(&s, &mut i, 2);
                let mut bg = None;
                if fg.is_some()
                    && s.get(i) == Some(&',')
                    && s.get(i + 1).is_some_and(char::is_ascii_digit)
                {
                    i += 1;
                    bg = digits(&s, &mut i, 2);
                }
                match fg {
                    None => {
                        b.style.fg = None;
                        b.style.bg = None;
                    }
                    Some(f) => {
                        b.style.fg = mirc(f).map(Colour::Rgb);
                        if let Some(g) = bg {
                            b.style.bg = mirc(g).map(Colour::Rgb);
                        }
                    }
                }
            }
            '\x04' => {
                let fg = hex6(&s, &mut i);
                let mut bg = None;
                if fg.is_some() && s.get(i) == Some(&',') {
                    let mut j = i + 1;
                    if let Some(c) = hex6(&s, &mut j) {
                        bg = Some(c);
                        i = j;
                    }
                }
                match fg {
                    None => {
                        b.style.fg = None;
                        b.style.bg = None;
                    }
                    Some(f) => {
                        b.style.fg = Some(Colour::Rgb(f));
                        if let Some(g) = bg {
                            b.style.bg = Some(Colour::Rgb(g));
                        }
                    }
                }
            }
            '\x1b' => {
                if s.get(i) == Some(&'[') {
                    let start = i + 1;
                    let mut j = start;
                    while j < s.len() && !('\x40'..='\x7e').contains(&s[j]) {
                        j += 1;
                    }
                    if j < s.len() {
                        if s[j] == 'm' {
                            let params: String = s[start..j].iter().collect();
                            sgr(&mut b.style, &params);
                        }
                        i = j + 1;
                    } else {
                        i = s.len();
                    }
                }
            }
            '\n' | '\u{c}' => b.end_row(),
            '\t' => {
                let n = 8 - (b.row.len() % 8);
                for _ in 0..n {
                    b.push(' ', 1);
                }
            }
            c if c.is_control() => {}
            c if invisible(c) => {}
            c => match c.width() {
                Some(0) => b.mark(c),
                Some(w) => b.push(c, w.min(2)),
                None => {}
            },
        }
    }
    if !b.done && (!b.row.is_empty() || b.grid.rows.is_empty()) {
        b.end_row();
    }
    // Trailing empty rows carry nothing.
    while b.grid.rows.last().is_some_and(Vec::is_empty) && b.grid.rows.len() > 1 {
        b.grid.rows.pop();
    }
    b.grid
}

/// The text a grid shows: each row's characters (marks dropped, a wide
/// character once), trailing blanks trimmed, rows joined by newlines.
pub fn visible_text(g: &Grid) -> String {
    g.rows
        .iter()
        .map(|r| {
            let s: String = r
                .iter()
                .filter(|c| c.part != Part::Tail)
                .map(|c| if c.ch == '\u{a0}' { ' ' } else { c.ch })
                .collect();
            s.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Grid {
        parse(s, Limits::default())
    }
    fn row(g: &Grid, r: usize) -> String {
        g.rows[r]
            .iter()
            .filter(|c| c.part != Part::Tail)
            .map(|c| c.ch)
            .collect()
    }

    #[test]
    fn mirc_colour_follows_the_client_grammar() {
        let g = p("\x034,12ab\x03c\x0304,d\x0304,123e");
        let cells = &g.rows[0];
        assert_eq!(row(&g, 0), "abc,d3e");
        assert_eq!(cells[0].style.fg, Some(Colour::Rgb([255, 0, 0])));
        assert_eq!(cells[0].style.bg, Some(Colour::Rgb([0, 0, 0xfc])));
        assert_eq!(cells[2].style, Style::default(), "a bare \\x03 drops both");
        assert_eq!(cells[3].ch, ',', "\\x0304, keeps its comma");
        assert_eq!(cells[3].style.bg, None);
        assert_eq!(
            cells[5].ch, '3',
            "two digits of background, the third is text"
        );
        assert_eq!(cells[5].style.bg, Some(Colour::Rgb(mirc(12).unwrap())));
        // Extended and default colours.
        let g = p("\x0352x\x0399y");
        assert_eq!(g.rows[0][0].style.fg, Some(Colour::Rgb([255, 0, 0])));
        assert_eq!(g.rows[0][1].style.fg, None, "99 is the default");
        // Hex colour.
        let g = p("\x04ff8000,000080h");
        assert_eq!(g.rows[0][0].style.fg, Some(Colour::Rgb([255, 128, 0])));
        assert_eq!(g.rows[0][0].style.bg, Some(Colour::Rgb([0, 0, 128])));
    }

    #[test]
    fn toggles_and_reset() {
        let g = p("\x02b\x1di\x1fu\x1es\x16r\x0fn\x02\x02x");
        let st: Vec<Style> = g.rows[0].iter().map(|c| c.style).collect();
        assert!(st[0].bold);
        assert!(st[1].bold && st[1].italic);
        assert!(st[4].reverse && st[4].strike && st[4].underline);
        assert_eq!(st[5], Style::default(), "\\x0f resets");
        assert_eq!(st[6], Style::default(), "bold twice is off");
    }

    #[test]
    fn ansi_sgr_and_other_escapes() {
        let g = p("\x1b[1;31mR\x1b[0m\x1b[38;5;196mX\x1b[48;2;1;2;3mY\x1b[2Jz\x1b[94mB");
        assert_eq!(row(&g, 0), "RXYzB", "the clear-screen sequence is dropped");
        let c = &g.rows[0];
        assert_eq!(c[0].style.fg, Some(Colour::Ansi(1)));
        assert_eq!(
            c[0].style.fg.unwrap().rgb(true),
            ansi256(9),
            "bold brightens"
        );
        assert_eq!(c[1].style.fg, Some(Colour::Ansi(196)));
        assert_eq!(c[2].style.bg, Some(Colour::Rgb([1, 2, 3])));
        assert_eq!(c[4].style.fg, Some(Colour::Ansi(12)));
    }

    #[test]
    fn rows_tabs_controls_and_widths() {
        let g = p("a\tb\r\nc\u{c}d\x07e\n");
        assert_eq!(
            g.height(),
            3,
            "a form feed ends a row, the trailing newline adds none"
        );
        assert_eq!(row(&g, 0), "a       b");
        assert_eq!(row(&g, 2), "de", "the bell is dropped");
        let g = p("漢\u{3000}x");
        assert_eq!(g.width(), 5, "CJK and U+3000 take two cells");
        assert_eq!(g.rows[0][1].part, Part::Tail);
        let g = p("e\u{301}\u{302}\u{200d}\u{fe0f}");
        assert_eq!(g.width(), 1);
        assert_eq!(
            g.rows[0][0].marks,
            vec!['\u{301}', '\u{302}'],
            "joiners draw nothing"
        );
    }

    #[test]
    fn limits_cut_art_and_wrap_text() {
        let lim = Limits {
            max_cols: 4,
            max_rows: 2,
            wrap: false,
        };
        let g = parse("abcdef\n1\n2", lim);
        assert!(g.truncated);
        assert_eq!(g.height(), 2);
        assert_eq!(row(&g, 0), "abcd");
        let lim = Limits {
            max_cols: 10,
            max_rows: 10,
            wrap: true,
        };
        let g = parse("the quick brown fox jumps", lim);
        assert_eq!(visible_text(&g), "the quick\nbrown fox\njumps");
        assert!(!g.truncated);
        let g = parse("abcdefghijklmnop", lim);
        assert_eq!(
            visible_text(&g),
            "abcdefghij\nklmnop",
            "one word breaks hard"
        );
    }

    #[test]
    fn blank_and_visible() {
        assert!(p("").is_blank());
        assert!(p("   \n  ").is_blank());
        assert!(
            !p("\x0301,01  ").is_blank(),
            "a background block is visible"
        );
        assert_eq!(visible_text(&p("\x0304a\u{a0}b  \nc")), "a b\nc");
    }
}
