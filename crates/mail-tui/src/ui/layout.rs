//! Soft-wrapping and cursor placement in terminal cells.
//!
//! Text is wrapped here rather than by `Paragraph::wrap`, so the same
//! computation decides both where text lands on screen and where the
//! cursor goes. Measuring in display width (not chars) keeps wide
//! characters (CJK, most emoji) in step. Positions are char indices into
//! a line, the unit `TextArea`/`TextInput` cursors use.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Display width of one char in terminal cells. Control characters count
/// as zero, matching how ratatui drops them when rendering.
fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Splits `text` into the char ranges `[start, end)` of each row it
/// occupies when soft-wrapped at `width` cells: breaks after the last
/// whitespace that fits, and hard-breaks a word longer than a whole row.
/// Always returns at least one row, so an empty line still takes one.
pub fn wrap_ranges(text: &str, width: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if width == 0 || chars.is_empty() {
        return vec![(0, chars.len())];
    }

    let mut rows = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut used = 0;
        let mut end = start;
        let mut last_break = None;
        while end < chars.len() {
            let w = char_width(chars[end]);
            if used + w > width {
                break;
            }
            used += w;
            if chars[end].is_whitespace() {
                last_break = Some(end + 1);
            }
            end += 1;
        }
        if end == chars.len() {
            rows.push((start, end));
            break;
        }
        let brk = match last_break {
            Some(b) if b > start => b,
            // No whitespace to break at: hard-break, taking at least one
            // char so a char wider than the whole row can't stall us.
            _ => end.max(start + 1),
        };
        rows.push((start, brk));
        start = brk;
    }
    rows
}

/// Where cursor `col` (a char index into `text`) lands among `rows` (from
/// `wrap_ranges`): `(row, x)` with `x` in cells. A cursor exactly at a
/// break point belongs to the start of the next row. A cursor past a row
/// that fills the full width (end of line, in Insert mode) is pinned to
/// the last cell rather than spilling onto the next line's first row.
pub fn locate(text: &str, rows: &[(usize, usize)], col: usize, width: usize) -> (usize, usize) {
    for (i, &(start, end)) in rows.iter().enumerate() {
        let last = i + 1 == rows.len();
        if col < end || last {
            let x: usize = text
                .chars()
                .skip(start)
                .take(col.min(end).saturating_sub(start))
                .map(char_width)
                .sum();
            return (i, x.min(width.saturating_sub(1)));
        }
    }
    (0, 0)
}

/// The chars `[start, end)` of a styled line, keeping each span's style —
/// one row's worth of a line wrapped by `wrap_ranges`.
pub fn slice_line(line: &Line<'static>, start: usize, end: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for span in &line.spans {
        let len = span.content.chars().count();
        let (span_start, span_end) = (offset, offset + len);
        offset = span_end;
        if span_end <= start || span_start >= end {
            continue;
        }
        let from = start.saturating_sub(span_start);
        let to = end.min(span_end) - span_start;
        let text: String = span.content.chars().skip(from).take(to - from).collect();
        spans.push(Span::styled(text, span.style));
    }
    Line::from(spans).style(line.style)
}

/// Soft-wraps every line in `lines` to `width`, keeping styles. The
/// returned rows are what's actually drawn, one per screen row, so their
/// count is the true scrollable height.
pub fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        for (start, end) in wrap_ranges(&text, width) {
            rows.push(slice_line(line, start, end));
        }
    }
    rows
}

/// For a single-line input `width` cells wide: the first char to draw and
/// the cursor's cell offset from there, scrolled just far enough that the
/// cursor stays visible once the value outgrows the field.
pub fn input_window(value: &str, cursor: usize, width: usize) -> (usize, usize) {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    if width == 0 {
        return (cursor, 0);
    }
    // Walk back from the cursor while the text before it still leaves a
    // cell for the cursor itself.
    let mut start = cursor;
    let mut used = 0;
    while start > 0 {
        let w = char_width(chars[start - 1]);
        if used + w > width - 1 {
            break;
        }
        used += w;
        start -= 1;
    }
    (start, used)
}

/// The tail of `value` starting at char `start` — what to draw for an
/// input scrolled by `input_window`.
pub fn from_char(value: &str, start: usize) -> &str {
    let idx = value.char_indices().nth(start).map(|(i, _)| i).unwrap_or(value.len());
    &value[idx..]
}

