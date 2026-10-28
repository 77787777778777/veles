//! Image macros (PLAN-slopdrop-art.md P6): a picture from the thread,
//! changed by a chain of ops (`bsky::magick`, `bsky::caption` in
//! slopdrop's `tcl/bsky.tcl`) or by a program in the base library's image
//! language (`magick_scheme`, [`crate::scheme`]), drawn here and attached
//! to the reply as a PNG. Anyone's: a stranger's drawing then passes the
//! same checks as their art (the words written on it, then the image
//! check), and the work is bounded per drawing.
//!
//! What may be fetched is only a picture the thread shows: a URL the
//! atproto plugin put in the post context (`embed.images`), matched
//! exactly, over HTTPS, with no redirect followed and at most
//! [`MAX_FETCH_BYTES`] read. A line can never make the gateway fetch an
//! address of its choosing.
//!
//! slopdrop records a macro as `{kind magick, src, ops, alt}`: `ops` is
//! the chain with each op's words joined by U+001F and the ops by U+001E,
//! so no Tcl list parser is needed here.

use std::time::Duration;

use image::{imageops, DynamicImage, Rgba, RgbaImage};
use serde_json::Value;

/// At most this many ops in one macro.
pub const MAX_OPS: usize = 10;
/// A source picture larger than this is not read.
pub const MAX_FETCH_BYTES: usize = 8 * 1024 * 1024;
/// No side of a working picture grows past this: a larger source is
/// scaled down first, and no op makes it larger.
pub const MAX_SIDE: u32 = 2000;
/// The atproto plugin takes a drawn PNG up to Bluesky's image limit.
pub const MAX_PNG_BYTES: usize = 1_000_000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// One step of a macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Width and height; 0 for one of them keeps the shape.
    Resize(u32, u32),
    /// Percent, 1–400.
    Scale(u32),
    /// Clockwise degrees: 90, 180 or 270.
    Rotate(u32),
    /// Upside down.
    Flip,
    /// Mirrored left to right.
    Flop,
    /// x, y, width, height; clipped to the picture.
    Crop(u32, u32, u32, u32),
    /// Width in pixels, colour.
    Border(u32, [u8; 3]),
    /// Sigma in tenths (5 = 0.5), 1–200.
    Blur(u32),
    Sharpen,
    Grayscale,
    Invert,
    /// Top and bottom text.
    Caption(String, String),
}

/// What a macro draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// `bsky::magick`: one picture of the thread and a chain of ops.
    Ops { src: String, ops: Vec<Op> },
    /// `magick_scheme`: a program and the programs it names.
    Scheme(crate::scheme::Drawing),
}

/// One macro: what it draws, and the code's alt text (empty = ours).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macro {
    pub body: Body,
    pub alt: String,
}

impl Macro {
    /// The macro as written: what the ledger hashes so the owner's
    /// approve or delete applies to this exact macro again.
    pub fn text(&self) -> String {
        match &self.body {
            Body::Ops { src, ops } => format!("magick {src} {ops:?}"),
            Body::Scheme(d) => {
                let progs: Vec<&str> = d.programs.iter().map(|p| p.text.as_str()).collect();
                format!("magick_scheme {}", progs.join("\n"))
            }
        }
    }

    /// How the hosted copy names the macro.
    pub fn describe(&self) -> String {
        match &self.body {
            Body::Ops { src, .. } => format!("(an image macro of {src})"),
            Body::Scheme(d) => {
                let root = &d.root().text;
                let mut shown: String = root.chars().take(400).collect();
                if shown.len() < root.len() {
                    shown.push('…');
                }
                format!("(an image macro: magick_scheme {shown})")
            }
        }
    }

    /// The pictures of the thread the macro may need.
    pub fn urls(&self) -> Vec<String> {
        match &self.body {
            Body::Ops { src, .. } => vec![src.clone()],
            Body::Scheme(d) => d.urls(),
        }
    }
}

