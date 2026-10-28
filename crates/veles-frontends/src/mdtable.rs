//! GFM table blocks for the terminal transcript.
//!
//! Keep the table as a table at every width: cells wrap independently,
//! then share one set of column starts. Inline styling and link spans
//! are produced by the same helpers as ordinary replies.

use super::{extract_links, inline_spans, locate_links, style_links, LinkSpan};
use ratatui::{
    style::{Modifier, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;
use veles_core::theme::Theme;

type RenderedRow = (Vec<Span<'static>>, Vec<LinkSpan>);

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone)]
struct CellLine {
    spans: Vec<Span<'static>>,
    links: Vec<LinkSpan>,
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Delimit pipes only outside code spans; an escaped pipe is content.
fn row_cells(row: &str) -> Vec<String> {
    let chars: Vec<char> = row.trim().chars().collect();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut code_ticks = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '|' | '\\') {
            cell.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '`' {
            let mut end = i + 1;
            while end < chars.len() && chars[end] == '`' {
                end += 1;
            }
            let n = end - i;
            if code_ticks == 0 {
                code_ticks = n;
            } else if code_ticks == n {
                code_ticks = 0;
            }
            cell.extend(chars[i..end].iter());
            i = end;
            continue;
        }
        if c == '|' && code_ticks == 0 {
            cells.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(c);
        }
        i += 1;
    }
    cells.push(cell.trim().to_string());
    if cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
}

fn separator_cells(row: &str) -> Option<Vec<Align>> {
    let cells = row_cells(row);
    if cells.is_empty() {
        return None;
    }
    cells
        .into_iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            let dashes = cell.trim_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => Align::Center,
                (_, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

fn looks_like_row(line: &str) -> bool {
    line.contains('|')
}

fn inline_cell(text: &str, th: &Theme) -> CellLine {
    let (clean, labels) = extract_links(text);
    let spans = inline_spans(&clean, th);
    let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let links = locate_links(&rendered, &labels);
    let spans = style_links(spans, 0, &links, 0, th);
    CellLine { spans, links }
}

fn cell_lines(source: &str, th: &Theme) -> Vec<CellLine> {
    static BR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let br = BR.get_or_init(|| regex::Regex::new(r"(?i)<br\s*/?>").unwrap());
    br.split(source)
        .map(|line| inline_cell(line.trim(), th))
        .collect()
}

fn wrap_cell_line(line: &CellLine, width: usize) -> Vec<CellLine> {
    #[derive(Clone)]
    struct Piece {
        text: String,
        style: Style,
        start: usize,
        end: usize,
    }
    let mut pieces = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        for (a, word) in text.match_indices(|c: char| !c.is_whitespace()) {
            let start = col + UnicodeWidthStr::width(&text[..a]);
            let end = start + UnicodeWidthStr::width(word);
            pieces.push(Piece {
                text: word.to_string(),
                style: span.style,
                start,
                end,
            });
        }
        col += UnicodeWidthStr::width(text);
    }
    let mut rows: Vec<Vec<Piece>> = vec![Vec::new()];
    for piece in pieces {
        let current_width = rows.last().unwrap().last().map_or(0, |p: &Piece| p.end);
        let needs_new = current_width > 0
            && piece.end - rows.last().unwrap().first().map_or(0, |p: &Piece| p.start) > width;
        if needs_new {
            rows.push(Vec::new());
        }
        let first = rows
            .last()
            .unwrap()
            .first()
            .map_or(piece.start, |p: &Piece| p.start);
        if piece.end - first > width && current_width > 0 {
            rows.push(Vec::new());
        }
        rows.last_mut().unwrap().push(piece);
    }
    rows.into_iter()
        .map(|pieces| {
            let origin = pieces.first().map_or(0, |p| p.start);
            let mut spans = Vec::new();
            let mut expected = origin;
            for piece in &pieces {
                if piece.start > expected {
                    spans.push(Span::raw(" ".repeat(piece.start - expected)));
                }
                spans.push(Span::styled(piece.text.clone(), piece.style));
                expected = piece.end;
            }
            let links = line
                .links
                .iter()
                .filter_map(|link| {
                    let start = link.start.max(origin);
                    let end = link.end.min(expected);
                    (start < end).then(|| LinkSpan {
                        start: start - origin,
                        end: end - origin,
                        url: link.url.clone(),
                    })
                })
                .collect();
            CellLine { spans, links }
        })
        .collect()
}

fn trim_trailing_spaces(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end_matches(' ').to_string();
        if trimmed.is_empty() {
            spans.pop();
        } else {
            let style = last.style;
            *last = Span::styled(trimmed, style);
            break;
        }
    }
}

fn render_row(
    cells: &[String],
    widths: &[usize],
    aligns: &[Align],
    th: &Theme,
    bold: bool,
) -> Vec<(Vec<Span<'static>>, Vec<LinkSpan>)> {
    let wrapped: Vec<Vec<CellLine>> = cells
        .iter()
        .enumerate()
        .map(|(c, source)| {
            cell_lines(source, th)
                .iter()
                .flat_map(|line| wrap_cell_line(line, widths[c]))
                .collect()
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let mut out = Vec::new();
    for row in 0..height {
        let mut spans = Vec::new();
        let mut links = Vec::new();
        let mut column = 0usize;
        for (c, cell) in wrapped.iter().enumerate() {
            if c > 0 {
                spans.push(Span::raw("  "));
                column += 2;
            }
            let empty = CellLine {
                spans: Vec::new(),
                links: Vec::new(),
            };
            let piece = cell.get(row).unwrap_or(&empty);
            let piece_width = spans_width(&piece.spans);
            let pad = widths[c].saturating_sub(piece_width);
            let before = match aligns[c] {
                Align::Right => pad,
                Align::Center => pad / 2,
                Align::Left => 0,
            };
            if before > 0 {
                spans.push(Span::raw(" ".repeat(before)));
                column += before;
            }
            for link in &piece.links {
                links.push(LinkSpan {
                    start: link.start + column,
                    end: link.end + column,
                    url: link.url.clone(),
                });
            }
            spans.extend(piece.spans.iter().cloned());
            column += piece_width;
            if c + 1 < widths.len() {
                let after = pad - before;
                spans.push(Span::raw(" ".repeat(after)));
                column += after;
            }
        }
        trim_trailing_spaces(&mut spans);
        if bold {
            for span in &mut spans {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
        }
        out.push((spans, links));
    }
    out
}

/// Parse and render the table at `start`, returning its rows and source
/// line count. A pipe run without a valid delimiter row is ordinary text.
pub(super) fn try_render(
    source: &[&str],
    start: usize,
    th: &Theme,
    table_width: usize,
) -> Option<(Vec<RenderedRow>, usize)> {
    let header = row_cells(source[start]);
    let aligns = separator_cells(source.get(start + 1)?)?;
    if header.is_empty() || header.len() != aligns.len() {
        return None;
    }
    let mut body = Vec::new();
    let mut i = start + 2;
    while i < source.len() && looks_like_row(source[i]) {
        body.push(row_cells(source[i]));
        i += 1;
    }
    let ncol = header.len();
    let mut all = vec![header.clone()];
    all.extend(body.iter().cloned());
    for row in &mut all {
        row.resize(ncol, String::new());
        row.truncate(ncol);
    }
    let mut natural = vec![0usize; ncol];
    for row in &all {
        for (c, text) in row.iter().enumerate() {
            natural[c] = natural[c].max(
                cell_lines(text, th)
                    .iter()
                    .map(|l| spans_width(&l.spans))
                    .max()
                    .unwrap_or(0),
            );
        }
    }
    // The natural header must stay a label: do not distribute all the
    // pressure to the widest prose column until the header wraps.
    let header_natural: Vec<usize> = all[0]
        .iter()
        .map(|text| {
            cell_lines(text, th)
                .iter()
                .map(|l| spans_width(&l.spans))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let gutters = 2 * (ncol - 1);
    // Fit if possible without mangling the header. Very narrow
    // terminals cannot fit every natural label; that is a bounded
    // overflow, not permission to make every cell one character wide.
    let minimum_total: usize = header_natural.iter().sum::<usize>() + gutters;
    let available = table_width.max(minimum_total);
    let mut widths = natural.clone();
    while widths.iter().sum::<usize>() + gutters > available {
        let candidate = widths
            .iter()
            .enumerate()
            .filter(|(i, w)| **w > header_natural[*i].max(1))
            .max_by_key(|(_, w)| **w)
            .map(|(c, w)| (c, *w));
        let Some((c, current)) = candidate else { break };
        let next = widths
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != c)
            .map(|(_, w)| *w)
            .max()
            .unwrap_or(1)
            .max(header_natural[c]);
        let shrink = (current - next.max(1))
            .max(1)
            .min(current - header_natural[c].max(1));
        widths[c] -= shrink;
    }
    let mut out = render_row(&all[0], &widths, &aligns, th, true);
    let rule = widths
        .iter()
        .enumerate()
        .map(|(c, width)| {
            let mut s = "─".repeat(*width);
            if c + 1 < ncol {
                s.push_str("─┼─");
            }
            s
        })
        .collect::<String>();
    out.push((
        vec![Span::styled(rule, Style::default().fg(th.dim()))],
        Vec::new(),
    ));
    for row in all.iter().skip(1) {
        out.extend(render_row(row, &widths, &aligns, th, false));
    }
    Some((out, i - start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_and_code_pipes_do_not_move_columns() {
        assert_eq!(row_cells(r"| a \| b | `x|y` |"), vec!["a | b", "`x|y`"]);
    }
}
