//! End-to-end tests for the vim engine: key sequences in vim notation
//! (`dw`, `ciwfoo<Esc>`, `<C-r>`) fed to a buffer written with `|`
//! marking the cursor, checked against the expected buffer + cursor.

use super::*;

/// Parses vim key notation: plain chars, plus `<Esc> <CR> <BS> <Del>
/// <Left> <Right> <Up> <Down> <Home> <End> <lt>` and `<C-x>` chords. A `<`
/// that doesn't start one of those is just the `<` key.
fn keys(s: &str) -> Vec<(KeyCode, KeyModifiers)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(len) = chars[i..].iter().position(|&c| c == '>') {
                let name: String = chars[i + 1..i + len].iter().collect();
                if let Some(key) = named_key(&name) {
                    out.push(key);
                    i += len + 1;
                    continue;
                }
            }
        }
        out.push((KeyCode::Char(chars[i]), KeyModifiers::NONE));
        i += 1;
    }
    out
}

fn named_key(name: &str) -> Option<(KeyCode, KeyModifiers)> {
    let none = KeyModifiers::NONE;
    Some(match name {
        "Esc" => (KeyCode::Esc, none),
        "CR" => (KeyCode::Enter, none),
        "BS" => (KeyCode::Backspace, none),
        "Del" => (KeyCode::Delete, none),
        "Left" => (KeyCode::Left, none),
        "Right" => (KeyCode::Right, none),
        "Up" => (KeyCode::Up, none),
        "Down" => (KeyCode::Down, none),
        "Home" => (KeyCode::Home, none),
        "End" => (KeyCode::End, none),
        "lt" => (KeyCode::Char('<'), none),
        _ => {
            let rest = name.strip_prefix("C-")?;
            let mut it = rest.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            (KeyCode::Char(c), KeyModifiers::CONTROL)
        }
    })
}

/// A buffer from `text`, with `|` marking the cursor (default: start).
fn buffer(text: &str) -> TextArea {
    let mut area = TextArea {
        lines: Vec::new(),
        cursor_row: 0,
        cursor_col: 0,
    };
    for (row, line) in text.split('\n').enumerate() {
        match line.find('|') {
            Some(byte) => {
                area.cursor_row = row;
                area.cursor_col = line[..byte].chars().count();
                area.lines.push(line.replacen('|', "", 1));
            }
            None => area.lines.push(line.to_string()),
        }
    }
    area
}

