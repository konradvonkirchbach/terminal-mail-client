use std::collections::HashMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::app::{ComposeField, ComposeState};
use crate::attach;
use crate::editable::byte_index;
use crate::spellcheck::{self, normalize_word};
use crate::theme::Theme;
use crate::vim::Mode as VimMode;

use super::layout;

const LABEL_WIDTH: usize = 9; // fits "Subject:" (8) plus a trailing space

pub fn draw(frame: &mut Frame, area: Rect, compose: &ComposeState, theme: &Theme) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // header block: 5 fields + top/bottom border
            Constraint::Min(3),    // body
            Constraint::Length(1), // help/status line
        ])
        .split(area);

    draw_header(frame, rows[0], compose, theme);
    draw_body(frame, rows[1], compose, theme);
    draw_help(frame, rows[2], compose, theme);
    // Drawn last so it floats over the body, since it can be taller than
    // the header block it hangs off of.
    draw_suggestions(frame, rows[0], compose, theme);
}

fn field_line(label: &str, value: &str, focused: bool, theme: &Theme) -> Line<'static> {
    let label_style = if focused {
        Style::new().fg(theme.accent)
    } else {
        Style::new().fg(theme.muted)
    };
    Line::from(vec![
        Span::styled(format!("{label:<LABEL_WIDTH$}"), label_style),
        Span::styled(value.to_string(), Style::new().fg(theme.foreground)),
    ])
}

fn attachments_line(compose: &ComposeState, theme: &Theme) -> Line<'static> {
    let focused = compose.focus == ComposeField::Attachments;
    let label_style = if focused {
        Style::new().fg(theme.accent)
    } else {
        Style::new().fg(theme.muted)
    };
    let mut spans = vec![Span::styled(format!("{:<LABEL_WIDTH$}", "Files:"), label_style)];

    if compose.attachments.items.is_empty() {
        spans.push(Span::styled("(none — Ctrl+A to attach)", Style::new().fg(theme.muted)));
    } else {
        for (i, a) in compose.attachments.items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let text = format!("{} ({})", a.filename, attach::human_size(a.size_bytes));
            let style = if focused && i == compose.attachments.selected {
                Style::new().bg(theme.selection).fg(theme.bright_foreground)
            } else {
                Style::new().fg(theme.foreground)
            };
            spans.push(Span::styled(text, style));
        }
    }

    Line::from(spans)
}

fn draw_header(frame: &mut Frame, area: Rect, compose: &ComposeState, theme: &Theme) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(" compose ", Style::new().fg(theme.bright_foreground)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // A focused field longer than the pane scrolls horizontally so its
    // cursor stays visible; unfocused ones just show their start.
    let field_width = (inner.width as usize).saturating_sub(LABEL_WIDTH);
    let fields = [
        ("To:", &compose.to, ComposeField::To),
        ("Cc:", &compose.cc, ComposeField::Cc),
        ("Bcc:", &compose.bcc, ComposeField::Bcc),
        ("Subject:", &compose.subject, ComposeField::Subject),
    ];
    let mut lines = Vec::with_capacity(fields.len() + 1);
    let mut cursor = None;
    for (row, (label, input, field)) in fields.into_iter().enumerate() {
        let focused = compose.focus == field;
        let shown = if focused {
            let (start, x) = layout::input_window(&input.value, input.cursor, field_width);
            cursor = Some((row as u16, x as u16));
            layout::from_char(&input.value, start)
        } else {
            input.value.as_str()
        };
        lines.push(field_line(label, shown, focused, theme));
    }
    lines.push(attachments_line(compose, theme));
    frame.render_widget(Paragraph::new(lines), inner);

    if let Some((row, x)) = cursor {
        frame.set_cursor_position((inner.x + LABEL_WIDTH as u16 + x, inner.y + row));
    }
}

/// Computes the rect for a floating list popup anchored with its
/// top-left corner at `(x, y)`, wide enough for `max_content_chars` of
/// text (clamped to `min_width..=max_width`) and tall enough for
/// `item_count` items, both clamped to fit on screen — shared by
/// `draw_suggestions` and `draw_misspelling_suggestions` so a future fix
/// to this clamping only has to happen once. `None` when there isn't
/// enough room left to draw anything useful.
fn popup_rect(
    frame_area: Rect,
    x: u16,
    y: u16,
    max_content_chars: u16,
    min_width: u16,
    max_width: u16,
    item_count: usize,
) -> Option<Rect> {
    let content_width = max_content_chars.clamp(min_width, max_width);
    let width = (content_width + 2).min(frame_area.width.saturating_sub(x));
    let height = (item_count as u16 + 2).min(frame_area.height.saturating_sub(y));
    if width < 3 || height < 3 {
        return None; // no room to draw it without corrupting the layout
    }
    Some(Rect { x, y, width, height })
}

