//! A [`Grid`] drawn into a PNG.
//!
//! Every cell is 8×16 pixels at scale 1: its background is painted
//! (solid-block art is written as foreground = background), then the
//! Unifont glyph in the foreground colour. Bold is a one-pixel emboss,
//! italic a shear of the top rows, underline row 15, strike row 8,
//! reverse swaps the colours. A combining mark is drawn over its cell;
//! stacked marks climb (above) or sink (below) three pixels each, and may
//! spill past the cell as zalgo does on a screen.
//!
//! The picture is scaled by a whole number (crisp pixels, never a
//! smoothed resize) so it reads at Bluesky's feed width, and it is
//! written as an indexed PNG when it has 256 colours or fewer — IRC art
//! always does — so it stays far below Bluesky's 1,000,000-byte image
//! limit and is never re-encoded as a smeared JPEG.

use std::collections::HashMap;

use crate::font::{font, Glyph};
use crate::palette::Rgb;
use crate::parse::{Grid, Part, Style};

/// The page colours when a cell names none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    /// Light grey (mIRC 15) on black: how most art is looked at.
    #[default]
    Dark,
    /// Black on white: mIRC's own default.
    Light,
}

impl Theme {
    fn colours(self) -> (Rgb, Rgb) {
        match self {
            Theme::Dark => ([0xd2, 0xd2, 0xd2], [0, 0, 0]),
            Theme::Light => ([0, 0, 0], [0xff, 0xff, 0xff]),
        }
    }

    /// `dark` / `light`.
    pub fn parse(s: &str) -> Option<Theme> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub theme: Theme,
    /// A blank border this many cells wide on every side.
    pub margin_cells: usize,
    /// The width the scale aims for (the largest whole scale that stays
    /// at or under it).
    pub target_width_px: u32,
    /// Neither side may exceed this.
    pub max_side_px: u32,
    /// The largest scale.
    pub max_scale: u32,
    /// The PNG must not exceed this many bytes (Bluesky: 1,000,000).
    pub max_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            theme: Theme::Dark,
            margin_cells: 1,
            target_width_px: 1400,
            max_side_px: 2000,
            max_scale: 8,
            max_bytes: 990_000,
        }
    }
}

/// A finished picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Png {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The grid, in cells.
    pub cols: usize,
    pub rows: usize,
    pub scale: u32,
    /// Part of the input did not fit and was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Nothing visible to draw.
    Empty,
    /// Even at scale 1 the PNG is over the byte limit.
    TooLarge(usize),
    Encode(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Empty => write!(f, "nothing to draw"),
            Error::TooLarge(n) => write!(f, "the picture is {n} bytes even at scale 1"),
            Error::Encode(e) => write!(f, "png: {e}"),
        }
    }
}

impl std::error::Error for Error {}

pub const CELL_W: usize = 8;
pub const CELL_H: usize = 16;

/// The scale-1 canvas.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<Rgb>,
}

impl Canvas {
    fn set(&mut self, x: isize, y: isize, c: Rgb) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = c;
        }
    }
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: Rgb) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                self.px[yy * self.w + xx] = c;
            }
        }
    }
}

fn colours(style: &Style, theme: Theme) -> (Rgb, Rgb) {
    let (dfg, dbg) = theme.colours();
    let fg = style.fg.map(|c| c.rgb(style.bold)).unwrap_or(dfg);
    let bg = style.bg.map(|c| c.rgb(false)).unwrap_or(dbg);
    if style.reverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

fn draw_glyph(cv: &mut Canvas, g: &Glyph<'_>, x0: isize, y0: isize, fg: Rgb, style: &Style) {
    for y in 0..Glyph::HEIGHT {
        let shear = if style.italic {
            ((15 - y) / 6) as isize
        } else {
            0
        };
        for x in 0..g.width_px as usize {
            if g.bit(x, y) {
                let px = x0 + x as isize + shear;
                cv.set(px, y0 + y as isize, fg);
                if style.bold {
                    cv.set(px + 1, y0 + y as isize, fg);
                }
            }
        }
    }
}

/// The scale-1 drawing of a grid.
fn draw(grid: &Grid, opts: &Options) -> Canvas {
    let m = opts.margin_cells;
    let cols = grid.width() + 2 * m;
    let rows = grid.height() + 2 * m;
    let (_, page) = opts.theme.colours();
    let mut cv = Canvas {
        w: cols * CELL_W,
        h: rows * CELL_H,
        px: vec![page; cols * CELL_W * rows * CELL_H],
    };
    let f = font();
    let replacement = f.glyph('\u{FFFD}');
    // Backgrounds first, every cell, so a mark spilling upward is not
    // painted over by the next row's background.
    for (r, row) in grid.rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let (_, bg) = colours(&cell.style, opts.theme);
            cv.fill((c + m) * CELL_W, (r + m) * CELL_H, CELL_W, CELL_H, bg);
        }
    }
    for (r, row) in grid.rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell.part == Part::Tail {
                continue;
            }
            let (fg, _) = colours(&cell.style, opts.theme);
            let x0 = ((c + m) * CELL_W) as isize;
            let y0 = ((r + m) * CELL_H) as isize;
            let span = if cell.part == Part::Lead { 2 } else { 1 } * CELL_W;
            if cell.ch != ' ' && cell.ch != '\u{a0}' {
                if let Some(g) = f.glyph(cell.ch).or(replacement) {
                    draw_glyph(&mut cv, &g, x0, y0, fg, &cell.style);
                }
            }
            let (mut above, mut below) = (0isize, 0isize);
            for mk in &cell.marks {
                let Some(g) = f.glyph(*mk) else { continue };
                let Some((top, bottom)) = g.ink_rows() else {
                    continue;
                };
                let dy = if top + bottom < Glyph::HEIGHT {
                    above += 1;
                    -3 * (above - 1)
                } else {
                    below += 1;
                    3 * (below - 1)
                };
                draw_glyph(&mut cv, &g, x0, y0 + dy, fg, &cell.style);
            }
            if cell.style.underline {
                cv.fill(x0 as usize, y0 as usize + 15, span, 1, fg);
            }
            if cell.style.strike {
                cv.fill(x0 as usize, y0 as usize + 8, span, 1, fg);
            }
        }
    }
    cv
}

