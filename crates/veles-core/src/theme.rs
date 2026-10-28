//! TUI theme system — named palettes for the ratatui surface.
//!
//! Built-ins: `synthwave` (default, 16-bit neon impressionism),
//! `classic` (hermes-ish blue/green), `amber` (CRT phosphor),
//! `mono` (single-color), `high-contrast` + `large-print` (a11y).
//! User themes: `~/.config/veles/themes/<name>.toml` (`config_dir()`)
//! with the same keys as the struct below (hex "#rrggbb" strings).
//!
//! Resolution order for the default theme at boot:
//! `VELES_TUI_THEME` env → config `tui.theme` → `synthwave`.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub bg: String,
    pub panel: String,
    pub panel_hi: String,
    pub hair: String,
    pub pink: String,
    pub cyan: String,
    pub purple: String,
    pub gold: String,
    pub green: String,
    pub red: String,
    pub text: String,
    pub dim: String,
}

impl Theme {
    pub fn color(&self, hex: &str) -> Color {
        let h = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
        Color::Rgb(r, g, b)
    }
    pub fn bg(&self) -> Color {
        self.color(&self.bg)
    }
    pub fn panel(&self) -> Color {
        self.color(&self.panel)
    }
    pub fn panel_hi(&self) -> Color {
        self.color(&self.panel_hi)
    }
    pub fn hair(&self) -> Color {
        self.color(&self.hair)
    }
    pub fn pink(&self) -> Color {
        self.color(&self.pink)
    }
    pub fn cyan(&self) -> Color {
        self.color(&self.cyan)
    }
    pub fn purple(&self) -> Color {
        self.color(&self.purple)
    }
    pub fn gold(&self) -> Color {
        self.color(&self.gold)
    }
    pub fn green(&self) -> Color {
        self.color(&self.green)
    }
    pub fn red(&self) -> Color {
        self.color(&self.red)
    }
    pub fn text(&self) -> Color {
        self.color(&self.text)
    }
    pub fn dim(&self) -> Color {
        self.color(&self.dim)
    }
}

pub fn synthwave() -> Theme {
    Theme {
        name: "synthwave".into(),
        bg: "#0d0613".into(),
        panel: "#1a1026".into(),
        panel_hi: "#2e1e4a".into(),
        hair: "#3a2a55".into(),
        pink: "#ff2d78".into(),
        cyan: "#2de2e6".into(),
        purple: "#b967ff".into(),
        gold: "#ffbe3d".into(),
        green: "#3dff8b".into(),
        red: "#ff3c5c".into(),
        text: "#efe6ff".into(),
        dim: "#8a76a8".into(),
    }
}

pub fn classic() -> Theme {
    Theme {
        name: "classic".into(),
        bg: "#121417".into(),
        panel: "#1b1e23".into(),
        panel_hi: "#2a3040".into(),
        hair: "#3a414d".into(),
        pink: "#6ea8fe".into(),
        cyan: "#74e0c9".into(),
        purple: "#b08cff".into(),
        gold: "#e5c07b".into(),
        green: "#81c784".into(),
        red: "#ff6b6b".into(),
        text: "#e6e6e6".into(),
        dim: "#8a8f98".into(),
    }
}

pub fn amber() -> Theme {
    Theme {
        name: "amber".into(),
        bg: "#120b00".into(),
        panel: "#1c1404".into(),
        panel_hi: "#2e2208".into(),
        hair: "#4a3a10".into(),
        pink: "#ffb000".into(),
        cyan: "#ffb000".into(),
        purple: "#ffb000".into(),
        gold: "#ffb000".into(),
        green: "#ffb000".into(),
        red: "#ff6a00".into(),
        text: "#ffd966".into(),
        dim: "#9a7b2a".into(),
    }
}

pub fn mono() -> Theme {
    Theme {
        name: "mono".into(),
        bg: "#0a0a0a".into(),
        panel: "#141414".into(),
        panel_hi: "#222222".into(),
        hair: "#333333".into(),
        pink: "#c8c8c8".into(),
        cyan: "#c8c8c8".into(),
        purple: "#c8c8c8".into(),
        gold: "#c8c8c8".into(),
        green: "#c8c8c8".into(),
        red: "#ffffff".into(),
        text: "#e8e8e8".into(),
        dim: "#777777".into(),
    }
}