/// A fuzzy-matched sender dropdown, floating just under whichever
/// recipient field is focused. Deliberately drawn over the body rather
/// than shrinking it — this is a transient overlay, not part of the
/// layout.
fn draw_suggestions(frame: &mut Frame, header_area: Rect, compose: &ComposeState, theme: &Theme) {
    if compose.suggestions.items.is_empty() {
        return;
    }
    let Some((row, _)) = focused_input_row(compose) else {
        return;
    };

    let inner = Block::new().borders(Borders::ALL).inner(header_area);
    let x = inner.x + LABEL_WIDTH as u16;
    let y = inner.y + row + 1;

    let max_content_chars = compose
        .suggestions
        .items
        .iter()
        .map(|a| a.to_string().chars().count() as u16)
        .max()
        .unwrap_or(10);
    let Some(popup) = popup_rect(
        frame.area(),
        x,
        y,
        max_content_chars,
        10,
        50,
        compose.suggestions.items.len(),
    ) else {
        return;
    };

    let items: Vec<ListItem> = compose
        .suggestions
        .items
        .iter()
        .enumerate()
        .map(|(i, addr)| {
            let style = if i == compose.suggestions.selected {
                Style::new().bg(theme.selection).fg(theme.bright_foreground)
            } else {
                Style::new().fg(theme.foreground)
            };
            ListItem::new(addr.to_string()).style(style)
        })
        .collect();
    let list = List::new(items).block(
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::new().fg(theme.accent))
            .style(Style::new().bg(theme.background)),
    );

    frame.render_widget(Clear, popup);
    frame.render_widget(list, popup);
}

fn focused_input_row(compose: &ComposeState) -> Option<(u16, &crate::editable::TextInput)> {
    match compose.focus {
        ComposeField::To => Some((0, &compose.to)),
        ComposeField::Cc => Some((1, &compose.cc)),
        ComposeField::Bcc => Some((2, &compose.bcc)),
        ComposeField::Subject => Some((3, &compose.subject)),
        ComposeField::Attachments | ComposeField::Body => None,
    }
}

fn draw_body(frame: &mut Frame, area: Rect, compose: &ComposeState, theme: &Theme) {
    let focused = compose.focus == ComposeField::Body;
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(if focused { theme.accent } else { theme.muted }))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(
            match &compose.spellcheck_note {
                Some(note) => format!(" body — {note} "),
                None => " body ".to_string(),
            },
            Style::new().fg(theme.bright_foreground),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let flagged_style = Style::new().fg(theme.red).add_modifier(Modifier::UNDERLINED);
    let selection_style = Style::new().bg(theme.selection).fg(theme.bright_foreground);
    let no_misspellings = HashMap::new();

    // Wrapped here (not by `Paragraph::wrap`) so the cursor and scroll are
    // computed in the same visual rows the text is actually drawn in — a
    // logical line wider than the pane would otherwise push the cursor
    // past the border and every later line off its expected row.
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cursor = (0, 0);
    for (i, l) in compose.body.lines.iter().enumerate() {
        let misspelled = compose.misspelled_by_line.get(i).unwrap_or(&no_misspellings);
        let line = spellcheck_line(l, misspelled, flagged_style);
        let line = match compose.vim.selection_on_row(&compose.body, i) {
            // A selected empty line still shows as one highlighted cell.
            Some(_) if l.is_empty() => Line::from(Span::styled(" ", selection_style)),
            Some((start, end)) => overlay_style(line, start, end, selection_style),
            None => line,
        };
        if l.is_empty() {
            if i == compose.body.cursor_row {
                cursor = (rows.len(), 0);
            }
            rows.push(line);
            continue;
        }
        let ranges = layout::wrap_ranges(l, width);
        if i == compose.body.cursor_row {
            let (row, x) = layout::locate(l, &ranges, compose.body.cursor_col, width);
            cursor = (rows.len() + row, x);
        }
        rows.extend(
            ranges
                .into_iter()
                .map(|(start, end)| layout::slice_line(&line, start, end)),
        );
    }

    // Keep the cursor's row on screen, pinned to the bottom edge once it
    // goes past it.
    let visible_height = inner.height.max(1) as usize;
    let scroll_y = cursor.0.saturating_sub(visible_height - 1);
    let paragraph = Paragraph::new(Text::from(rows)).scroll((scroll_y as u16, 0));
    frame.render_widget(paragraph, inner);

    // In Command mode the cursor belongs to the command line (`draw_help`).
    if focused && !matches!(compose.vim.mode, VimMode::Command(_)) {
        let y = (cursor.0 - scroll_y) as u16;
        let x = cursor.1 as u16;
        frame.set_cursor_position((inner.x + x, inner.y + y));
        if matches!(compose.vim.mode, VimMode::Normal | VimMode::Insert) {
            draw_misspelling_suggestions(frame, inner, (x, y), compose, theme);
        }
    }
}

