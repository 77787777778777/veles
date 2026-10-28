//! Pack GNU Unifont `.hex` files into the crate's glyph table
//! (`font/unifont.bin.z`). Run by `scripts/textart-font.sh`; not part of
//! the library.
//!
//!   cargo run -p veles-textart --example pack_font -- OUT HEX…
//!
//! A `.hex` line is `CODEPOINT:BITMAP`: 16 rows, each row the glyph's
//! width in bits as hex (32 hex digits for an 8-pixel glyph, 64 for 16).
//! Later files override earlier ones, as Unifont's own `hexmerge` does.
//! Only planes 0 and 1 are kept (the BMP, and the emoji, symbols, legacy
//! computing and math alphanumerics of plane 1).
//!
//! The table, before deflate (`src/font.rs` reads it):
//!   b"VTXF", version u8 = 1, count u32 LE,
//!   count × { codepoint u32 LE, width_px u8, offset u32 LE } (sorted),
//!   the bitmaps: 16 rows × width_px/8 bytes per glyph.

use std::collections::BTreeMap;
use std::io::Write;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("usage: pack_font OUT HEX…");
    let mut glyphs: BTreeMap<u32, (u8, Vec<u8>)> = BTreeMap::new();
    for path in args {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (cp, bits) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("{path}:{}: no ':'", n + 1));
            let cp =
                u32::from_str_radix(cp, 16).unwrap_or_else(|e| panic!("{path}:{}: {e}", n + 1));
            if cp > 0x1_FFFF {
                continue;
            }
            if bits.len() % 32 != 0 || bits.is_empty() {
                panic!("{path}:{}: {} hex digits is not 16 rows", n + 1, bits.len());
            }
            let row_bytes = bits.len() / 32;
            let width_px = (row_bytes * 8) as u8;
            let bytes: Vec<u8> = (0..bits.len() / 2)
                .map(|i| u8::from_str_radix(&bits[2 * i..2 * i + 2], 16).unwrap())
                .collect();
            glyphs.insert(cp, (width_px, bytes));
        }
    }
    let mut index = Vec::new();
    let mut data = Vec::new();
    for (cp, (w, bytes)) in &glyphs {
        index.extend_from_slice(&cp.to_le_bytes());
        index.push(*w);
        index.extend_from_slice(&(data.len() as u32).to_le_bytes());
        data.extend_from_slice(bytes);
    }
    let mut raw = Vec::new();
    raw.extend_from_slice(b"VTXF");
    raw.push(1);
    raw.extend_from_slice(&(glyphs.len() as u32).to_le_bytes());
    raw.extend_from_slice(&index);
    raw.extend_from_slice(&data);
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw).unwrap();
    let packed = z.finish().unwrap();
    std::fs::write(&out, &packed).unwrap_or_else(|e| panic!("{out}: {e}"));
    eprintln!(
        "{} glyphs, {} bytes raw, {} bytes packed -> {out}",
        glyphs.len(),
        raw.len(),
        packed.len()
    );
}