/// The buffer as text with `|` at the cursor — the inverse of `buffer`.
fn show(area: &TextArea) -> String {
    area.lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            if row == area.cursor_row {
                let idx = crate::editable::byte_index(line, area.cursor_col);
                format!("{}|{}", &line[..idx], &line[idx..])
            } else {
                line.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Ed {
    vim: Vim,
    area: TextArea,
    outcomes: Vec<Outcome>,
}

impl Ed {
    fn new(text: &str) -> Self {
        Self {
            vim: Vim::new(),
            area: buffer(text),
            outcomes: Vec::new(),
        }
    }

    fn feed(&mut self, s: &str) -> &mut Self {
        for (code, mods) in keys(s) {
            let outcome = self.vim.handle_key(&mut self.area, code, mods);
            if outcome != Outcome::None {
                self.outcomes.push(outcome);
            }
        }
        self
    }

    fn show(&self) -> String {
        show(&self.area)
    }
}

/// Starts in Normal mode on `start`, feeds `keys`, expects `expected`.
#[track_caller]
fn check(start: &str, keys: &str, expected: &str) {
    let mut ed = Ed::new(start);
    ed.feed(keys);
    assert_eq!(ed.show(), expected, "keys {keys:?} on {start:?}");
}

// -- the test helpers themselves --------------------------------------------

#[test]
fn key_notation_parses_named_keys_chords_and_literals() {
    assert_eq!(
        keys("a<Esc>"),
        vec![
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE)
        ]
    );
    assert_eq!(keys("<C-r>"), vec![(KeyCode::Char('r'), KeyModifiers::CONTROL)]);
    assert_eq!(
        keys("<x"),
        vec![
            (KeyCode::Char('<'), KeyModifiers::NONE),
            (KeyCode::Char('x'), KeyModifiers::NONE)
        ]
    );
    assert_eq!(keys("<lt>"), vec![(KeyCode::Char('<'), KeyModifiers::NONE)]);
}

#[test]
fn buffer_and_show_round_trip() {
    for text in ["|", "a|b", "x\n  y|z\nw", "grü|ße"] {
        assert_eq!(show(&buffer(text)), text);
    }
}

// -- motions -------------------------------------------------------------------

#[test]
fn hjkl_and_arrows() {
    check("a|bc", "l", "ab|c");
    check("a|bc", "h", "|abc");
    check("a|bc", "<Right><Left><Left>", "|abc");
    check("|abc", "h", "|abc");
    check("ab|c", "l", "ab|c");
    check("|a\nb", "j", "a\n|b");
    check("a\n|b", "k", "|a\nb");
    check("|a\nb", "k", "|a\nb");
    check("|abcd", "3l", "abc|d");
}

#[test]
fn vertical_motions_keep_the_desired_column() {
    check("abc|d\nx\nabcdef", "jj", "abcd\nx\nabc|def");
    check("ab|c\nabcdef", "$j", "abc\nabcde|f");
    check("abc|def\nx\nabcdef", "jhj", "abcdef\nx\n|abcdef");
}

#[test]
fn desired_column_resets_after_focus_changes() {
    let mut ed = Ed::new("abcd\nabc|d");
    ed.feed("k");
    assert_eq!(ed.show(), "abc|d\nabcd");
    ed.vim.leave(&ed.area);
    ed.area.cursor_col = 1;
    ed.vim.enter_normal(&mut ed.area);
    ed.feed("j");
    assert_eq!(
        ed.show(),
        "abcd\na|bcd",
        "a new vertical run starts from the actual column"
    );
}

#[test]
fn word_motions() {
    check("|hello world", "w", "hello |world");
    check("hello |world", "b", "|hello world");
    check("|hello world", "e", "hell|o world");
    check("|one two three", "2w", "one two |three");
    check("|foo.bar baz", "W", "foo.bar |baz");
    check("foo.bar |baz", "B", "|foo.bar baz");
    check("|foo.bar baz", "E", "foo.ba|r baz");
    check("|one\ntwo", "w", "one\n|two");
}

#[test]
fn line_motions() {
    check("  ab|c", "0", "|  abc");
    check("  ab|c", "^", "  |abc");
    check("a|bc", "$", "ab|c");
    check("|abc", "<End>", "ab|c");
    check("ab|c", "<Home>", "|abc");
}

#[test]
fn file_motions_with_and_without_counts() {
    check("|x\ny\nz", "G", "x\ny\n|z");
    check("|x\ny\nz", "2G", "x\n|y\nz");
    check("x\ny\n|z", "gg", "|x\ny\nz");
    check("x\ny\n|z", "2gg", "x\n|y\nz");
    check("|x\n  y", "G", "x\n  |y");
}

#[test]
fn find_char_motions_and_repeats() {
    check("|a-b-c", "f-", "a|-b-c");
    check("|a-b-c", "2f-", "a-b|-c");
    check("|a-b-c", "tc", "a-b|-c");
    check("|a-b-c", "f-;", "a-b|-c");
    check("|a-b-c", "f-;,", "a|-b-c");
    check("a-b-|c", "F-", "a-b|-c");
    check("a-b-|c", "2T-", "a-|b-c");
    check("|a-b-c", "fz", "|a-b-c");
    check("|a,b,c", "t,;", "a,|b,c");
}

#[test]
fn paragraph_and_bracket_motions() {
    check("|a\nb\n\nc", "}", "a\nb\n|\nc");
    check("|a\nb\n\nc", "}}", "a\nb\n\n|c");
    check("a\nb\n\n|c", "{", "a\nb\n|\nc");
    check("|f(a)", "%", "f(a|)");
    check("f(a|)", "%", "f|(a)");
}

#[test]
fn half_page_motions_move_ten_lines() {
    let text: String = (0..30).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    let mut ed = Ed::new(&format!("|{text}"));
    ed.feed("<C-d>");
    assert_eq!(ed.area.cursor_row, 10);
    ed.feed("<C-d><C-d><C-d>");
    assert_eq!(ed.area.cursor_row, 29, "clamps at the last line");
    ed.feed("<C-u>");
    assert_eq!(ed.area.cursor_row, 19);
}

#[test]
fn search_forward_backward_and_repeat() {
    check("|cat dog cat", "/cat<CR>", "cat dog |cat");
    check("|cat dog cat", "/cat<CR>n", "|cat dog cat");
    check("|cat dog cat", "/cat<CR>N", "|cat dog cat");
    check("|cat dog cat", "?cat<CR>", "cat dog |cat");
    check("|cat dog cat", "/cat<CR>/<CR>", "|cat dog cat");
}

#[test]
fn star_and_hash_search_the_word_under_the_cursor() {
    check("|foo bar foo", "*", "foo bar |foo");
    check("foo bar |foo", "#", "|foo bar foo");
    check("|foo foobar foo", "*", "foo foobar |foo");
}

#[test]
fn failed_searches_leave_the_cursor_and_report() {
    let mut ed = Ed::new("|abc");
    ed.feed("/zzz<CR>");
    assert_eq!(ed.show(), "|abc");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("Pattern not found: zzz".into())]);

    let mut ed = Ed::new("|abc");
    ed.feed("/<CR>");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("No previous search pattern".into())]);

    let mut ed = Ed::new("| .");
    ed.feed("*");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("No string under cursor".into())]);
}

