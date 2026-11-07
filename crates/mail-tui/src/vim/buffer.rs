//! Buffer-editing primitives the vim engine builds its commands from —
//! delete/yank/put over a `Range`, joining lines, case changes. They
//! operate on `TextArea` directly and keep its invariant of always
//! holding at least one line; cursor placement follows vim's rules for
//! each operation.

use crate::editable::{byte_index, TextArea};

use super::motion::{Pos, Range};

pub fn line_len(area: &TextArea, row: usize) -> usize {
    area.lines.get(row).map_or(0, |l| l.chars().count())
}

fn first_non_blank(line: &str) -> usize {
    line.chars().position(|c| !c.is_whitespace()).unwrap_or(0)
}

/// `range` with its endpoints clamped into the buffer and put in order,
/// so every primitive can trust it.
fn normalize(area: &TextArea, r: &Range) -> Range {
    let last_row = area.lines.len() - 1;
    let clamp = |p: Pos| {
        let row = p.row.min(last_row);
        Pos::new(row, p.col.min(line_len(area, row)))
    };
    let (a, b) = (clamp(r.start), clamp(r.end));
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    Range {
        start,
        end,
        linewise: r.linewise,
    }
}

/// The text `range` covers. Charwise ranges put `\n` between rows;
/// linewise ranges are the rows joined with `\n` (no trailing newline —
/// the register's `linewise` flag carries that instead).
pub fn range_text(area: &TextArea, range: &Range) -> String {
    let r = normalize(area, range);
    if r.linewise {
        return area.lines[r.start.row..=r.end.row].join("\n");
    }
    if r.start.row == r.end.row {
        let line = &area.lines[r.start.row];
        return line[byte_index(line, r.start.col)..byte_index(line, r.end.col)].to_string();
    }
    let first = &area.lines[r.start.row];
    let last = &area.lines[r.end.row];
    let mut out = first[byte_index(first, r.start.col)..].to_string();
    for line in &area.lines[r.start.row + 1..r.end.row] {
        out.push('\n');
        out.push_str(line);
    }
    out.push('\n');
    out.push_str(&last[..byte_index(last, r.end.col)]);
    out
}

/// Deletes `range`, returning what was removed. The cursor lands at the
/// range's start (charwise) or on the first non-blank of the line that
/// now occupies the first deleted row (linewise).
pub fn delete(area: &mut TextArea, range: &Range) -> String {
    let r = normalize(area, range);
    let removed = range_text(area, &r);
    if r.linewise {
        area.lines.drain(r.start.row..=r.end.row);
        if area.lines.is_empty() {
            area.lines.push(String::new());
        }
        area.cursor_row = r.start.row.min(area.lines.len() - 1);
        area.cursor_col = first_non_blank(&area.lines[area.cursor_row]);
        return removed;
    }
    let first = &area.lines[r.start.row];
    let head = first[..byte_index(first, r.start.col)].to_string();
    let last = &area.lines[r.end.row];
    let tail = last[byte_index(last, r.end.col)..].to_string();
    area.lines[r.start.row] = head + &tail;
    area.lines.drain(r.start.row + 1..=r.end.row);
    area.cursor_row = r.start.row;
    area.cursor_col = r.start.col;
    removed
}

/// `cc`/`S`/linewise `c`: replaces rows `first..=last` with one empty
/// line to type into, returning the removed lines' text.
pub fn change_lines(area: &mut TextArea, first: usize, last: usize) -> String {
    let r = normalize(area, &Range::lines(first, last));
    let removed = range_text(area, &r);
    area.lines.splice(r.start.row..=r.end.row, [String::new()]);
    area.cursor_row = r.start.row;
    area.cursor_col = 0;
    removed
}

