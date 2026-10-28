use crate::shapes::area;

/// One-line summary of a rectangle, for the CLI report.
pub fn describe(w: u64, h: u64) -> String {
    format!("{w}x{h} = {}", area(w, h))
}
