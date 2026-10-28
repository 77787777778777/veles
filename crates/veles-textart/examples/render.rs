//! Draw IRC text from stdin into a PNG: for looking at what the renderer
//! makes of real art, and for batch jobs (the stamp prebake renders every
//! `ST*` drawing this way).
//!
//!   printf '\x0304red\x03 text' | cargo run -p veles-textart --example render -- out.png
//!   … --example render -- out.png --light        (black on white)
//!   … --example render -- out.png --text 60      (long text, wrapped)

use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .expect("usage: render OUT.png [--light] [--text COLS]");
    let mut opts = veles_textart::Options::default();
    let mut text_cols = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--light" => opts.theme = veles_textart::Theme::Light,
            "--text" => text_cols = args.next().and_then(|c| c.parse().ok()),
            other => panic!("unknown argument {other}"),
        }
    }
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("stdin is UTF-8");
    let png = match text_cols {
        Some(c) => veles_textart::render_text(&input, c, &opts),
        None => veles_textart::render(&input, &opts),
    }
    .unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&out, &png.bytes).unwrap_or_else(|e| panic!("{out}: {e}"));
    eprintln!(
        "{out}: {}x{} px, {}x{} cells, scale {}, {} bytes{}; words: {:?}",
        png.width,
        png.height,
        png.cols,
        png.rows,
        png.scale,
        png.bytes.len(),
        if png.truncated { ", truncated" } else { "" },
        veles_textart::words(&input)
    );
}