/// Inserts `text` (which may contain newlines) at `at`, returning the
/// position just past the inserted text.
pub fn insert_text(area: &mut TextArea, at: Pos, text: &str) -> Pos {
    let row = at.row.min(area.lines.len() - 1);
    let col = at.col.min(line_len(area, row));
    let idx = byte_index(&area.lines[row], col);
    let parts: Vec<&str> = text.split('\n').collect();
    if parts.len() == 1 {
        area.lines[row].insert_str(idx, text);
        return Pos::new(row, col + text.chars().count());
    }
    let tail = area.lines[row].split_off(idx);
    area.lines[row].push_str(parts[0]);
    let mut new_rows: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    let last_len = new_rows.last().map_or(0, |s| s.chars().count());
    if let Some(last) = new_rows.last_mut() {
        last.push_str(&tail);
    }
    let count = new_rows.len();
    area.lines.splice(row + 1..row + 1, new_rows);
    Pos::new(row + count, last_len)
}

/// `p` (`after`) / `P`: puts `text` `count` times. Linewise text goes on
/// new lines below/above the cursor's line, with the cursor on the first
/// of them; charwise text goes after/at the cursor, with the cursor on
/// its last char (or its start, when it spans lines) — as in vim.
pub fn put(area: &mut TextArea, text: &str, linewise: bool, after: bool, count: usize) {
    let count = count.max(1);
    if linewise {
        let block: Vec<String> = text.split('\n').map(str::to_string).collect();
        let mut rows = Vec::with_capacity(block.len() * count);
        for _ in 0..count {
            rows.extend(block.iter().cloned());
        }
        let at = if after { area.cursor_row + 1 } else { area.cursor_row };
        let at = at.min(area.lines.len());
        area.lines.splice(at..at, rows);
        area.cursor_row = at;
        area.cursor_col = first_non_blank(&area.lines[at]);
        return;
    }
    if text.is_empty() {
        return;
    }
    let len = line_len(area, area.cursor_row);
    let col = if after && len > 0 {
        (area.cursor_col + 1).min(len)
    } else {
        area.cursor_col.min(len)
    };
    let at = Pos::new(area.cursor_row, col);
    let end = insert_text(area, at, &text.repeat(count));
    if text.contains('\n') {
        area.cursor_row = at.row;
        area.cursor_col = at.col;
    } else {
        area.cursor_row = end.row;
        area.cursor_col = end.col.saturating_sub(1);
    }
}

/// `J`: joins `row` with the `lines - 1` rows after it (so `lines` is the
/// total, at least 2). Each joined line loses its leading whitespace and
/// is separated by one space — none when the current line is empty or
/// already ends in whitespace, the joined text is empty, or it starts
/// with `)`. Returns `false` (and changes nothing) with no line to join.
pub fn join(area: &mut TextArea, row: usize, lines: usize) -> bool {
    if row + 1 >= area.lines.len() {
        return false;
    }
    let mut join_col = 0;
    for _ in 1..lines.max(2) {
        if row + 1 >= area.lines.len() {
            break;
        }
        let next = area.lines.remove(row + 1);
        let next = next.trim_start();
        let cur = &mut area.lines[row];
        join_col = cur.chars().count();
        let needs_space =
            !cur.is_empty() && !cur.ends_with(char::is_whitespace) && !next.is_empty() && !next.starts_with(')');
        if needs_space {
            cur.push(' ');
        }
        cur.push_str(next);
    }
    area.cursor_row = row;
    area.cursor_col = join_col;
    true
}

/// Rewrites every char in `range` through `f` (the case operators).
pub fn map_chars(area: &mut TextArea, range: &Range, f: impl Fn(char) -> String) {
    let r = normalize(area, range);
    for row in r.start.row..=r.end.row {
        let len = line_len(area, row);
        let (from, to) = if r.linewise {
            (0, len)
        } else {
            let from = if row == r.start.row { r.start.col } else { 0 };
            let to = if row == r.end.row { r.end.col } else { len };
            (from, to)
        };
        let line = &area.lines[row];
        let mapped: String = line
            .chars()
            .enumerate()
            .map(|(i, c)| if i >= from && i < to { f(c) } else { c.to_string() })
            .collect();
        area.lines[row] = mapped;
    }
}

