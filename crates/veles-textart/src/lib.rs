//! veles-textart — IRC text art as a picture (PLAN-slopdrop-art.md).
//!
//! slopdrop's art is written for an IRC client: a monospace grid, mIRC
//! colour codes, ANSI escapes, block and box characters, braille, CJK
//! that takes two cells. A Bluesky post is 300 graphemes in a
//! proportional font, so the art goes out as an image instead. This crate
//! is the whole drawing, pure (no IO, no config):
//!
//! - [`parse`] turns the text into a grid of styled cells, the way a
//!   client would show it ([`parse::parse`] has the grammar);
//! - [`render`] draws the grid with GNU Unifont into an indexed PNG
//!   scaled to read at feed width and kept under Bluesky's image limit;
//! - [`render_text`] does the same for long plain text, word-wrapped;
//! - [`words`] and [`visible`] are what a reader can READ in the picture:
//!   what the text checks judge, and the default alt text.

pub mod font;
pub mod palette;
pub mod parse;
pub mod render;

pub use parse::{Grid, Limits};
pub use render::{Error, Options, Png, Theme};

/// Draw IRC art. A drawing is cut at the grid limits, never reflowed.
pub fn render(input: &str, opts: &Options) -> Result<Png, Error> {
    render::render_grid(&parse::parse(input, Limits::default()), opts)
}

/// Draw plain (or coloured) text word-wrapped at `cols` columns: a long
/// text output as one picture.
pub fn render_text(input: &str, cols: usize, opts: &Options) -> Result<Png, Error> {
    render::render_grid(&parse::parse(input, text_limits(cols)), opts)
}

/// The text [`render_text`] shows at `cols`: wrapped and cut exactly as
/// drawn, formatting removed. What a check of the picture must read.
pub fn visible_wrapped(input: &str, cols: usize) -> String {
    parse::visible_text(&parse::parse(input, text_limits(cols)))
}

fn text_limits(cols: usize) -> Limits {
    Limits {
        max_cols: cols.clamp(20, 248),
        wrap: true,
        ..Limits::default()
    }
}

/// The text the picture shows, formatting removed, one line per row.
pub fn visible(input: &str) -> String {
    parse::visible_text(&parse::parse(input, Limits::default()))
}

/// The WORDS a reader can make out in the picture: runs of two or more
/// letters or digits, in reading order, joined by single spaces. The
/// lines of a drawing (`/__\`, `|`, `(o)`) are not words; the speech in
/// a `*say` bubble and the text inside a template are.
pub fn words(input: &str) -> String {
    let text = visible(input);
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut cur = String::new();
        for c in line.chars().chain(std::iter::once(' ')) {
            if c.is_alphanumeric() || (c == '\'' && !cur.is_empty()) {
                cur.push(c);
            } else {
                let w = cur.trim_end_matches('\'');
                if w.chars().filter(|c| c.is_alphanumeric()).count() >= 2 {
                    out.push(w.to_string());
                }
                cur.clear();
            }
        }
    }
    out.join(" ")
}