// -- operators ---------------------------------------------------------------

#[test]
fn delete_with_word_motions() {
    check("|foo bar", "dw", "|bar");
    check("|a b c d e f g h", "2d3w", "|g h");
    check("|a b c d", "d2w", "|c d");
    check("|foo\nbar", "dw", "|\nbar");
    check("foo |bar", "db", "|bar");
    check("|foo bar", "de", "| bar");
    check("|foo bar", "dW", "|bar");
}

#[test]
fn delete_with_line_motions() {
    check("ab|cd", "d$", "a|b");
    check("ab|cd", "D", "a|b");
    check("ab|cd", "d0", "|cd");
    check("  ab|cd", "d^", "  |cd");
}

#[test]
fn delete_lines() {
    check("a\n|b\nc", "dd", "a\n|c");
    check("a\n|b", "dd", "|a");
    check("|only", "dd", "|");
    check("|a\nb\nc", "2dd", "|c");
    check("|a\nb\nc\nd", "d2j", "|d");
    check("a\n|b\nc", "dk", "|c");
    check("a\n|b\nc", "dG", "|a");
    check("a\n|b\nc", "dgg", "|c");
    check("|a\nb\n\nc", "d}", "|\nc");
}

#[test]
fn delete_with_find_and_bracket_motions() {
    check("|a-b-c", "df-", "|b-c");
    check("|a-b-c", "dtc", "|c");
    check("a-b-|c", "dF-", "a-b|c");
    check("x|(a)y", "d%", "x|y");
}

#[test]
fn text_objects_with_operators() {
    check("foo b|ar baz", "diw", "foo | baz");
    check("foo b|ar baz", "daw", "foo |baz");
    check("f(a, |b)", "di(", "f(|)");
    check("f(a, |b)x", "da(", "f|x");
    check("f(a, |b)", "dib", "f(|)");
    check("say \"h|i\" now", "di\"", "say \"|\" now");
    check("|a\nb\n\nc", "dip", "|\nc");
    check("|a\nb\n\nc", "dap", "|c");
    check("[1, |2]", "di[", "[|]");
    check("{a|b}", "da{", "|");
    check("x|y", "di(", "x|y");
}

#[test]
fn change_commands_enter_insert_mode() {
    check("foo b|ar baz", "ciwqux<Esc>", "foo qu|x baz");
    check("|foo bar", "cwnew<Esc>", "ne|w bar");
    check("fo|o bar", "cwX<Esc>", "fo|X bar");
    check("|foo bar baz", "c2wX<Esc>", "|X baz");
    check("|foo bar", "ceX<Esc>", "|X bar");
    check("ab|cd", "CX<Esc>", "ab|X");
    check("ab|cd", "c$X<Esc>", "ab|X");
    check("say \"h|i\" now", "ci\"yo<Esc>", "say \"y|o\" now");
    check("x\n  |abc\ny", "ccZ<Esc>", "x\n|Z\ny");
    check("x\n  |abc\ny", "SZ<Esc>", "x\n|Z\ny");
    check("a|bc", "sX<Esc>", "a|Xc");
    check("|", "sX<Esc>", "|X");
}

