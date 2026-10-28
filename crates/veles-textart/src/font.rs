//! GNU Unifont 18 (planes 0 and 1), packed by `examples/pack_font.rs`
//! (`scripts/textart-font.sh`) and unpacked once on first use.
//!
//! Unifont is the IRC font: an 8×16 bitmap for every narrow character
//! and 16×16 for every wide one, covering the whole BMP — block
//! elements, box drawing, braille, CJK, the Latin-1 that CP437 art was
//! converted to — and plane 1's emoji, legacy-computing symbols and
//! mathematical alphanumerics. Licence: GPL-2.0-or-later with the font
//! embedding exception, or SIL OFL 1.1 (`font/OFL-1.1.txt`).

use std::io::Read;
use std::sync::OnceLock;

const PACKED: &[u8] = include_bytes!("../font/unifont.bin.z");

/// One glyph: 16 rows, `width_px / 8` bytes per row, MSB is the leftmost
/// pixel.
#[derive(Clone, Copy, Debug)]
pub struct Glyph<'a> {
    pub width_px: u8,
    rows: &'a [u8],
}

impl Glyph<'_> {
    pub const HEIGHT: usize = 16;

    /// Is pixel (x, y) inked?
    pub fn bit(&self, x: usize, y: usize) -> bool {
        let bpr = self.width_px as usize / 8;
        if x >= self.width_px as usize || y >= Self::HEIGHT {
            return false;
        }
        self.rows[y * bpr + x / 8] & (0x80 >> (x % 8)) != 0
    }

    /// Where the ink sits: (first inked row, last inked row), or `None`
    /// for a blank glyph. A combining mark above its base has its ink in
    /// the top rows, one below in the bottom rows.
    pub fn ink_rows(&self) -> Option<(usize, usize)> {
        let bpr = self.width_px as usize / 8;
        let inked = |y: usize| self.rows[y * bpr..(y + 1) * bpr].iter().any(|b| *b != 0);
        let top = (0..Self::HEIGHT).find(|&y| inked(y))?;
        let bottom = (0..Self::HEIGHT).rev().find(|&y| inked(y))?;
        Some((top, bottom))
    }
}

pub struct Font {
    cps: Vec<u32>,
    meta: Vec<(u8, u32)>,
    data: Vec<u8>,
}

impl Font {
    fn unpack(packed: &[u8]) -> Result<Font, String> {
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(packed)
            .read_to_end(&mut raw)
            .map_err(|e| format!("font: {e}"))?;
        if raw.len() < 9 || &raw[..4] != b"VTXF" || raw[4] != 1 {
            return Err("font: not a VTXF v1 table".into());
        }
        let count = u32::from_le_bytes(raw[5..9].try_into().unwrap()) as usize;
        let idx_end = 9 + count * 9;
        if raw.len() < idx_end {
            return Err("font: index cut short".into());
        }
        let mut cps = Vec::with_capacity(count);
        let mut meta = Vec::with_capacity(count);
        for e in raw[9..idx_end].as_chunks::<9>().0 {
            cps.push(u32::from_le_bytes(e[0..4].try_into().unwrap()));
            meta.push((e[4], u32::from_le_bytes(e[5..9].try_into().unwrap())));
        }
        let data = raw[idx_end..].to_vec();
        Ok(Font { cps, meta, data })
    }

    /// The glyph for `c`, if Unifont draws it.
    pub fn glyph(&self, c: char) -> Option<Glyph<'_>> {
        let i = self.cps.binary_search(&(c as u32)).ok()?;
        let (w, off) = self.meta[i];
        let len = w as usize / 8 * Glyph::HEIGHT;
        let rows = self.data.get(off as usize..off as usize + len)?;
        Some(Glyph { width_px: w, rows })
    }

    /// How many glyphs the table holds.
    pub fn len(&self) -> usize {
        self.cps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cps.is_empty()
    }
}

/// The font, unpacked on first use (about 4 MB, a few tens of ms).
pub fn font() -> &'static Font {
    static F: OnceLock<Font> = OnceLock::new();
    F.get_or_init(|| Font::unpack(PACKED).expect("the packed Unifont table is part of the crate"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_covers_art_text_and_emoji() {
        let f = font();
        assert!(f.len() > 100_000, "{}", f.len());
        assert_eq!(f.glyph('A').unwrap().width_px, 8);
        assert_eq!(f.glyph('█').unwrap().width_px, 8);
        assert_eq!(f.glyph('⣿').unwrap().width_px, 8, "braille");
        assert_eq!(f.glyph('漢').unwrap().width_px, 16);
        assert_eq!(f.glyph('😀').unwrap().width_px, 16, "plane 1 emoji");
        assert!(f.glyph('\u{1FB00}').is_some(), "legacy computing sextants");
        let full = f.glyph('█').unwrap();
        assert!((0..8).all(|x| (0..16).all(|y| full.bit(x, y))));
        assert_eq!(f.glyph(' ').unwrap().ink_rows(), None);
        let (top, _) = f
            .glyph('\u{301}')
            .expect("combining acute")
            .ink_rows()
            .unwrap();
        assert!(top < 6, "an above-mark inks the top rows: {top}");
    }
}