pub fn toggle_case(c: char) -> String {
    if c.is_lowercase() {
        c.to_uppercase().collect()
    } else if c.is_uppercase() {
        c.to_lowercase().collect()
    } else {
        c.to_string()
    }
}

pub fn upper(c: char) -> String {
    c.to_uppercase().collect()
}

pub fn lower(c: char) -> String {
    c.to_lowercase().collect()
}

/// `r{ch}`: replaces `count` chars from `at` with `ch`, leaving the
/// cursor on the last one. Fails (changing nothing) when there aren't
/// `count` chars left on the line, like vim.
pub fn replace_chars(area: &mut TextArea, at: Pos, count: usize, ch: char) -> bool {
    let len = line_len(area, at.row);
    if count == 0 || at.col + count > len {
        return false;
    }
    let line = &area.lines[at.row];
    let replaced: String = line
        .chars()
        .enumerate()
        .map(|(i, c)| if i >= at.col && i < at.col + count { ch } else { c })
        .collect();
    area.lines[at.row] = replaced;
    area.cursor_row = at.row;
    area.cursor_col = at.col + count - 1;
    true
}

/// Insert-mode `Ctrl-w`: deletes the word (and any whitespace) before the
/// cursor; at the start of a line it joins with the previous one, like
/// Backspace.
pub fn delete_word_before(area: &mut TextArea) {
    if area.cursor_col == 0 {
        area.backspace();
        return;
    }
    let chars: Vec<char> = area.lines[area.cursor_row].chars().collect();
    let col = area.cursor_col.min(chars.len());
    let mut start = col;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if start > 0 {
        let word = super::motion::is_word_char(chars[start - 1]);
        while start > 0 && !chars[start - 1].is_whitespace() && super::motion::is_word_char(chars[start - 1]) == word {
            start -= 1;
        }
    }
    let row = area.cursor_row;
    delete(area, &Range::chars(Pos::new(row, start), Pos::new(row, col)));
}

/// Insert-mode `Ctrl-u`: deletes everything before the cursor on its line.
pub fn delete_to_line_start(area: &mut TextArea) {
    let row = area.cursor_row;
    let col = area.cursor_col;
    delete(area, &Range::chars(Pos::new(row, 0), Pos::new(row, col)));
}

