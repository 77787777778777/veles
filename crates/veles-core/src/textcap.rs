//! Capping a `String` at a number of bytes without cutting a character.
//!
//! `String::truncate` panics when the byte it is given falls inside a
//! character, and in the gateway a panic aborts the process so that its
//! supervisor restarts it. Every cap on text that came from outside (a
//! tool's output, an HTTP body, a message a model wrote) is a cap on text
//! that may be in any script, so it goes through here. On 2026-11-14 the
//! compaction transcript's cap met a multi-byte character in a long
//! session and took the laptop's gateway down at every start.

/// Shorten `s` to at most `max_bytes` bytes, at the character boundary at
/// or before that byte. Returns whether anything was cut.
pub fn truncate_at_boundary(s: &mut String, max_bytes: usize) -> bool {
    if s.len() <= max_bytes {
        return false;
    }
    let mut cut = max_bytes;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cap_inside_a_character_cuts_before_it() {
        // "é" is two bytes, Gurmukhi three, an emoji four.
        for (text, cap, want) in [
            ("aéé", 2, "a"),
            ("aéé", 3, "aé"),
            ("ਪੰਜਾਬੀ", 4, "ਪ"),
            ("x🙂y", 3, "x"),
            ("x🙂y", 5, "x🙂"),
        ] {
            let mut s = text.to_string();
            assert!(truncate_at_boundary(&mut s, cap), "{text}");
            assert_eq!(s, want, "{text} at {cap}");
        }
    }

    #[test]
    fn text_within_the_cap_is_left_alone() {
        let mut s = "ਪੰਜਾਬੀ".to_string();
        let len = s.len();
        assert!(!truncate_at_boundary(&mut s, len));
        assert_eq!(s, "ਪੰਜਾਬੀ");
        let mut ascii = "abcdef".to_string();
        assert!(truncate_at_boundary(&mut ascii, 3));
        assert_eq!(ascii, "abc");
        let mut empty_cap = "é".to_string();
        assert!(truncate_at_boundary(&mut empty_cap, 0));
        assert_eq!(empty_cap, "");
    }
}