#[test]
fn change_leaves_the_editor_in_insert_mode() {
    let mut ed = Ed::new("|foo");
    ed.feed("cw");
    assert_eq!(ed.vim.mode, Mode::Insert);
    assert_eq!(ed.show(), "|");
}

#[test]
fn x_and_capital_x() {
    check("a|bc", "x", "a|c");
    check("|abcd", "3x", "|d");
    check("ab|c", "x", "a|b");
    check("|", "x", "|");
    check("ab|c", "X", "a|c");
    check("|abc", "X", "|abc");
    check("abc|d", "2X", "a|d");
}

#[test]
fn replace_join_and_toggle_case() {
    check("a|bc", "rX", "a|Xc");
    check("|abc", "2rX", "X|Xc");
    check("ab|c", "3rX", "ab|c");
    check("a|bc", "r<Esc>", "a|bc");
    check("|a\nb", "J", "a| b");
    check("|a\nb\nc", "3J", "a b| c");
    check("|only", "J", "|only");
    check("|abc", "~", "A|bc");
    check("|abc", "2~", "AB|c");
    check("a|b", "5~", "a|B");
}

#[test]
fn yank_and_put() {
    check("|a\nb", "yyp", "a\n|a\nb");
    check("a\n|b", "yyP", "a\n|b\nb");
    check("|a\nb", "Yp", "a\n|a\nb");
    check("|foo bar", "ywP", "foo| foo bar");
    check("|a b", "dwp", "ba| ");
    check("|ab", "xp", "b|a");
    check("|a\nb", "ddp", "b\n|a");
    check("|x", "yl3p", "xxx|x");
    check("|abc", "p", "|abc");
}

#[test]
fn yank_fills_the_register_without_changing_the_buffer() {
    let mut ed = Ed::new("|foo bar");
    ed.feed("yw");
    assert_eq!(ed.show(), "|foo bar");
    assert_eq!(
        ed.vim.register(),
        &Register {
            text: "foo ".into(),
            linewise: false
        }
    );

    let mut ed = Ed::new("|a\nb\nc");
    ed.feed("y2j");
    assert_eq!(
        ed.vim.register(),
        &Register {
            text: "a\nb\nc".into(),
            linewise: true
        }
    );
}

#[test]
fn deletes_fill_the_register_but_empty_deletes_do_not_clobber_it() {
    let mut ed = Ed::new("|abc\n");
    ed.feed("x");
    assert_eq!(ed.vim.register().text, "a");
    ed.feed("jx");
    assert_eq!(
        ed.vim.register().text,
        "a",
        "x on an empty line leaves the register alone"
    );
}

// -- insert mode -------------------------------------------------------------

#[test]
fn insert_entry_points() {
    check("a|b", "iX<Esc>", "a|Xb");
    check("a|b", "aX<Esc>", "ab|X");
    check("  a|b", "IX<Esc>", "  |Xab");
    check("|ab", "AX<Esc>", "ab|X");
    check("|a\nb", "oX<Esc>", "a\n|X\nb");
    check("a\n|b", "OX<Esc>", "a\n|X\nb");
    check("|", "aX<Esc>", "|X");
}

#[test]
fn insert_mode_editing_keys() {
    check("|", "ihello<C-w>world<Esc>", "worl|d");
    check("|", "iabc<C-u>x<Esc>", "|x");
    check("|", "iab<BS><Esc>", "|a");
    check("|", "ia<CR>b<Esc>", "a\n|b");
    check("a|bc", "i<Home>X<End>Y<Esc>", "Xabc|Y");
    check("a|bc", "i<Del><Esc>", "|ac");
    check("a|b", "i<C-h><Esc>", "|b");
}

#[test]
fn esc_moves_the_cursor_left_and_returns_to_normal() {
    let mut ed = Ed::new("|");
    ed.feed("iab");
    assert_eq!(ed.vim.mode, Mode::Insert);
    assert_eq!(ed.show(), "ab|");
    ed.feed("<Esc>");
    assert_eq!(ed.vim.mode, Mode::Normal);
    assert_eq!(ed.show(), "a|b");
}