/// Insert-mode Delete: removes the char under the cursor, or joins the
/// next line onto this one at the end of a line.
pub fn delete_forward(area: &mut TextArea) {
    let row = area.cursor_row;
    let col = area.cursor_col;
    if col < line_len(area, row) {
        delete(area, &Range::chars(Pos::new(row, col), Pos::new(row, col + 1)));
    } else if row + 1 < area.lines.len() {
        let next = area.lines.remove(row + 1);
        area.lines[row].push_str(&next);
        area.cursor_col = col;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(text: &str) -> TextArea {
        TextArea {
            lines: text.split('\n').map(str::to_string).collect(),
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    fn p(row: usize, col: usize) -> Pos {
        Pos::new(row, col)
    }

    #[test]
    fn range_text_reads_charwise_and_linewise_spans() {
        let a = area("hello\nwide\nworld");
        assert_eq!(range_text(&a, &Range::chars(p(0, 1), p(0, 4))), "ell");
        assert_eq!(range_text(&a, &Range::chars(p(0, 3), p(2, 2))), "lo\nwide\nwo");
        assert_eq!(range_text(&a, &Range::lines(1, 2)), "wide\nworld");
    }

    #[test]
    fn range_text_orders_and_clamps_its_endpoints() {
        let a = area("abc");
        assert_eq!(
            range_text(&a, &Range::chars(p(0, 2), p(0, 0))),
            "ab",
            "reversed endpoints"
        );
        assert_eq!(
            range_text(&a, &Range::chars(p(0, 1), p(0, 99))),
            "bc",
            "end past the line"
        );
    }

    #[test]
    fn delete_charwise_within_and_across_lines() {
        let mut a = area("hello world");
        assert_eq!(delete(&mut a, &Range::chars(p(0, 5), p(0, 11))), " world");
        assert_eq!(a.lines, vec!["hello"]);
        assert_eq!((a.cursor_row, a.cursor_col), (0, 5));

        let mut a = area("ab\ncd\nef");
        assert_eq!(delete(&mut a, &Range::chars(p(0, 1), p(2, 1))), "b\ncd\ne");
        assert_eq!(a.lines, vec!["af"]);
    }

    #[test]
    fn delete_linewise_keeps_at_least_one_line() {
        let mut a = area("  one\ntwo");
        delete(&mut a, &Range::lines(0, 0));
        assert_eq!(a.lines, vec!["two"]);

        let mut a = area("only");
        assert_eq!(delete(&mut a, &Range::lines(0, 0)), "only");
        assert_eq!(a.lines, vec![""], "an emptied buffer keeps one empty line");
    }

    #[test]
    fn delete_linewise_puts_the_cursor_on_the_first_non_blank_of_the_next_line() {
        let mut a = area("one\n   two\nthree");
        delete(&mut a, &Range::lines(0, 0));
        assert_eq!((a.cursor_row, a.cursor_col), (0, 3));
        let mut a = area("one\ntwo");
        delete(&mut a, &Range::lines(1, 1));
        assert_eq!(a.cursor_row, 0, "deleting the last line moves up");
    }

    #[test]
    fn delete_handles_multibyte_text() {
        let mut a = area("grüße");
        assert_eq!(delete(&mut a, &Range::chars(p(0, 2), p(0, 4))), "üß");
        assert_eq!(a.lines, vec!["gre"]);
    }

    #[test]
    fn change_lines_leaves_one_empty_line_to_type_into() {
        let mut a = area("a\nb\nc");
        assert_eq!(change_lines(&mut a, 0, 1), "a\nb");
        assert_eq!(a.lines, vec!["", "c"]);
        assert_eq!((a.cursor_row, a.cursor_col), (0, 0));
    }

    #[test]
    fn insert_text_single_and_multi_line() {
        let mut a = area("ad");
        assert_eq!(insert_text(&mut a, p(0, 1), "bc"), p(0, 3));
        assert_eq!(a.lines, vec!["abcd"]);

        let mut a = area("ad");
        assert_eq!(insert_text(&mut a, p(0, 1), "b\nc"), p(1, 1));
        assert_eq!(a.lines, vec!["ab", "cd"]);
    }

    #[test]
    fn put_charwise_after_and_before_the_cursor() {
        let mut a = area("ac");
        put(&mut a, "b", false, true, 1);
        assert_eq!(a.lines, vec!["abc"]);
        assert_eq!(a.cursor_col, 1, "cursor on the last pasted char");

        let mut a = area("bc");
        put(&mut a, "a", false, false, 3);
        assert_eq!(a.lines, vec!["aaabc"]);
        assert_eq!(a.cursor_col, 2);
    }

    #[test]
    fn put_charwise_into_an_empty_line_and_with_newlines() {
        let mut a = area("");
        put(&mut a, "x", false, true, 1);
        assert_eq!(a.lines, vec!["x"]);

        let mut a = area("ad");
        put(&mut a, "b\nc", false, true, 1);
        assert_eq!(a.lines, vec!["ab", "cd"]);
        assert_eq!(
            (a.cursor_row, a.cursor_col),
            (0, 1),
            "multi-line paste leaves the cursor at its start"
        );
    }

    #[test]
    fn put_linewise_below_and_above() {
        let mut a = area("one\nthree");
        put(&mut a, "two", true, true, 1);
        assert_eq!(a.lines, vec!["one", "two", "three"]);
        assert_eq!(a.cursor_row, 1);

        let mut a = area("b");
        put(&mut a, "  a", true, false, 2);
        assert_eq!(a.lines, vec!["  a", "  a", "b"]);
        assert_eq!(
            (a.cursor_row, a.cursor_col),
            (0, 2),
            "first non-blank of the first pasted line"
        );
    }

    #[test]
    fn join_applies_vims_spacing_rules() {
        let mut a = area("hello\n   world");
        assert!(join(&mut a, 0, 2));
        assert_eq!(a.lines, vec!["hello world"]);
        assert_eq!(a.cursor_col, 5);

        let mut a = area("a \nb");
        join(&mut a, 0, 2);
        assert_eq!(a.lines, vec!["a b"], "no extra space after trailing whitespace");

        let mut a = area("f(x\n)");
        join(&mut a, 0, 2);
        assert_eq!(a.lines, vec!["f(x)"], "no space before ')'");

        let mut a = area("\nb");
        join(&mut a, 0, 2);
        assert_eq!(a.lines, vec!["b"], "no space when joining onto an empty line");
    }

    #[test]
    fn join_with_a_count_and_at_the_last_line() {
        let mut a = area("a\nb\nc\nd");
        join(&mut a, 0, 3);
        assert_eq!(a.lines, vec!["a b c", "d"]);

        let mut a = area("last");
        assert!(!join(&mut a, 0, 2));
        assert_eq!(a.lines, vec!["last"]);
    }

    #[test]
    fn map_chars_changes_case_over_char_and_line_ranges() {
        let mut a = area("hello World");
        map_chars(&mut a, &Range::chars(p(0, 0), p(0, 5)), upper);
        assert_eq!(a.lines, vec!["HELLO World"]);
        map_chars(&mut a, &Range::lines(0, 0), toggle_case);
        assert_eq!(a.lines, vec!["hello wORLD"]);
        map_chars(&mut a, &Range::lines(0, 0), lower);
        assert_eq!(a.lines, vec!["hello world"]);
    }

    #[test]
    fn toggle_case_handles_multibyte_and_non_letters() {
        assert_eq!(toggle_case('ä'), "Ä");
        assert_eq!(toggle_case('Ö'), "ö");
        assert_eq!(toggle_case('1'), "1");
    }

    #[test]
    fn replace_chars_needs_enough_chars_on_the_line() {
        let mut a = area("abcd");
        assert!(replace_chars(&mut a, p(0, 1), 2, 'x'));
        assert_eq!(a.lines, vec!["axxd"]);
        assert_eq!(a.cursor_col, 2);
        assert!(!replace_chars(&mut a, p(0, 3), 2, 'y'), "only one char left");
        assert_eq!(a.lines, vec!["axxd"]);
    }

    #[test]
    fn delete_word_before_removes_whitespace_then_the_word() {
        let mut a = area("say hello  ");
        a.cursor_col = 11;
        delete_word_before(&mut a);
        assert_eq!(a.lines, vec!["say "]);
        delete_word_before(&mut a);
        assert_eq!(a.lines, vec![""]);
    }

    #[test]
    fn delete_word_before_stops_at_punctuation_and_joins_at_line_start() {
        let mut a = area("foo.bar");
        a.cursor_col = 7;
        delete_word_before(&mut a);
        assert_eq!(a.lines, vec!["foo."]);

        let mut a = area("a\nb");
        a.cursor_row = 1;
        delete_word_before(&mut a);
        assert_eq!(a.lines, vec!["ab"]);
    }

    #[test]
    fn delete_to_line_start_and_delete_forward() {
        let mut a = area("hello");
        a.cursor_col = 3;
        delete_to_line_start(&mut a);
        assert_eq!(a.lines, vec!["lo"]);
        assert_eq!(a.cursor_col, 0);

        let mut a = area("ab\ncd");
        a.cursor_col = 2;
        delete_forward(&mut a);
        assert_eq!(a.lines, vec!["abcd"], "Delete at end of line joins the next");
        delete_forward(&mut a);
        assert_eq!(a.lines, vec!["abd"]);
    }
}