/// The captions in a chain of ops: the words it writes on the picture.
fn caption_words(ops: &[Op]) -> String {
    ops.iter()
        .filter_map(|o| match o {
            Op::Caption(t, b) => Some(format!("{t}\n{b}").trim().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A macro from its record's fields.
pub fn parse(src: &str, ops: &str, alt: &str) -> Result<Macro, String> {
    let src = src.trim();
    if src.is_empty() {
        return Err("an image macro names no picture".to_string());
    }
    let ops: Vec<Op> = ops
        .split('\u{1e}')
        .filter(|o| !o.trim().is_empty())
        .map(|o| parse_op(&o.split('\u{1f}').collect::<Vec<_>>()))
        .collect::<Result<_, _>>()?;
    if ops.is_empty() {
        return Err("an image macro has no ops".to_string());
    }
    if ops.len() > MAX_OPS {
        return Err(format!("an image macro has at most {MAX_OPS} ops"));
    }
    Ok(Macro {
        body: Body::Ops {
            src: src.to_string(),
            ops,
        },
        alt: alt.to_string(),
    })
}

fn parse_op(w: &[&str]) -> Result<Op, String> {
    let name = w.first().map(|s| s.trim()).unwrap_or("");
    let args = &w[1.min(w.len())..];
    let n = |i: usize, lo: u32, hi: u32| -> Result<u32, String> {
        let s = args.get(i).map(|s| s.trim()).unwrap_or("");
        let v: u32 = s
            .parse()
            .map_err(|_| format!("{name}: \"{s}\" is not a whole number"))?;
        if v < lo || v > hi {
            return Err(format!("{name}: {v} is outside {lo}–{hi}"));
        }
        Ok(v)
    };
    let want = |k: usize| -> Result<(), String> {
        if args.len() != k {
            return Err(format!("{name} takes {k} argument(s)"));
        }
        Ok(())
    };
    Ok(match name {
        "resize" => {
            want(2)?;
            let (w, h) = (n(0, 0, MAX_SIDE)?, n(1, 0, MAX_SIDE)?);
            if w == 0 && h == 0 {
                return Err("resize: both sides 0".to_string());
            }
            Op::Resize(w, h)
        }
        "scale" => {
            want(1)?;
            Op::Scale(n(0, 1, 400)?)
        }
        "rotate" => {
            want(1)?;
            let d = args[0].trim();
            match d {
                "90" | "-270" => Op::Rotate(90),
                "180" | "-180" => Op::Rotate(180),
                "270" | "-90" => Op::Rotate(270),
                _ => return Err(format!("rotate: {d} (90, 180 or 270)")),
            }
        }
        "flip" => {
            want(0)?;
            Op::Flip
        }
        "flop" => {
            want(0)?;
            Op::Flop
        }
        "crop" => {
            want(4)?;
            Op::Crop(
                n(0, 0, 100_000)?,
                n(1, 0, 100_000)?,
                n(2, 1, 100_000)?,
                n(3, 1, 100_000)?,
            )
        }
        "border" => {
            if args.is_empty() || args.len() > 2 {
                return Err("border takes 1 or 2 arguments".to_string());
            }
            let c = match args.get(1) {
                Some(c) => colour(c)?,
                None => [0, 0, 0],
            };
            Op::Border(n(0, 1, 200)?, c)
        }
        "blur" => {
            want(1)?;
            let s = args[0].trim();
            let v: f32 = s
                .parse()
                .map_err(|_| format!("blur: \"{s}\" is not a number"))?;
            if !(0.1..=20.0).contains(&v) {
                return Err(format!("blur: {v} is outside 0.1–20"));
            }
            Op::Blur((v * 10.0).round() as u32)
        }
        "sharpen" => {
            want(0)?;
            Op::Sharpen
        }
        "grayscale" | "greyscale" => {
            want(0)?;
            Op::Grayscale
        }
        "invert" => {
            want(0)?;
            Op::Invert
        }
        "caption" => {
            if args.is_empty() || args.len() > 2 {
                return Err("caption takes 1 or 2 arguments".to_string());
            }
            let cut = |s: &str| s.trim().chars().take(200).collect::<String>();
            Op::Caption(
                cut(args[0]),
                args.get(1).map(|b| cut(b)).unwrap_or_default(),
            )
        }
        other => return Err(format!("unknown op \"{other}\"")),
    })
}

/// A colour by name (any case) or `#rrggbb`.
pub(crate) fn colour(s: &str) -> Result<[u8; 3], String> {
    let s = s.trim().to_ascii_lowercase();
    Ok(match s.as_str() {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "red" => [220, 30, 30],
        "green" => [30, 160, 60],
        "blue" => [30, 80, 220],
        "yellow" => [250, 220, 40],
        "orange" => [250, 140, 20],
        "purple" => [130, 40, 170],
        "pink" => [250, 150, 190],
        "brown" => [120, 70, 30],
        "cyan" => [0, 200, 220],
        "magenta" => [220, 0, 200],
        "gray" | "grey" => [128, 128, 128],
        hex if hex.starts_with('#') && hex.len() == 7 && hex.is_ascii() => {
            let b = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16);
            match (b(1), b(3), b(5)) {
                (Ok(r), Ok(g), Ok(bl)) => [r, g, bl],
                _ => return Err(format!("colour \"{s}\" (a name or #rrggbb)")),
            }
        }
        _ => return Err(format!("colour \"{s}\" (a name or #rrggbb)")),
    })
}

/// A URL a macro may fetch at all: HTTPS (a test's loopback stub
/// aside).
fn fetchable(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(test) && url.starts_with("http://127.0.0.1:"))
}

/// Every picture the post context shows (the post that asked and its
/// thread): the only URLs a macro may fetch.
pub fn thread_images(ctx: Option<&Value>) -> Vec<String> {
    let Some(ctx) = ctx else {
        return Vec::new();
    };
    std::iter::once(ctx.get("post"))
        .flatten()
        .chain(
            ctx.get("thread")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .filter_map(|p| p.pointer("/embed/images").and_then(Value::as_array))
        .flatten()
        .filter_map(|im| im.get(0).and_then(Value::as_str))
        .filter(|u| fetchable(u))
        .map(str::to_string)
        .collect()
}

/// The picture at `url`, when the thread shows it: HTTPS, no redirect,
/// at most [`MAX_FETCH_BYTES`].
pub async fn fetch(url: &str, allowed: &[String]) -> Result<Vec<u8>, String> {
    if !fetchable(url) || !allowed.iter().any(|a| a == url) {
        return Err("that picture is not one the thread shows".to_string());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| format!("fetching the picture: {e}"))?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetching the picture: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "fetching the picture: HTTP {}",
            resp.status().as_u16()
        ));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_FETCH_BYTES as u64)
    {
        return Err("the picture is larger than 8 MB".to_string());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("fetching the picture: {e}"))?
    {
        if body.len() + chunk.len() > MAX_FETCH_BYTES {
            return Err("the picture is larger than 8 MB".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The pictures a macro needs, fetched (or why each could not be): only
/// those the thread shows, at most [`crate::scheme::MAX_SOURCES`].
pub async fn sources(
    m: &Macro,
    allowed: &[String],
) -> std::collections::HashMap<String, Result<Vec<u8>, String>> {
    let mut out = std::collections::HashMap::new();
    let urls = m.urls();
    let ops = matches!(m.body, Body::Ops { .. });
    for url in urls {
        // A program may name pictures it never draws (a branch not
        // taken, a dead host): only the thread's are fetched, and the
        // program fails where it uses another.
        if !ops && !allowed.contains(&url) {
            continue;
        }
        if out.len() >= crate::scheme::MAX_SOURCES {
            break;
        }
        let got = fetch(&url, allowed).await;
        out.insert(url, got);
    }
    out
}

/// Draw a macro from its fetched pictures: (png, width, height, the words
/// written on it). CPU-bound: call it off the async workers.
pub fn draw(
    m: &Macro,
    sources: &std::collections::HashMap<String, Result<Vec<u8>, String>>,
) -> Result<(Vec<u8>, u32, u32, String), String> {
    match &m.body {
        Body::Ops { src, ops } => {
            let bytes = match sources.get(src) {
                Some(Ok(b)) => b,
                Some(Err(e)) => return Err(e.clone()),
                None => return Err("that picture is not one the thread shows".to_string()),
            };
            let (png, w, h) = render(bytes, ops)?;
            Ok((png, w, h, caption_words(ops)))
        }
        Body::Scheme(d) => {
            let drawn = crate::scheme::run(d, sources)?;
            let (png, w, h) = encode_fit(drawn.image)?;
            Ok((png, w, h, drawn.words))
        }
    }
}

/// Decode the picture, run the ops, and encode a PNG under
/// [`MAX_PNG_BYTES`]: (png, width, height). CPU-bound: call it off the
/// async workers.
pub fn render(bytes: &[u8], ops: &[Op]) -> Result<(Vec<u8>, u32, u32), String> {
    let mut img = decode(bytes)?;
    for op in ops {
        img = apply(img, op)?;
    }
    encode_fit(img)
}

/// A picture read with the decoder's limits, scaled down to
/// [`MAX_SIDE`].
pub(crate) fn decode(bytes: &[u8]) -> Result<RgbaImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("reading the picture: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(10_000);
    limits.max_image_height = Some(10_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader
        .decode()
        .map_err(|e| format!("reading the picture: {e}"))?;
    Ok(fit(img.to_rgba8(), MAX_SIDE))
}

/// `img` scaled down, keeping its shape, so no side passes `max`.
pub(crate) fn fit(img: RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (nw, nh) = fit_dims(w, h, max);
    if (nw, nh) == (w, h) {
        return img;
    }
    imageops::resize(&img, nw, nh, imageops::FilterType::Triangle)
}

/// `w`×`h` scaled down, keeping its shape, so no side passes `max`.
pub(crate) fn fit_dims(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w <= max && h <= max {
        return (w, h);
    }
    let f = (max as f64 / w as f64).min(max as f64 / h as f64);
    (
        ((w as f64 * f).round() as u32).clamp(1, max),
        ((h as f64 * f).round() as u32).clamp(1, max),
    )
}

fn apply(img: RgbaImage, op: &Op) -> Result<RgbaImage, String> {
    let (w, h) = img.dimensions();
    Ok(match op {
        Op::Resize(rw, rh) => {
            let (nw, nh) = match (*rw, *rh) {
                (0, nh) => (((w as u64 * nh as u64) / h.max(1) as u64) as u32, nh),
                (nw, 0) => (nw, ((h as u64 * nw as u64) / w.max(1) as u64) as u32),
                (nw, nh) => (nw, nh),
            };
            let (nw, nh) = (nw.clamp(1, MAX_SIDE), nh.clamp(1, MAX_SIDE));
            imageops::resize(&img, nw, nh, imageops::FilterType::CatmullRom)
        }
        Op::Scale(pct) => {
            // Straight to the size that fits: never a larger picture in
            // between (400% of 2000 px would be 8000).
            let (nw, nh) = fit_dims(
                ((w as u64 * *pct as u64) / 100).max(1) as u32,
                ((h as u64 * *pct as u64) / 100).max(1) as u32,
                MAX_SIDE,
            );
            imageops::resize(&img, nw, nh, imageops::FilterType::CatmullRom)
        }
        Op::Rotate(90) => imageops::rotate90(&img),
        Op::Rotate(180) => imageops::rotate180(&img),
        Op::Rotate(_) => imageops::rotate270(&img),
        Op::Flip => imageops::flip_vertical(&img),
        Op::Flop => imageops::flip_horizontal(&img),
        Op::Crop(x, y, cw, ch) => {
            if *x >= w || *y >= h {
                return Err(format!("crop: {x},{y} is outside the {w}×{h} picture"));
            }
            let (cw, ch) = ((*cw).min(w - x), (*ch).min(h - y));
            imageops::crop_imm(&img, *x, *y, cw, ch).to_image()
        }
        Op::Border(px, c) => {
            let mut out =
                RgbaImage::from_pixel(w + 2 * px, h + 2 * px, Rgba([c[0], c[1], c[2], 255]));
            imageops::overlay(&mut out, &img, *px as i64, *px as i64);
            fit(out, MAX_SIDE)
        }
        Op::Blur(tenths) => imageops::fast_blur(&img, *tenths as f32 / 10.0),
        Op::Sharpen => imageops::unsharpen(&img, 1.0, 5),
        Op::Grayscale => DynamicImage::ImageLuma8(imageops::grayscale(&img)).to_rgba8(),
        Op::Invert => {
            let mut out = img;
            imageops::invert(&mut out);
            out
        }
        Op::Caption(top, bottom) => {
            let mut out = img;
            caption(&mut out, top, bottom);
            out
        }
    })
}

/// A PNG of `img` under [`MAX_PNG_BYTES`]: an opaque picture loses its
/// alpha channel, and one still too large is scaled down by a quarter
/// until it fits (a photo as a PNG is large; Bluesky shows it at about
/// 1000 px anyway).
pub(crate) fn encode_fit(img: RgbaImage) -> Result<(Vec<u8>, u32, u32), String> {
    let mut img = img;
    for _ in 0..10 {
        let (w, h) = img.dimensions();
        let opaque = img.pixels().all(|p| p.0[3] == 255);
        let dynimg = if opaque {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(img.clone()).to_rgb8())
        } else {
            DynamicImage::ImageRgba8(img.clone())
        };
        let mut out = Vec::new();
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            &mut out,
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::Adaptive,
        );
        dynimg
            .write_with_encoder(enc)
            .map_err(|e| format!("writing the picture: {e}"))?;
        if out.len() <= MAX_PNG_BYTES {
            return Ok((out, w, h));
        }
        let (nw, nh) = ((w * 3 / 4).max(1), (h * 3 / 4).max(1));
        img = imageops::resize(&img, nw, nh, imageops::FilterType::Triangle);
    }
    Err("the picture does not fit Bluesky's 1 MB image limit".to_string())
}

// ── captions ────────────────────────────────────────────────────────

/// Write `top` along the top and `bottom` along the bottom, the way an
/// image macro does: centred, white with a black outline, in the IRC font
/// (GNU Unifont) scaled to the picture, wrapped at spaces, at most three
/// lines each.
pub(crate) fn caption(img: &mut RgbaImage, top: &str, bottom: &str) {
    let (w, h) = img.dimensions();
    // About 18 narrow characters across, and a line at most a sixth of
    // the height.
    let s = (w / (8 * 18)).min(h / (16 * 6)).max(1);
    let cols = ((w as u64 * 94 / 100) / (8 * s as u64)).max(1) as usize;
    let line_h = 16 * s + s;
    for (text, at_top) in [(top, true), (bottom, false)] {
        let lines: Vec<String> = wrap(text, cols).into_iter().take(3).collect();
        if lines.is_empty() {
            continue;
        }
        let block = lines.len() as u32 * line_h;
        let y0 = if at_top {
            2 * s
        } else {
            h.saturating_sub(block + 2 * s)
        };
        for (i, line) in lines.iter().enumerate() {
            let line_w = cells(line) as u32 * 8 * s;
            let x0 = w.saturating_sub(line_w) / 2;
            let y = (y0 + i as u32 * line_h) as i64;
            draw_text(img, line, x0 as i64, y, s, WHITE, Some(BLACK));
        }
    }
}

/// How many narrow cells `s` takes in the font (a wide glyph takes two).
fn cells(s: &str) -> usize {
    let f = veles_textart::font::font();
    s.chars()
        .map(|c| f.glyph(c).map(|g| g.width_px as usize / 8).unwrap_or(1))
        .sum()
}

/// `text` wrapped at spaces to lines of at most `cols` cells; a word
/// longer than a line is cut.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while cells(&word) > cols {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut head = String::new();
            let mut rest = String::new();
            for c in word.chars() {
                if rest.is_empty() && cells(&head) + cells(&c.to_string()) <= cols {
                    head.push(c);
                } else {
                    rest.push(c);
                }
            }
            if head.is_empty() {
                break;
            }
            lines.push(head);
            word = rest;
        }
        if word.is_empty() {
            continue;
        }
        let need = if cur.is_empty() {
            cells(&word)
        } else {
            cells(&cur) + 1 + cells(&word)
        };
        if need > cols && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);

/// One line of text at (x0, y0) (either may be off the picture), each
/// font pixel an s×s block. With an `outline` (a caption): each block
/// drawn twice, half a block apart (bold, as an image macro's letters
/// are), the outline first (every inked block grown by half a block),
/// then the fill over it. Without one (`annotate`): plain letters.
pub(crate) fn draw_text(
    img: &mut RgbaImage,
    line: &str,
    x0: i64,
    y0: i64,
    s: u32,
    fill_colour: Rgba<u8>,
    outline: Option<Rgba<u8>>,
) {
    let f = veles_textart::font::font();
    let o = (s / 2).max(1) as i64;
    let bold = if outline.is_some() { (s / 2) as i64 } else { 0 };
    let (w, h) = (img.width() as i64, img.height() as i64);
    let fill = |img: &mut RgbaImage, x: i64, y: i64, size: i64, grow: i64, c: Rgba<u8>| {
        for yy in (y - grow).max(0)..(y + size + grow).min(h) {
            for xx in (x - grow).max(0)..(x + size + grow).min(w) {
                img.put_pixel(xx as u32, yy as u32, c);
            }
        }
    };
    let passes: Vec<(Rgba<u8>, i64)> = match outline {
        Some(c) => vec![(c, o), (fill_colour, 0)],
        None => vec![(fill_colour, 0)],
    };
    for (colour, grow) in passes {
        let mut x = x0;
        for c in line.chars() {
            let g = f.glyph(c).or_else(|| f.glyph('?'));
            let Some(g) = g else { continue };
            for gy in 0..veles_textart::font::Glyph::HEIGHT {
                for gx in 0..g.width_px as usize {
                    if g.bit(gx, gy) {
                        let px = x + gx as i64 * s as i64;
                        let py = y0 + gy as i64 * s as i64;
                        for dx in if bold > 0 { vec![0, bold] } else { vec![0] } {
                            fill(img, px + dx, py, s as i64, grow, colour);
                        }
                    }
                }
            }
            x += g.width_px as i64 * s as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(img: &RgbaImage) -> Vec<u8> {
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    /// A macro's ops are read from slopdrop's record: words joined by
    /// U+001F, ops by U+001E. Unknown ops, wrong arities and numbers out
    /// of range are refused, with the op named.
    #[test]
    fn a_macro_is_read_from_its_record() {
        let m = parse(
            "https://cdn.bsky.app/x@jpeg",
            "caption\u{1f}when you\u{1f}see it\u{1e}rotate\u{1f}-90\u{1e}resize\u{1f}400\u{1f}0\u{1e}blur\u{1f}1.5\u{1e}border\u{1f}4\u{1f}#ff0000",
            "",
        )
        .unwrap();
        let Body::Ops { ops, .. } = &m.body else {
            panic!("an op chain")
        };
        assert_eq!(
            *ops,
            vec![
                Op::Caption("when you".into(), "see it".into()),
                Op::Rotate(270),
                Op::Resize(400, 0),
                Op::Blur(15),
                Op::Border(4, [255, 0, 0]),
            ]
        );
        assert_eq!(caption_words(ops), "when you\nsee it");
        for (ops, why) in [
            ("explode", "unknown op"),
            ("resize\u{1f}400", "takes 2"),
            ("resize\u{1f}0\u{1f}0", "both sides 0"),
            ("rotate\u{1f}45", "rotate: 45"),
            ("scale\u{1f}9000", "outside"),
            ("border\u{1f}4\u{1f}chartreuse", "colour"),
            ("", "no ops"),
        ] {
            let e = parse("https://x", ops, "").unwrap_err();
            assert!(e.contains(why), "{ops:?}: {e}");
        }
        let many = ["flip"; MAX_OPS + 1].join("\u{1e}");
        assert!(parse("https://x", &many, "").is_err());
    }

    /// Only a picture the thread shows is fetched: the URLs in the post
    /// context's `embed.images`, matched exactly.
    #[tokio::test]
    async fn only_a_picture_the_thread_shows_is_fetched() {
        let ctx = serde_json::json!({
            "post": {"uri": "at://a/p/1", "embed": {"images": [["https://cdn.test/a@jpeg", "a"]]}},
            "thread": [{"uri": "at://b/p/2", "embed": {"images": [["https://cdn.test/b@jpeg", ""],
                                                                   ["http://plain.test/c", ""]]}},
                       {"uri": "at://c/p/3", "text": "no picture"}],
        });
        let allowed = thread_images(Some(&ctx));
        assert_eq!(
            allowed,
            vec!["https://cdn.test/a@jpeg", "https://cdn.test/b@jpeg"]
        );
        for url in [
            "https://cdn.test/a@jpeg?x",
            "http://plain.test/c",
            "https://10.0.30.1/secret",
            "file:///etc/passwd",
        ] {
            let e = fetch(url, &allowed).await.unwrap_err();
            assert!(e.contains("not one the thread shows"), "{url}: {e}");
        }
    }

    /// The ops do what they say: the shape after each, a caption writes
    /// white and black over the top and bottom and leaves the middle, a
    /// crop clips to the picture, and the PNG fits Bluesky's limit even
    /// for a noisy picture that would not as drawn.
    #[test]
    fn the_ops_change_the_picture_as_named() {
        let mut src = RgbaImage::from_pixel(300, 200, Rgba([200, 30, 30, 255]));
        src.put_pixel(0, 0, Rgba([0, 0, 255, 255]));
        let bytes = png_of(&src);
        let shape = |ops: &[Op]| {
            let (png, w, h) = render(&bytes, ops).unwrap();
            let img = image::load_from_memory(&png).unwrap().to_rgba8();
            assert_eq!(img.dimensions(), (w, h));
            img
        };
        assert_eq!(shape(&[Op::Resize(150, 0)]).dimensions(), (150, 100));
        assert_eq!(shape(&[Op::Scale(50)]).dimensions(), (150, 100));
        assert_eq!(shape(&[Op::Rotate(90)]).dimensions(), (200, 300));
        assert_eq!(
            shape(&[Op::Crop(250, 150, 500, 500)]).dimensions(),
            (50, 50)
        );
        assert_eq!(shape(&[Op::Border(10, [0, 0, 0])]).dimensions(), (320, 220));
        assert!(render(&bytes, &[Op::Crop(400, 0, 5, 5)]).is_err());
        // The marked corner moves as the op says.
        assert_eq!(shape(&[Op::Flop]).get_pixel(299, 0).0, [0, 0, 255, 255]);
        assert_eq!(shape(&[Op::Flip]).get_pixel(0, 199).0, [0, 0, 255, 255]);
        let g = shape(&[Op::Grayscale]).get_pixel(5, 5).0;
        assert!(g[0] == g[1] && g[1] == g[2], "{g:?}");
        assert_eq!(shape(&[Op::Invert]).get_pixel(5, 5).0, [55, 225, 225, 255]);

        let big = RgbaImage::from_pixel(600, 400, Rgba([200, 30, 30, 255]));
        let c = render(
            &png_of(&big),
            &[Op::Caption("TOP TEXT".into(), "BOTTOM TEXT".into())],
        )
        .unwrap();
        let img = image::load_from_memory(&c.0).unwrap().to_rgba8();
        let count = |y0: u32, y1: u32, rgb: [u8; 3]| {
            (y0..y1)
                .flat_map(|y| (0..600).map(move |x| (x, y)))
                .filter(|&(x, y)| img.get_pixel(x, y).0[..3] == rgb)
                .count()
        };
        assert!(count(0, 80, [255, 255, 255]) > 200, "white letters on top");
        assert!(count(0, 80, [0, 0, 0]) > 200, "with a black outline");
        assert!(count(320, 400, [255, 255, 255]) > 200, "and at the bottom");
        assert_eq!(
            count(150, 250, [255, 255, 255]),
            0,
            "the middle is the picture"
        );

        // Noise does not compress: the PNG is scaled down until it fits.
        let mut noisy = RgbaImage::new(700, 700);
        let mut x: u32 = 1;
        for p in noisy.pixels_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *p = Rgba([x as u8, (x >> 8) as u8, (x >> 16) as u8, 255]);
        }
        let (png, w, _) = render(&png_of(&noisy), &[Op::Flop]).unwrap();
        assert!(png.len() <= MAX_PNG_BYTES, "{}", png.len());
        assert!(w < 700, "scaled down to fit: {w}");
    }

    /// A caption wraps at spaces and cuts a word longer than a line.
    #[test]
    fn a_caption_wraps_at_spaces() {
        assert_eq!(wrap("when you see it", 8), vec!["when you", "see it"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("  ", 4), Vec::<String>::new());
    }

    /// A loopback server that answers every request with `len` bytes,
    /// claiming `claim` as its length or claiming nothing.
    fn stub_bytes(claim: Option<usize>, len: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf);
                let head = if let Some(n) = claim {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n")
                } else {
                    "HTTP/1.0 200 OK\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&vec![0u8; len]);
            }
        });
        format!("http://127.0.0.1:{port}/big.png")
    }

    /// No more than 8 MB of a picture is read: one that says it is
    /// larger is refused before its body is read (here it then sends 1 KB
    /// and closes, which only that refusal names), one that does not say
    /// is cut off at the cap.
    #[tokio::test]
    async fn a_picture_past_8_mb_is_not_read() {
        for (claim, len) in [
            (Some(MAX_FETCH_BYTES + 1024), 1024),
            (None, MAX_FETCH_BYTES + 1024),
        ] {
            let url = stub_bytes(claim, len);
            let e = fetch(&url, std::slice::from_ref(&url)).await.unwrap_err();
            assert!(e.contains("larger than 8 MB"), "{claim:?}: {e}");
        }
        let url = stub_bytes(None, 1024);
        assert_eq!(
            fetch(&url, std::slice::from_ref(&url)).await.unwrap().len(),
            1024
        );
    }

    /// A picture wider than the decoder's limit is refused before it is
    /// decoded (a few bytes of PNG can claim a huge canvas).
    #[test]
    fn a_huge_canvas_is_not_decoded() {
        let wide = RgbaImage::new(12_000, 1);
        let e = render(&png_of(&wide), &[Op::Flip]).unwrap_err();
        assert!(e.contains("reading the picture"), "{e}");
    }
}