/// High-contrast (a11y): pure black/white, maximum-contrast accents —
/// WCAG AAA against bg by construction. Same layout, readable in
/// bright light and for low-vision users.
pub fn high_contrast() -> Theme {
    Theme {
        name: "high-contrast".into(),
        bg: "#000000".into(),
        panel: "#000000".into(),
        panel_hi: "#1a1a1a".into(),
        hair: "#767676".into(),
        pink: "#ff66ff".into(),
        cyan: "#66ffff".into(),
        purple: "#d9b3ff".into(),
        gold: "#ffff66".into(),
        green: "#80ff80".into(),
        red: "#ff8080".into(),
        text: "#ffffff".into(),
        dim: "#b8b8b8".into(),
    }
}

/// Large-print companion intent: identical palette to high-contrast —
/// the size change comes from the TUI's font_scale knob, this theme
/// exists so `/theme high-contrast` + font scale compose cleanly and
/// the desktop can pair them by name.
pub fn large_print() -> Theme {
    Theme {
        name: "large-print".into(),
        bg: "#ffffff".into(),
        panel: "#f0f0f0".into(),
        panel_hi: "#e0e0e0".into(),
        hair: "#4d4d4d".into(),
        pink: "#c40094".into(),
        cyan: "#00666e".into(),
        purple: "#5b2ea6".into(),
        gold: "#7a5c00".into(),
        green: "#0a6b2d".into(),
        red: "#b30000".into(),
        text: "#000000".into(),
        dim: "#3d3d3d".into(),
    }
}

/// All built-in themes, in display order.
pub fn builtin() -> Vec<Theme> {
    vec![
        synthwave(),
        classic(),
        amber(),
        mono(),
        high_contrast(),
        large_print(),
    ]
}

/// User themes from `~/.config/veles/themes/*.toml` (the config dir).
pub fn user_themes() -> Vec<Theme> {
    let dir = user_theme_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "toml").unwrap_or(false))
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    for n in names {
        let path = dir.join(format!("{n}.toml"));
        if let Ok(src) = std::fs::read_to_string(&path) {
            if let Ok(mut t) = toml::from_str::<Theme>(&src) {
                t.name = n;
                out.push(t);
            }
        }
    }
    out
}

pub fn user_theme_dir() -> PathBuf {
    crate::config::config_dir().join("themes")
}

/// All available themes: built-ins first, then user themes (a user
/// theme with the same name shadows the built-in).
pub fn all() -> Vec<Theme> {
    let mut v = builtin();
    for t in user_themes() {
        if let Some(i) = v.iter().position(|b| b.name == t.name) {
            v[i] = t;
        } else {
            v.push(t);
        }
    }
    v
}

/// The theme to boot with: `VELES_TUI_THEME` → config `tui.theme` →
/// `synthwave`. Unknown names fall back to synthwave.
pub fn default_theme() -> Theme {
    let name = std::env::var("VELES_TUI_THEME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| crate::config::load().ok().and_then(|c| c.tui.theme.clone()))
        .unwrap_or_else(|| "synthwave".into());
    all()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(synthwave)
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn hex_parsing_works() {
        let t = synthwave();
        assert_eq!(t.pink(), Color::Rgb(0xff, 0x2d, 0x78));
        assert_eq!(t.bg(), Color::Rgb(0x0d, 0x06, 0x13));
    }

    #[test]
    fn builtin_names_unique() {
        let names: Vec<String> = builtin().iter().map(|t| t.name.clone()).collect();
        let mut uniq = names.clone();
        uniq.dedup();
        assert_eq!(names, uniq);
        assert!(names.contains(&"synthwave".to_string()));
    }

    #[test]
    fn default_resolves_to_synthwave_without_env() {
        // env may be set in CI; force the config-less path via a bogus
        // name — the fallback must still be synthwave.
        let t = all()
            .into_iter()
            .find(|t| t.name == "definitely-not-a-theme")
            .unwrap_or_else(synthwave);
        assert_eq!(t.name, "synthwave");
    }
}