#[test]
fn esc_in_normal_mode_is_harmless_and_cancels_a_pending_command() {
    check("|abc", "<Esc><Esc>", "|abc");
    check("|abc", "d<Esc>x", "|bc");
    check("|abc", "2<Esc>x", "|bc");
    let mut ed = Ed::new("|abc");
    ed.feed("<Esc>");
    assert!(ed.outcomes.is_empty(), "Esc in Normal mode never asks to quit");
}

#[test]
fn pending_keys_show_the_partial_command() {
    let mut ed = Ed::new("|abc");
    ed.feed("2d");
    assert_eq!(ed.vim.pending_keys(), "2d");
    ed.feed("i");
    assert_eq!(ed.vim.pending_keys(), "2di");
    ed.feed("<Esc>");
    assert_eq!(ed.vim.pending_keys(), "");
}

#[test]
fn unknown_keys_are_ignored_without_side_effects() {
    check("|abc", "Q", "|abc");
    check("|abc", "dQx", "|bc");
    check("|abc", "gQ", "|abc");
    check("|abc", "<C-x>", "|abc");
}

// -- undo, redo and repeat ------------------------------------------------------

#[test]
fn undo_and_redo_walk_the_history() {
    check("|a b c", "dwu", "|a b c");
    check("|a b c", "dwdwuu", "|a b c");
    check("|a b c", "dwdwuu<C-r>", "|b c");
    check("|a b c", "dwdwuu2<C-r>", "|c");
    check("|a b c", "dwdw2u", "|a b c");
}

#[test]
fn an_insert_session_is_a_single_undo_step() {
    check("|x", "ahello world<Esc>u", "|x");
    check("|x", "ahello<Esc>aworld<Esc>u", "xhell|o");
}

#[test]
fn undo_and_redo_report_the_ends_of_history() {
    let mut ed = Ed::new("|abc");
    ed.feed("u");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("Already at oldest change".into())]);
    ed.outcomes.clear();
    ed.feed("<C-r>");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("Already at newest change".into())]);
}

#[test]
fn a_new_change_clears_the_redo_history() {
    let mut ed = Ed::new("|abc");
    ed.feed("xux<C-r>");
    assert_eq!(ed.show(), "|bc");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("Already at newest change".into())]);
}

#[test]
fn motions_and_yanks_do_not_create_undo_steps() {
    let mut ed = Ed::new("|a b");
    ed.feed("wwbyw");
    ed.feed("u");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("Already at oldest change".into())]);
}

#[test]
fn undo_history_is_capped() {
    let mut ed = Ed::new("|");
    for _ in 0..(UNDO_LIMIT + 50) {
        ed.feed("ax<Esc>");
    }
    for _ in 0..UNDO_LIMIT {
        ed.feed("u");
    }
    assert_eq!(
        ed.area.lines[0].chars().count(),
        50,
        "only the last {UNDO_LIMIT} steps are kept"
    );
    ed.feed("u");
    assert_eq!(ed.outcomes, vec![Outcome::Notice("Already at oldest change".into())]);
}

#[test]
fn dot_repeats_the_last_change() {
    check("|a b c d", "dw.", "|c d");
    check("|abcd", "x..", "|d");
    check("|abcdefg", "x3.", "|efg");
    check("|foo bar", "ciwX<Esc>w.", "X |X");
    check("|a", "ob<Esc>.", "a\nb\n|b");
    check("|a\nb", "A!<Esc>j.", "a!\nb|!");
    check("|a", "ylp.", "aa|a");
    check("a|bc", "rX.", "a|Xc");
}

#[test]
fn dot_ignores_undo_yank_and_motions() {
    check("|abc", "xu.", "|bc");
    check("|abcd", "xyw.", "|cd");
    check("|abcd", "xl.", "b|d");
    check("|abc", ".", "|abc");
}

#[test]
fn a_repeated_change_is_one_undo_step() {
    check("|abcdef", "x3.u", "|bcdef");
}

// -- visual mode --------------------------------------------------------------

