//! Minimal hand-rolled text editing widgets for the compose view.
//!
//! `tui-textarea` would normally cover this, but its published version
//! still pulls in an older ratatui/crossterm than this project uses, so
//! two incompatible copies of both would end up in the dependency tree.
//! Compose only needs single-line fields plus one multi-line body, which
//! is little enough to own directly.

/// A single-line editable field (To/Cc/Bcc/Subject).
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub value: String,
    pub cursor: usize, // char index, not byte index
}

impl TextInput {
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn insert(&mut self, c: char) {
        let idx = byte_index(&self.value, self.cursor);
        self.value.insert(idx, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let idx = byte_index(&self.value, self.cursor);
        self.value.remove(idx);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }
}

/// A multi-line editable field (the message body).
#[derive(Debug, Clone)]
pub struct TextArea {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl Default for TextArea {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }
}

impl TextArea {
    pub fn insert(&mut self, c: char) {
        let idx = byte_index(&self.lines[self.cursor_row], self.cursor_col);
        self.lines[self.cursor_row].insert(idx, c);
        self.cursor_col += 1;
    }

    pub fn newline(&mut self) {
        let idx = byte_index(&self.lines[self.cursor_row], self.cursor_col);
        let rest = self.lines[self.cursor_row].split_off(idx);
        self.lines.insert(self.cursor_row + 1, rest);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            let idx = byte_index(&self.lines[self.cursor_row], self.cursor_col);
            self.lines[self.cursor_row].remove(idx);
        } else if self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    pub fn left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn right(&mut self) {
        let len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_row].chars().count());
        }
    }

    pub fn down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_row].chars().count());
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- TextInput ------------------------------------------------------------

    #[test]
    fn with_value_places_the_cursor_at_the_end() {
        let input = TextInput::with_value("hello");
        assert_eq!(input.value, "hello");
        assert_eq!(input.cursor, 5);
    }

    #[test]
    fn insert_and_backspace_operate_at_the_cursor() {
        let mut input = TextInput::default();
        input.insert('a');
        input.insert('c');
        input.left();
        input.insert('b');
        assert_eq!(input.value, "abc");
        assert_eq!(input.cursor, 2);

        input.backspace();
        assert_eq!(input.value, "ac");
        assert_eq!(input.cursor, 1);
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut input = TextInput::with_value("x");
        input.left();
        assert_eq!(input.cursor, 0);
        input.backspace();
        assert_eq!(input.value, "x");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn left_and_right_clamp_to_the_value_bounds() {
        let mut input = TextInput::with_value("ab");
        input.right();
        assert_eq!(input.cursor, 2, "cursor must not run past the end");
        input.left();
        input.left();
        input.left();
        assert_eq!(input.cursor, 0, "cursor must not go below 0");
    }

    #[test]
    fn insert_and_backspace_handle_multibyte_characters_correctly() {
        // "café" — é is 2 bytes in UTF-8, so a naive byte-index cursor
        // would panic or corrupt the string here.
        let mut input = TextInput::with_value("café");
        assert_eq!(input.cursor, 4);
        input.backspace();
        assert_eq!(input.value, "caf");
        input.insert('é');
        assert_eq!(input.value, "café");
    }

    // -- TextArea ---------------------------------------------------------------

    #[test]
    fn newline_splits_the_current_line_at_the_cursor() {
        let mut area = TextArea::default();
        for c in "hello".chars() {
            area.insert(c);
        }
        area.cursor_col = 2;
        area.newline();

        assert_eq!(area.lines, vec!["he".to_string(), "llo".to_string()]);
        assert_eq!(area.cursor_row, 1);
        assert_eq!(area.cursor_col, 0);
    }

    #[test]
    fn backspace_at_start_of_line_joins_it_with_the_previous_one() {
        let mut area = TextArea::default();
        for c in "ab".chars() {
            area.insert(c);
        }
        area.newline();
        for c in "cd".chars() {
            area.insert(c);
        }
        // area is now ["ab", "cd"] with the cursor at the end of "cd"
        area.cursor_col = 0;
        area.backspace();

        assert_eq!(area.lines, vec!["abcd".to_string()]);
        assert_eq!(area.cursor_row, 0);
        assert_eq!(area.cursor_col, 2, "cursor lands at the join point");
    }

    #[test]
    fn backspace_at_the_very_start_of_the_document_is_a_no_op() {
        let mut area = TextArea::default();
        area.backspace();
        assert_eq!(area.lines, vec!["".to_string()]);
        assert_eq!(area.cursor_row, 0);
        assert_eq!(area.cursor_col, 0);
    }

    #[test]
    fn up_and_down_clamp_the_column_to_the_shorter_lines_length() {
        let mut area = TextArea::default();
        for c in "long line".chars() {
            area.insert(c);
        }
        area.newline();
        area.insert('x');
        // area is now ["long line", "x"], cursor at end of "x" (col 1)
        area.up();
        assert_eq!(area.cursor_row, 0);
        assert_eq!(area.cursor_col, 1, "moving up keeps the column when the line above is long enough");

        area.cursor_col = "long line".chars().count();
        area.down();
        assert_eq!(area.cursor_row, 1);
        assert_eq!(area.cursor_col, 1, "moving down onto a shorter line clamps the column");
    }

    #[test]
    fn text_joins_lines_with_newlines() {
        let mut area = TextArea::default();
        for c in "ab".chars() {
            area.insert(c);
        }
        area.newline();
        for c in "cd".chars() {
            area.insert(c);
        }
        assert_eq!(area.text(), "ab\ncd");
    }

    #[test]
    fn right_wraps_to_the_start_of_the_next_line_at_end_of_line() {
        let mut area = TextArea::default();
        area.insert('a');
        area.newline();
        area.insert('b');
        area.cursor_row = 0;
        area.cursor_col = 1; // end of "a"

        area.right();

        assert_eq!(area.cursor_row, 1);
        assert_eq!(area.cursor_col, 0);
    }
}
