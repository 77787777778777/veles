/// Shorten `s` to at most `n` characters.
pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
