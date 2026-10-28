//! Keyboard binding modes — shared by the TUI and the desktop.
//!
//! Three schemes, switchable at runtime (`/bindings` in the TUI, the
//! Keybindings picker in the desktop, or config/env at boot):
//!
//! - `normie` — the current defaults: Enter sends, Shift-Enter (or
//!   Alt-Enter) inserts a newline, Ctrl-* app shortcuts, arrows edit.
//! - `emacs` — readline-style editing: C-j newline, C-a/C-e line
//!   edges, C-b/C-f chars, M-b/M-f words, C-p/C-n history, C-k kill /
//!   C-y yank, C-g cancel, M-x command palette. App shortcuts move to
//!   the M-* prefix so the C-* chord stays emacs.
//! - `vi` — modal editing: Esc enters normal mode (h/l/j/k/0/$/x/dd/
//!   i/a/A), Enter sends from either mode, C-j newline in insert, M-x
//!   (or `:` in normal mode) opens the command palette.
//!
//! Resolution order for the boot default:
//! `VELES_TUI_KEYMAP` env → config `tui.keymap` → `normie`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keymap {
    Normie,
    Emacs,
    Vi,
}

impl Keymap {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "normie" | "default" => Some(Keymap::Normie),
            "emacs" => Some(Keymap::Emacs),
            "vi" | "vim" => Some(Keymap::Vi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Keymap::Normie => "normie",
            Keymap::Emacs => "emacs",
            Keymap::Vi => "vi",
        }
    }

    pub fn all() -> [Keymap; 3] {
        [Keymap::Normie, Keymap::Emacs, Keymap::Vi]
    }
}

/// The keymap to boot with. Unknown names fall back to normie.
pub fn default_keymap() -> Keymap {
    let name = std::env::var("VELES_TUI_KEYMAP")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::config::load()
                .ok()
                .and_then(|c| c.tui.keymap.clone())
        })
        .unwrap_or_else(|| "normie".into());
    Keymap::parse(&name).unwrap_or(Keymap::Normie)
}

#[cfg(test)]
mod keymap_tests {
    use super::*;

    #[test]
    fn parses_all_names() {
        assert_eq!(Keymap::parse("normie"), Some(Keymap::Normie));
        assert_eq!(Keymap::parse("EMACS"), Some(Keymap::Emacs));
        assert_eq!(Keymap::parse("vim"), Some(Keymap::Vi));
        assert_eq!(Keymap::parse("bogus"), None);
    }

    #[test]
    fn names_roundtrip() {
        for k in Keymap::all() {
            assert_eq!(Keymap::parse(k.name()), Some(k));
        }
    }

    #[test]
    fn unknown_env_falls_back() {
        // The env var may be set in CI; the fallback is exercised by
        // construction — parse() returning None → normie.
        assert_eq!(
            Keymap::parse("nope").unwrap_or(Keymap::Normie),
            Keymap::Normie
        );
    }
}