/// The scales to try, largest first: the largest whole scale that keeps
/// the width at or under the target and both sides under the limit, then
/// every smaller one (a busy picture may need a smaller scale to fit the
/// byte limit).
fn scales(w: usize, h: usize, opts: &Options) -> Vec<u32> {
    let mut s = opts.max_scale.max(1);
    while s > 1
        && (w as u32 * s > opts.target_width_px
            || w as u32 * s > opts.max_side_px
            || h as u32 * s > opts.max_side_px)
    {
        s -= 1;
    }
    (1..=s).rev().collect()
}

fn encode(cv: &Canvas, scale: u32) -> Result<Vec<u8>, Error> {
    let s = scale as usize;
    let (w, h) = (cv.w * s, cv.h * s);
    // A palette in first-seen order: deterministic bytes for a given grid.
    let mut index: HashMap<Rgb, u8> = HashMap::new();
    let mut palette: Vec<Rgb> = Vec::new();
    let mut indexed = true;
    for c in &cv.px {
        if !index.contains_key(c) {
            if palette.len() == 256 {
                indexed = false;
                break;
            }
            index.insert(*c, palette.len() as u8);
            palette.push(*c);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        let data: Vec<u8> = if indexed {
            enc.set_color(png::ColorType::Indexed);
            enc.set_palette(palette.iter().flatten().copied().collect::<Vec<u8>>());
            let mut d = Vec::with_capacity(w * h);
            for y in 0..cv.h {
                let mut line = Vec::with_capacity(w);
                for x in 0..cv.w {
                    let i = index[&cv.px[y * cv.w + x]];
                    line.extend(std::iter::repeat_n(i, s));
                }
                for _ in 0..s {
                    d.extend_from_slice(&line);
                }
            }
            d
        } else {
            enc.set_color(png::ColorType::Rgb);
            let mut d = Vec::with_capacity(w * h * 3);
            for y in 0..cv.h {
                let mut line = Vec::with_capacity(w * 3);
                for x in 0..cv.w {
                    let c = cv.px[y * cv.w + x];
                    for _ in 0..s {
                        line.extend_from_slice(&c);
                    }
                }
                for _ in 0..s {
                    d.extend_from_slice(&line);
                }
            }
            d
        };
        let mut wr = enc
            .write_header()
            .map_err(|e| Error::Encode(e.to_string()))?;
        wr.write_image_data(&data)
            .map_err(|e| Error::Encode(e.to_string()))?;
        wr.finish().map_err(|e| Error::Encode(e.to_string()))?;
    }
    Ok(out)
}

/// Draw a grid. [`Error::Empty`] when it shows nothing.
pub fn render_grid(grid: &Grid, opts: &Options) -> Result<Png, Error> {
    if grid.is_blank() {
        return Err(Error::Empty);
    }
    let cv = draw(grid, opts);
    let mut last = 0;
    for s in scales(cv.w, cv.h, opts) {
        let bytes = encode(&cv, s)?;
        if bytes.len() <= opts.max_bytes {
            return Ok(Png {
                width: (cv.w as u32) * s,
                height: (cv.h as u32) * s,
                cols: grid.width(),
                rows: grid.height(),
                scale: s,
                truncated: grid.truncated,
                bytes,
            });
        }
        last = bytes.len();
    }
    Err(Error::TooLarge(last))
}