/// Does the text carry mIRC or ANSI formatting?
pub fn has_codes(input: &str) -> bool {
    input.chars().any(|c| {
        matches!(
            c,
            '\x02' | '\x03' | '\x04' | '\x0f' | '\x11' | '\x16' | '\x1b' | '\x1d' | '\x1e' | '\x1f'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live `fedoragoonsay hello world` output (stolen-treasure.tcl:
    /// `stampsay -l -s fedoragoon`), quirks included.
    pub const FEDORA: &str = "        _____,          hello world\n       /_ ___\\\\      /\n   ___|________|___\n     (--[ .]-[ .]\n     (_______O__)\n   ,\"`          \\\n  /____/         \\\n  |   |    _______\\\n  \\    \\_-`_____.__)\n   `-_ /          |\n      |      ,`   |\n      |      |    |\n      |______|____|\n      |______,_____,";

    #[test]
    fn the_words_are_what_a_reader_can_read() {
        assert_eq!(words(FEDORA), "hello world");
        assert_eq!(words("\x0304I'm\x03 a \x02cat\x02's toy"), "I'm cat's toy");
        assert_eq!(words("(o)  /__\\ |"), "");
        assert_eq!(words("漢字 ok"), "漢字 ok");
    }

    #[test]
    fn codes_are_noticed() {
        assert!(has_codes("\x0304red"));
        assert!(has_codes("\x1b[31mred"));
        assert!(!has_codes("plain\ntext ▀▄"));
    }

    #[test]
    fn a_drawing_renders_crisp_small_and_deterministic() {
        let a = render(FEDORA, &Options::default()).unwrap();
        let b = render(FEDORA, &Options::default()).unwrap();
        assert_eq!(a, b, "same input, same bytes");
        assert_eq!(a.cols, 35);
        assert_eq!(a.rows, 14);
        // (35 + 2) × 8 = 296 px at scale 1; the largest whole scale under
        // 1400 px is 4.
        assert_eq!(a.scale, 4);
        assert_eq!((a.width, a.height), (296 * 4, 16 * 16 * 4));
        assert!(a.bytes.len() < 100_000, "{} bytes", a.bytes.len());
        assert_eq!(&a.bytes[1..4], b"PNG");
        assert!(!a.truncated);
    }

    #[test]
    fn colours_backgrounds_and_themes_reach_the_pixels() {
        let dec = |p: &Png| {
            let d = png::Decoder::new(std::io::Cursor::new(p.bytes.clone()));
            let mut r = d.read_info().unwrap();
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let info = r.next_frame(&mut buf).unwrap();
            let pal = r.info().palette.clone().map(|p| p.to_vec());
            (info, buf, pal)
        };
        let opts = Options {
            margin_cells: 0,
            max_scale: 1,
            ..Options::default()
        };
        // A solid red block written as fg = bg (gold bigtext's trick).
        let p = render("\x0304,04*", &opts).unwrap();
        let (info, buf, pal) = dec(&p);
        assert_eq!(info.color_type, png::ColorType::Indexed);
        let pal = pal.unwrap();
        assert!(
            buf.iter()
                .all(|&i| pal[i as usize * 3..i as usize * 3 + 3] == [255, 0, 0]),
            "every pixel is the red background or the red glyph"
        );
        // The page colour follows the theme where a cell names none.
        for (theme, page) in [(Theme::Dark, [0, 0, 0]), (Theme::Light, [255, 255, 255])] {
            let p = render(".", &Options { theme, ..opts }).unwrap();
            let (_, buf, pal) = dec(&p);
            let pal = pal.unwrap();
            let i = buf[0] as usize;
            assert_eq!(pal[i * 3..i * 3 + 3], page, "{theme:?}");
        }
    }

    #[test]
    fn nothing_visible_is_an_error_and_a_wall_is_cut() {
        assert_eq!(render("  \n ", &Options::default()), Err(Error::Empty));
        let wall = "x".repeat(400);
        let p = render(&wall, &Options::default()).unwrap();
        assert!(p.truncated);
        assert_eq!(p.cols, 248);
        assert!(p.width <= 2000 && p.height <= 2000);
    }

    #[test]
    fn noise_still_fits_the_byte_limit() {
        // Truecolour noise: more than 256 colours, the worst case.
        let mut s = String::new();
        for r in 0..120u32 {
            for c in 0..240u32 {
                let v = (r * 7919 + c * 104729) % 16_777_213;
                s.push_str(&format!(
                    "\x1b[48;2;{};{};{}m{}",
                    v >> 16 & 255,
                    v >> 8 & 255,
                    v & 255,
                    char::from_u32(0x2580 + (v % 32)).unwrap()
                ));
            }
            s.push('\n');
        }
        let p = render(&s, &Options::default()).unwrap();
        assert!(
            p.bytes.len() <= Options::default().max_bytes,
            "{}",
            p.bytes.len()
        );
    }

    #[test]
    fn long_text_wraps_into_a_picture() {
        let text = "word ".repeat(200);
        let p = render_text(&text, 60, &Options::default()).unwrap();
        assert!(p.cols <= 60);
        assert!(!p.truncated);
        assert!(p.rows >= 16);
    }
}
