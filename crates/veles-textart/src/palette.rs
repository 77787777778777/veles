//! The colours IRC art is written in.
//!
//! mIRC numbers 0–98 (`\x03NN`): the 16 classic colours and the 83
//! extended ones mIRC 7 added (16–98), from mIRC's own table as
//! documented at modern.ircdocs.horse. 99 is "the client's default".
//! ANSI: the 16 xterm colours, the 6×6×6 cube and the grey ramp of the
//! 256-colour set.

/// An sRGB colour.
pub type Rgb = [u8; 3];

const fn hex(v: u32) -> Rgb {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// mIRC colours 0–98.
pub const MIRC: [Rgb; 99] = [
    hex(0xffffff),
    hex(0x000000),
    hex(0x00007f),
    hex(0x009300),
    hex(0xff0000),
    hex(0x7f0000),
    hex(0x9c009c),
    hex(0xfc7f00),
    hex(0xffff00),
    hex(0x00fc00),
    hex(0x009393),
    hex(0x00ffff),
    hex(0x0000fc),
    hex(0xff00ff),
    hex(0x7f7f7f),
    hex(0xd2d2d2),
    // 16–27
    hex(0x470000),
    hex(0x472100),
    hex(0x474700),
    hex(0x324700),
    hex(0x004700),
    hex(0x00472c),
    hex(0x004747),
    hex(0x002747),
    hex(0x000047),
    hex(0x2e0047),
    hex(0x470047),
    hex(0x47002a),
    // 28–39
    hex(0x740000),
    hex(0x743a00),
    hex(0x747400),
    hex(0x517400),
    hex(0x007400),
    hex(0x007449),
    hex(0x007474),
    hex(0x004074),
    hex(0x000074),
    hex(0x4b0074),
    hex(0x740074),
    hex(0x740045),
    // 40–51
    hex(0xb50000),
    hex(0xb56300),
    hex(0xb5b500),
    hex(0x7db500),
    hex(0x00b500),
    hex(0x00b571),
    hex(0x00b5b5),
    hex(0x0063b5),
    hex(0x0000b5),
    hex(0x7500b5),
    hex(0xb500b5),
    hex(0xb5006b),
    // 52–63
    hex(0xff0000),
    hex(0xff8c00),
    hex(0xffff00),
    hex(0xb2ff00),
    hex(0x00ff00),
    hex(0x00ffa0),
    hex(0x00ffff),
    hex(0x008cff),
    hex(0x0000ff),
    hex(0xa500ff),
    hex(0xff00ff),
    hex(0xff0098),
    // 64–75
    hex(0xff5959),
    hex(0xffb459),
    hex(0xffff71),
    hex(0xcfff60),
    hex(0x6fff6f),
    hex(0x65ffc9),
    hex(0x6dffff),
    hex(0x59b4ff),
    hex(0x5959ff),
    hex(0xc459ff),
    hex(0xff66ff),
    hex(0xff59bc),
    // 76–87
    hex(0xff9c9c),
    hex(0xffd39c),
    hex(0xffff9c),
    hex(0xe2ff9c),
    hex(0x9cff9c),
    hex(0x9cffdb),
    hex(0x9cffff),
    hex(0x9cd3ff),
    hex(0x9c9cff),
    hex(0xdc9cff),
    hex(0xff9cff),
    hex(0xff94d3),
    // 88–98: the grey ramp
    hex(0x000000),
    hex(0x131313),
    hex(0x282828),
    hex(0x363636),
    hex(0x4d4d4d),
    hex(0x656565),
    hex(0x818181),
    hex(0x9f9f9f),
    hex(0xbcbcbc),
    hex(0xe2e2e2),
    hex(0xffffff),
];

/// An mIRC colour number: `None` for 99 (the client's default) and
/// anything past it.
pub fn mirc(n: u8) -> Option<Rgb> {
    MIRC.get(n as usize).copied()
}

const ANSI16: [Rgb; 16] = [
    hex(0x000000),
    hex(0xcd0000),
    hex(0x00cd00),
    hex(0xcdcd00),
    hex(0x0000ee),
    hex(0xcd00cd),
    hex(0x00cdcd),
    hex(0xe5e5e5),
    hex(0x7f7f7f),
    hex(0xff0000),
    hex(0x00ff00),
    hex(0xffff00),
    hex(0x5c5cff),
    hex(0xff00ff),
    hex(0x00ffff),
    hex(0xffffff),
];

/// One of the 256 ANSI colours.
pub fn ansi256(n: u8) -> Rgb {
    match n {
        0..=15 => ANSI16[n as usize],
        16..=231 => {
            let i = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            [level(i / 36), level((i / 6) % 6), level(i % 6)]
        }
        _ => {
            let g = 8 + 10 * (n - 232);
            [g, g, g]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_have_their_landmarks() {
        assert_eq!(mirc(0), Some([255, 255, 255]));
        assert_eq!(mirc(15), Some([0xd2, 0xd2, 0xd2]));
        assert_eq!(mirc(52), Some([255, 0, 0]));
        assert_eq!(mirc(98), Some([255, 255, 255]));
        assert_eq!(mirc(99), None, "99 is the client's default");
        assert_eq!(ansi256(1), [0xcd, 0, 0]);
        assert_eq!(ansi256(16), [0, 0, 0]);
        assert_eq!(ansi256(21), [0, 0, 255]);
        assert_eq!(ansi256(196), [255, 0, 0]);
        assert_eq!(ansi256(231), [255, 255, 255]);
        assert_eq!(ansi256(232), [8, 8, 8]);
        assert_eq!(ansi256(255), [238, 238, 238]);
    }
}
