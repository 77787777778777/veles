//! ASCII synthwave art — shared by the TUI (empty state, `/art`) and
//! the readline REPL banner. Pure data + role labels; each surface maps
//! the roles onto its own palette (TUI styles, REPL ANSI, tests plain).

/// Color roles in the scene. Surfaces decide the actual colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtRole {
    /// Top of the sky (darker).
    Sky,
    /// The glow band just above the horizon.
    Horizon,
    /// The sun body.
    Sun,
    /// The sun's slice lines (cut-out).
    Slice,
    /// Mountain silhouettes.
    Mountain,
    /// The perspective grid.
    Grid,
    /// Accent text (wordmark / tagline).
    Accent,
}

pub struct ArtLine {
    pub text: String,
    pub role: ArtRole,
}

fn parse(raw: &str) -> Vec<ArtLine> {
    raw.lines()
        .map(|l| {
            let role = match l.as_bytes().first().copied() {
                Some(b'S') => ArtRole::Sky,
                Some(b'H') => ArtRole::Horizon,
                Some(b'U') => ArtRole::Sun,
                Some(b'L') => ArtRole::Slice,
                Some(b'M') => ArtRole::Mountain,
                Some(b'G') => ArtRole::Grid,
                Some(b'A') => ArtRole::Accent,
                _ => ArtRole::Horizon,
            };
            ArtLine {
                text: l[1..].to_string(),
                role,
            }
        })
        .collect()
}

/// The full synthwave scene: sliced sun over mountains over a grid.
/// 50 columns wide, 16 rows tall — fits a 60-col terminal.
pub fn synthwave_scene() -> Vec<ArtLine> {
    parse(
        "\
S                ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
U              ██████████████████████████
L            ████▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀████
U            ████████████████████████████
L            ████▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄████
U              ██████████████████████████
L                ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
H
M            ▲   /\\                      /\\
M               /  \\                    /  \\
M              /    \\                  /    \\
M             /      \\                /      \\
M            /________\\              /________\\
M           ///////////              ///////////
G          ∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖
G           ∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖
G            ∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖
G             ∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖∖
A             ▲ VELES · neon impressionism",
    )
}

/// Compact wordmark for the REPL banner: 3 lines, no sun.
pub fn wordmark() -> Vec<ArtLine> {
    parse(
        "\
A  ▲ VELES
G≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈≈
H  neon impressionism",
    )
}

/// ANSI-colored rendering for the readline REPL (truecolor).
pub fn render_ansi(scene: &[ArtLine]) -> String {
    let mut out = String::new();
    for l in scene {
        let (r, g, b) = match l.role {
            ArtRole::Sky => (0x1a, 0x10, 0x26),
            ArtRole::Horizon => (0x8a, 0x76, 0xa8),
            ArtRole::Sun => (0xff, 0x2d, 0x78),
            ArtRole::Slice => (0x0d, 0x06, 0x13),
            ArtRole::Mountain => (0xb9, 0x67, 0xff),
            ArtRole::Grid => (0x2d, 0xe2, 0xe6),
            ArtRole::Accent => (0xff, 0xbe, 0x3d),
        };
        out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{}\x1b[0m\n", l.text));
    }
    out
}

#[cfg(test)]
mod art_tests {
    use super::*;

    #[test]
    fn scene_is_nonempty_with_all_roles() {
        let scene = synthwave_scene();
        assert!(scene.len() >= 12, "scene too short: {} rows", scene.len());
        for role in [
            ArtRole::Sky,
            ArtRole::Horizon,
            ArtRole::Sun,
            ArtRole::Slice,
            ArtRole::Mountain,
            ArtRole::Grid,
            ArtRole::Accent,
        ] {
            assert!(
                scene.iter().any(|l| l.role == role),
                "missing role {role:?}"
            );
        }
    }

    #[test]
    fn scene_lines_nonempty() {
        // organic art — widths vary; just check nothing is empty
        for l in synthwave_scene() {
            assert!(
                !l.text.is_empty() || l.role == ArtRole::Horizon,
                "unexpected empty line: {:?}",
                l.text
            );
        }
    }

    #[test]
    fn wordmark_render_has_ansi() {
        let out = render_ansi(&wordmark());
        assert!(out.contains("\x1b[38;2;"));
        assert!(out.contains("VELES"));
    }
}