#[test]
fn visual_charwise_operations() {
    check("a|bcd", "vld", "a|d");
    check("|abc", "vlcX<Esc>", "|Xc");
    check("|abc", "vlU", "|ABc");
    check("|ABc", "vlu", "|abc");
    check("|aBc", "vl~", "|Abc");
    check("a|bcde", "vllohd", "|e");
    check("foo b|ar baz", "viwd", "foo | baz");
    check("|abc", "v2lx", "|");
}

#[test]
fn visual_line_operations() {
    check("a\n|b\nc", "Vd", "a\n|c");
    check("|a\nb\nc", "Vjd", "|c");
    check("|a\nb\nc", "VjjJ", "a b| c");
    check("|a\nb\n\nc", "vipd", "|\nc");
    check("|ab\ncd", "VjcX<Esc>", "|X");
}

#[test]
fn visual_yank_fills_the_register_and_returns_to_normal() {
    let mut ed = Ed::new("a|bcd");
    ed.feed("v$y");
    assert_eq!(
        ed.vim.register(),
        &Register {
            text: "bcd".into(),
            linewise: false
        }
    );
    assert_eq!(ed.vim.mode, Mode::Normal);
    assert_eq!(ed.show(), "a|bcd");

    let mut ed = Ed::new("|a\nb");
    ed.feed("Vjy");
    assert_eq!(
        ed.vim.register(),
        &Register {
            text: "a\nb".into(),
            linewise: true
        }
    );
}

#[test]
fn visual_mode_switching_and_escape() {
    let mut ed = Ed::new("|abc");
    ed.feed("v");
    assert_eq!(ed.vim.mode, Mode::Visual);
    ed.feed("V");
    assert_eq!(ed.vim.mode, Mode::VisualLine);
    ed.feed("v");
    assert_eq!(ed.vim.mode, Mode::Visual);
    ed.feed("v");
    assert_eq!(ed.vim.mode, Mode::Normal);
    ed.feed("vl<Esc>");
    assert_eq!(ed.vim.mode, Mode::Normal);
    assert_eq!(ed.show(), "a|bc", "Esc keeps the cursor where the selection ended");
}

#[test]
fn selection_on_row_reports_the_highlighted_columns() {
    let mut ed = Ed::new("a|bcd\nefg");
    assert_eq!(
        ed.vim.selection_on_row(&ed.area, 0),
        None,
        "no selection outside Visual mode"
    );
    ed.feed("vlj");
    assert_eq!(ed.vim.selection_on_row(&ed.area, 0), Some((1, 4)));
    assert_eq!(ed.vim.selection_on_row(&ed.area, 1), Some((0, 3)));
    ed.feed("<Esc>ggVj");
    assert_eq!(ed.vim.selection_on_row(&ed.area, 0), Some((0, 4)));
    assert_eq!(ed.vim.selection_on_row(&ed.area, 1), Some((0, 3)));
    assert_eq!(ed.vim.selection_on_row(&ed.area, 7), None);
}

// -- command line ---------------------------------------------------------------

#[test]
fn ex_commands_produce_outcomes() {
    let cases = [
        (":q<CR>", Outcome::Quit { force: false }),
        (":q!<CR>", Outcome::Quit { force: true }),
        (":wq<CR>", Outcome::Send),
        (":x<CR>", Outcome::Send),
        (
            ":w<CR>",
            Outcome::Notice("Drafts aren't saved yet — :wq sends, :q! discards".into()),
        ),
        (":foo<CR>", Outcome::Notice("Not an editor command: foo".into())),
    ];
    for (keys, expected) in cases {
        let mut ed = Ed::new("|abc");
        ed.feed(keys);
        assert_eq!(ed.outcomes, vec![expected], "{keys}");
        assert_eq!(ed.vim.mode, Mode::Normal, "{keys}");
    }
}

#[test]
fn ex_line_numbers_jump_to_that_line() {
    check("|a\nb\n  c", ":3<CR>", "a\nb\n  |c");
    check("|a\nb", ":99<CR>", "a\n|b");
    check("a\n|b", ":0<CR>", "|a\nb");
}