/// Patches `style` onto chars `[start, end)` of `line`, splitting spans at
/// those char boundaries — so a Visual selection layers over the line's
/// existing styling (misspelling underlines stay visible) instead of
/// replacing it.
fn overlay_style(line: Line<'static>, start: usize, end: usize, style: Style) -> Line<'static> {
    if start >= end {
        return line;
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut offset = 0;
    for span in line.spans {
        let len = span.content.chars().count();
        let (span_start, span_end) = (offset, offset + len);
        offset = span_end;
        if span_end <= start || span_start >= end {
            out.push(span);
            continue;
        }
        let content = span.content.as_ref();
        let from = byte_index(content, start.saturating_sub(span_start));
        let to = byte_index(content, (end - span_start).min(len));
        if from > 0 {
            out.push(Span::styled(content[..from].to_string(), span.style));
        }
        out.push(Span::styled(content[from..to].to_string(), span.style.patch(style)));
        if to < content.len() {
            out.push(Span::styled(content[to..].to_string(), span.style));
        }
    }
    Line::from(out)
}

/// A read-only "did you mean?" popup listing Hunspell's suggested
/// corrections for whichever misspelled word the cursor is currently on,
/// floating just below the cursor's on-screen row. Not interactive —
/// unlike the recipient-suggestion dropdown, the body already claims
/// every plausible accept-key (arrows move the cursor, Enter inserts a
/// newline, Tab switches fields), so this is purely informational.
fn draw_misspelling_suggestions(
    frame: &mut Frame,
    body_inner: Rect,
    (cursor_x, cursor_y): (u16, u16),
    compose: &ComposeState,
    theme: &Theme,
) {
    let Some((word, suggestions)) = compose.misspelled_word_at_cursor() else {
        return;
    };
    if suggestions.is_empty() {
        return;
    }

    let x = body_inner.x + cursor_x.min(body_inner.width.saturating_sub(1));
    let y = body_inner.y + cursor_y + 1;

    let max_content_chars = suggestions.iter().map(|s| s.chars().count() as u16).max().unwrap_or(10);
    let Some(popup) = popup_rect(frame.area(), x, y, max_content_chars, 10, 40, suggestions.len()) else {
        return;
    };

    let items: Vec<ListItem> = suggestions.iter().map(|s| ListItem::new(s.clone())).collect();
    let list = List::new(items).block(
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::new().fg(theme.red))
            .style(Style::new().bg(theme.background).fg(theme.foreground))
            .title(Span::styled(
                format!(" {word}? "),
                Style::new().fg(theme.bright_foreground),
            )),
    );

    frame.render_widget(Clear, popup);
    frame.render_widget(list, popup);
}

fn draw_help(frame: &mut Frame, area: Rect, compose: &ComposeState, theme: &Theme) {
    let muted = Style::new().fg(theme.muted);
    let body_focused = compose.focus == ComposeField::Body;
    let text = if compose.sending {
        Line::from(Span::styled("Sending...", Style::new().fg(theme.accent)))
    } else if let (true, VimMode::Command(kind)) = (body_focused, compose.vim.mode) {
        let line = format!("{}{}", kind.prompt(), compose.vim.command_line);
        frame.set_cursor_position((area.x + line.chars().count() as u16, area.y));
        Line::from(Span::styled(line, Style::new().fg(theme.bright_foreground)))
    } else if let Some(notice) = &compose.notice {
        Line::from(Span::styled(notice.clone(), Style::new().fg(theme.yellow)))
    } else if let Some(err) = &compose.error {
        Line::from(Span::styled(format!("Send failed: {err}"), Style::new().fg(theme.red)))
    } else if !compose.suggestions.items.is_empty() {
        Line::from(Span::styled(
            "\u{2191}/\u{2193} choose suggestion   Enter/Tab accept   Esc dismiss",
            muted,
        ))
    } else if body_focused {
        let (label, hint) = body_mode_text(compose.vim.mode, compose.vim.pending_keys());
        let mut spans = Vec::new();
        if !label.is_empty() {
            spans.push(Span::styled(
                label,
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(hint, muted));
        Line::from(spans)
    } else {
        let hint = match compose.focus {
            ComposeField::Attachments if !compose.attachments.items.is_empty() => {
                "Ctrl+A attach   Backspace remove   Tab/Shift+Tab move field   Ctrl+S send   Esc \u{2192} body"
            }
            _ => "Tab/Shift+Tab move field   Ctrl+A attach   Ctrl+S send   Esc \u{2192} body",
        };
        Line::from(Span::styled(hint, muted))
    };
    frame.render_widget(Paragraph::new(text).style(Style::new().bg(theme.background)), area);
}

/// The body's status line as plain text: vim's mode indicator (or, in
/// Normal mode, the partly typed command, like vim's showcmd) and the
/// keys most worth knowing in that mode.
fn body_mode_text(mode: VimMode, pending: &str) -> (String, &'static str) {
    const VISUAL_HINT: &str = "d delete   y yank   c change   ~ u U case   Esc cancel";
    match mode {
        VimMode::Insert => (
            "-- INSERT --".to_string(),
            "Esc normal mode   Tab next field   Ctrl+S send",
        ),
        VimMode::Visual => ("-- VISUAL --".to_string(), VISUAL_HINT),
        VimMode::VisualLine => ("-- VISUAL LINE --".to_string(), VISUAL_HINT),
        VimMode::Normal | VimMode::Command(_) => (
            pending.to_string(),
            "i insert   v visual   u undo   :wq send   :q! discard   Tab next field",
        ),
    }
}

/// Renders one body line, underlining whichever whitespace-delimited
/// tokens match a misspelled word. Falls back to a single unstyled span
/// (inheriting the block's own colors) when nothing in the line is
/// flagged, which is the common case and keeps rendering cheap.
fn spellcheck_line(line: &str, misspelled: &HashMap<String, Vec<String>>, flagged: Style) -> Line<'static> {
    if misspelled.is_empty() {
        return Line::raw(line.to_string());
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last_end = 0;
    for (start, token) in spellcheck::tokens(line) {
        if misspelled.contains_key(&normalize_word(token)) {
            if start > last_end {
                spans.push(Span::raw(line[last_end..start].to_string()));
            }
            spans.push(Span::styled(token.to_string(), flagged));
            last_end = start + token.len();
        }
    }

    if spans.is_empty() {
        return Line::raw(line.to_string());
    }
    if last_end < line.len() {
        spans.push(Span::raw(line[last_end..].to_string()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editable::TextInput;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    /// Renders `compose` into a `width`x`height` test terminal and returns
    /// where the cursor ended up.
    fn rendered_cursor(compose: &ComposeState, width: u16, height: u16) -> Position {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::fallback();
        terminal.draw(|f| draw(f, f.area(), compose, &theme)).unwrap();
        terminal.get_cursor_position().unwrap()
    }

    #[test]
    fn a_long_body_line_wraps_and_the_cursor_follows_it_inside_the_box() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Body;
        compose.vim.enter_insert(&compose.body);
        for c in "word ".repeat(30).chars() {
            compose.body.insert(c);
        }

        // 40 wide -> 38-cell body: rows of 35 chars ("word " x7), so the
        // 150-char line takes 5 rows and the cursor sits 10 cells into the
        // last one. Header block is 7 rows; the body's inner area starts
        // at (1, 8).
        assert_eq!(rendered_cursor(&compose, 40, 20), Position::new(11, 12));
    }

    #[test]
    fn the_cursor_stays_on_screen_when_wrapped_rows_push_it_past_the_bottom() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Body;
        compose.vim.enter_insert(&compose.body);
        for _ in 0..6 {
            for c in "word ".repeat(30).chars() {
                compose.body.insert(c);
            }
            compose.body.newline();
        }

        // 6 lines x 5 visual rows each = 30+ rows in a 10-row body pane.
        let pos = rendered_cursor(&compose, 40, 20);
        assert!(
            (8..18).contains(&pos.y),
            "cursor row {} must be inside the body pane",
            pos.y
        );
        assert_eq!(pos.x, 1, "an empty last line puts the cursor at its start");
    }

    #[test]
    fn a_long_header_field_scrolls_so_the_cursor_stays_visible() {
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::To;
        compose.to = TextInput::with_value("a".repeat(100));

        let pos = rendered_cursor(&compose, 40, 20);
        assert!(pos.x < 39, "cursor x {} must stay inside the border", pos.x);
        assert_eq!(pos.y, 1);
    }

    /// A misspelled-word map with empty suggestion lists — `spellcheck_line`
    /// only ever checks key presence, so the tests exercising it don't
    /// need to populate the values.
    fn flagged(words: &[&str]) -> HashMap<String, Vec<String>> {
        words.iter().map(|w| (w.to_string(), Vec::new())).collect()
    }

    #[test]
    fn spellcheck_line_leaves_a_clean_line_as_one_unstyled_span() {
        let line = spellcheck_line("all good here", &HashMap::new(), Style::new());
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content.as_ref(), "all good here");
    }

    #[test]
    fn spellcheck_line_underlines_only_the_misspelled_token() {
        let misspelled = flagged(&["wrold"]);
        let flagged_style = Style::new().add_modifier(Modifier::UNDERLINED);
        let line = spellcheck_line("hello wrold today", &misspelled, flagged_style);

        let flagged_spans: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(flagged_spans, vec!["wrold"]);
    }

    #[test]
    fn spellcheck_line_matches_a_token_despite_trailing_punctuation() {
        let misspelled = flagged(&["wrold"]);
        let line = spellcheck_line(
            "hello wrold.",
            &misspelled,
            Style::new().add_modifier(Modifier::UNDERLINED),
        );

        let flagged_spans: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            flagged_spans,
            vec!["wrold."],
            "the whole token (punctuation included) is styled, only the lookup key is stripped"
        );
    }

    #[test]
    fn spellcheck_line_reconstructs_the_exact_original_line() {
        // Regression guard for the byte-offset splicing: whatever spans
        // come out, concatenating their content must reproduce the input
        // exactly — including with multibyte characters, where an
        // off-by-one would panic rather than just mismatch.
        let misspelled = flagged(&["wrold", "häuser"]);
        for line in [
            "hello wrold today",
            "  wrold  ",
            "Häuser sind schön wrold.",
            "no matches at all",
        ] {
            let rendered = spellcheck_line(line, &misspelled, Style::new());
            let reconstructed: String = rendered.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(reconstructed, line);
        }
    }

    // -- overlay_style -----------------------------------------------------------

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn highlighted(line: &Line) -> String {
        line.spans
            .iter()
            .filter(|s| s.style.bg == Some(ratatui::style::Color::Blue))
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn selection() -> Style {
        Style::new().bg(ratatui::style::Color::Blue)
    }

    #[test]
    fn overlay_style_highlights_exactly_the_selected_chars() {
        let line = overlay_style(Line::raw("hello world"), 3, 8, selection());
        assert_eq!(text_of(&line), "hello world");
        assert_eq!(highlighted(&line), "lo wo");
    }

    #[test]
    fn overlay_style_spans_several_existing_spans_and_keeps_their_styling() {
        let underlined = Style::new().add_modifier(Modifier::UNDERLINED);
        let line = Line::from(vec![
            Span::raw("say "),
            Span::styled("wrold", underlined),
            Span::raw(" now"),
        ]);
        let out = overlay_style(line, 2, 7, selection());
        assert_eq!(text_of(&out), "say wrold now");
        assert_eq!(highlighted(&out), "y wro");
        let still_underlined: String = out
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            still_underlined, "wrold",
            "the misspelling underline survives the selection"
        );
    }

    #[test]
    fn overlay_style_handles_multibyte_text_and_out_of_range_ends() {
        let line = overlay_style(Line::raw("grüße"), 1, 99, selection());
        assert_eq!(text_of(&line), "grüße");
        assert_eq!(highlighted(&line), "rüße");
    }

    #[test]
    fn overlay_style_with_an_empty_range_changes_nothing() {
        let line = overlay_style(Line::raw("abc"), 2, 2, selection());
        assert_eq!(highlighted(&line), "");
        assert_eq!(text_of(&line), "abc");
    }

    // -- body_mode_text ------------------------------------------------------------

    #[test]
    fn body_mode_text_labels_each_mode() {
        assert_eq!(body_mode_text(VimMode::Insert, "").0, "-- INSERT --");
        assert_eq!(body_mode_text(VimMode::Visual, "").0, "-- VISUAL --");
        assert_eq!(body_mode_text(VimMode::VisualLine, "").0, "-- VISUAL LINE --");
        assert_eq!(
            body_mode_text(VimMode::Normal, "").0,
            "",
            "Normal mode has no label, like vim"
        );
        assert_eq!(
            body_mode_text(VimMode::Normal, "2d").0,
            "2d",
            "shows the partly typed command"
        );
        assert!(body_mode_text(VimMode::Normal, "").1.contains(":wq send"));
    }
}