/// Expands tabs to the next multiple-of-8 column and drops other control
/// characters. ratatui silently drops control chars when rendering, which
/// collapses tab-indented mail (plain-text tables, code) into run-on text.
pub fn expand_tabs(line: &str) -> String {
    const TAB: usize = 8;
    let mut out = String::with_capacity(line.len());
    let mut col = 0;
    for c in line.chars() {
        if c == '\t' {
            let spaces = TAB - col % TAB;
            out.extend(std::iter::repeat_n(' ', spaces));
            col += spaces;
        } else if !c.is_control() {
            out.push(c);
            col += char_width(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Style};

    fn rows_text(text: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        wrap_ranges(text, width)
            .into_iter()
            .map(|(s, e)| chars[s..e].iter().collect())
            .collect()
    }

    #[test]
    fn a_line_that_fits_is_one_row() {
        assert_eq!(rows_text("hello", 10), vec!["hello"]);
        assert_eq!(wrap_ranges("", 10), vec![(0, 0)]);
    }

    #[test]
    fn wraps_after_the_last_space_that_fits() {
        assert_eq!(rows_text("hello world again", 11), vec!["hello ", "world again"]);
        assert_eq!(rows_text("hello world", 7), vec!["hello ", "world"]);
    }

    #[test]
    fn hard_breaks_a_word_longer_than_a_row() {
        assert_eq!(rows_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wide_characters_count_two_cells() {
        // Each of these is 2 cells wide: four of them fill 8 cells.
        assert_eq!(rows_text("日本語のテキスト", 8), vec!["日本語の", "テキスト"]);
        // A 2-cell char can't start in the last free cell of a row.
        assert_eq!(rows_text("a日b", 2), vec!["a", "日", "b"]);
    }

    #[test]
    fn locate_maps_a_cursor_onto_its_visual_row_and_cell() {
        let text = "hello world again";
        let rows = wrap_ranges(text, 11); // "hello " / "world again"
        assert_eq!(locate(text, &rows, 0, 11), (0, 0));
        assert_eq!(locate(text, &rows, 5, 11), (0, 5));
        assert_eq!(
            locate(text, &rows, 6, 11),
            (1, 0),
            "the break point starts the next row"
        );
        assert_eq!(locate(text, &rows, 8, 11), (1, 2));
    }

    #[test]
    fn locate_pins_an_end_of_line_cursor_on_a_full_row_to_the_last_cell() {
        let text = "abcd";
        let rows = wrap_ranges(text, 4);
        assert_eq!(locate(text, &rows, 4, 4), (0, 3));
    }

    #[test]
    fn locate_measures_wide_characters_in_cells() {
        let text = "ü日x";
        let rows = wrap_ranges(text, 20);
        assert_eq!(locate(text, &rows, 2, 20), (0, 3), "ü is 1 cell, 日 is 2");
    }

    #[test]
    fn slice_line_keeps_span_styles_across_a_split() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![Span::raw("ab"), Span::styled("cdef", bold), Span::raw("g")]);
        let row = slice_line(&line, 1, 4);
        let parts: Vec<(&str, Style)> = row.spans.iter().map(|s| (s.content.as_ref(), s.style)).collect();
        assert_eq!(parts, vec![("b", Style::new()), ("cd", bold)]);
    }

    #[test]
    fn wrap_lines_reconstructs_the_original_text() {
        let lines = vec![Line::from("one two three four"), Line::from(""), Line::from("five")];
        let rows = wrap_lines(&lines, 8);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["one two ", "three ", "four", "", "five"]);
    }

    #[test]
    fn input_window_scrolls_only_once_the_value_outgrows_the_field() {
        assert_eq!(input_window("short", 5, 10), (0, 5));
        // 12 chars, cursor at the end, 10-cell field: show the last 9 chars
        // plus a cell for the cursor.
        assert_eq!(input_window("abcdefghijkl", 12, 10), (3, 9));
        // Cursor back at the start: no scroll needed.
        assert_eq!(input_window("abcdefghijkl", 0, 10), (0, 0));
        assert_eq!(from_char("abcdefghijkl", 3), "defghijkl");
    }

    #[test]
    fn expand_tabs_aligns_to_tab_stops_and_drops_control_chars() {
        assert_eq!(expand_tabs("a\tb"), "a       b");
        assert_eq!(expand_tabs("\tx"), "        x");
        assert_eq!(expand_tabs("abcdefgh\ti"), "abcdefgh        i");
        assert_eq!(expand_tabs("bell\u{7}"), "bell");
    }
}