#[test]
fn command_line_editing_and_cancelling() {
    let mut ed = Ed::new("|abc");
    ed.feed(":");
    assert_eq!(ed.vim.mode, Mode::Command(CmdKind::Ex));
    ed.feed("ab<BS>");
    assert_eq!(ed.vim.command_line, "a");
    ed.feed("<BS><BS>");
    assert_eq!(ed.vim.mode, Mode::Normal, "Backspace on an empty command line cancels");

    let mut ed = Ed::new("|abc");
    ed.feed(":q<Esc>");
    assert_eq!(ed.vim.mode, Mode::Normal);
    assert!(ed.outcomes.is_empty(), "Esc abandons the command instead of running it");
    assert_eq!(CmdKind::SearchBackward.prompt(), '?');
}

// -- focus integration -----------------------------------------------------

#[test]
fn entering_and_leaving_keeps_an_insert_session_undoable() {
    let mut ed = Ed::new("|x");
    ed.vim.enter_insert(&ed.area);
    assert_eq!(ed.vim.mode, Mode::Insert);
    ed.feed("hi");
    ed.vim.leave(&ed.area);
    ed.vim.enter_normal(&mut ed.area);
    ed.feed("u");
    assert_eq!(ed.show(), "|x");
}

#[test]
fn enter_normal_clamps_a_past_the_end_cursor() {
    let mut ed = Ed::new("ab|");
    ed.vim.enter_normal(&mut ed.area);
    assert_eq!(ed.show(), "a|b");
}

// -- multibyte text -----------------------------------------------------------

#[test]
fn editing_multibyte_text_stays_on_char_boundaries() {
    check("|grüße aus köln", "wdw", "grüße |köln");
    check("grü|ße", "x", "grü|e");
    check("|äöü", "~~~", "ÄÖ|Ü");
    check("|grüße", "ciwhallo<Esc>", "hall|o");
    check("|über\nx", "yyjp", "über\nx\n|über");
}

// -- robustness -------------------------------------------------------------------

/// A tiny deterministic PRNG so the fuzz run is reproducible.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }
}

#[track_caller]
fn assert_invariants(ed: &Ed, context: &str) {
    let area = &ed.area;
    assert!(!area.lines.is_empty(), "buffer emptied: {context}");
    assert!(area.cursor_row < area.lines.len(), "row out of bounds: {context}");
    let len = area.lines[area.cursor_row].chars().count();
    let max = if ed.vim.mode == Mode::Insert {
        len
    } else {
        len.saturating_sub(1)
    };
    assert!(
        area.cursor_col <= max,
        "col {} past {max} in {:?}: {context}",
        area.cursor_col,
        ed.vim.mode
    );
}

#[test]
fn random_key_sequences_never_panic_or_break_invariants() {
    const ALPHABET: &[&str] = &[
        "h", "j", "k", "l", "w", "b", "e", "W", "B", "E", "0", "^", "$", "g", "G", "f", "F", "t", "T", ";", ",", "{",
        "}", "%", "x", "X", "d", "c", "y", "D", "C", "p", "P", "u", "<C-r>", "J", "~", "r", "s", "S", "i", "a", "I",
        "A", "o", "O", "v", "V", ".", "n", "N", "*", "#", ":", "/", "?", "q", "!", "2", "3", "<Esc>", "<CR>", "<BS>",
        "<C-w>", "<C-u>", "<C-d>", "ä", " ", "(", ")", "\"", "<Left>", "<Right>", "<Up>", "<Down>", "<Home>", "<End>",
        "<Del>", "Y", "p", "U", "o",
    ];
    const SAMPLES: &[&str] = &[
        "|",
        "|hello world",
        "|Dear team,\n\nthe (quick) \"brown\" fox.\n  Grüße, Ömer\n",
        "a\n\n\nb|",
        "f(a, [b], {c})",
    ];
    let mut rng = Lcg(0x5eed);
    for round in 0..400 {
        let start = SAMPLES[round % SAMPLES.len()];
        let mut ed = Ed::new(start);
        let mut history = String::new();
        for _ in 0..150 {
            let key = ALPHABET[rng.next() % ALPHABET.len()];
            history.push_str(key);
            ed.feed(key);
            assert_invariants(&ed, &format!("round {round}, start {start:?}, keys {history:?}"));
            // Keep pathological pastes from growing the buffer unboundedly.
            if ed.area.lines.iter().map(String::len).sum::<usize>() > 4_000 {
                ed.area = buffer(start);
            }
        }
    }
}
