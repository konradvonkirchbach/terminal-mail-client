use std::collections::HashMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{ComposeField, ComposeState};
use crate::attach;
use crate::editable::byte_index;
use crate::spellcheck::{self, normalize_word};
use crate::theme::Theme;
use crate::vim::Mode as VimMode;

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
    let mut spans = vec![Span::styled(
        format!("{:<LABEL_WIDTH$}", "Files:"),
        label_style,
    )];

    if compose.attachments.items.is_empty() {
        spans.push(Span::styled(
            "(none — Ctrl+A to attach)",
            Style::new().fg(theme.muted),
        ));
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

    let lines = vec![
        field_line("To:", &compose.to.value, compose.focus == ComposeField::To, theme),
        field_line("Cc:", &compose.cc.value, compose.focus == ComposeField::Cc, theme),
        field_line("Bcc:", &compose.bcc.value, compose.focus == ComposeField::Bcc, theme),
        field_line(
            "Subject:",
            &compose.subject.value,
            compose.focus == ComposeField::Subject,
            theme,
        ),
        attachments_line(compose, theme),
    ];
    frame.render_widget(Paragraph::new(lines), inner);

    if let Some((row, input)) = focused_input_row(compose) {
        frame.set_cursor_position((inner.x + LABEL_WIDTH as u16 + input.cursor as u16, inner.y + row));
    }
}

/// Computes the rect for a floating list popup anchored with its
/// top-left corner at `(x, y)`, wide enough for `max_content_chars` of
/// text (clamped to `min_width..=max_width`) and tall enough for
/// `item_count` items, both clamped to fit on screen — shared by
/// `draw_suggestions` and `draw_misspelling_suggestions` so a future fix
/// to this clamping only has to happen once. `None` when there isn't
/// enough room left to draw anything useful.
fn popup_rect(frame_area: Rect, x: u16, y: u16, max_content_chars: u16, min_width: u16, max_width: u16, item_count: usize) -> Option<Rect> {
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
    let Some((row, _)) = focused_input_row(compose) else { return };

    let inner = Block::new().borders(Borders::ALL).inner(header_area);
    let x = inner.x + LABEL_WIDTH as u16;
    let y = inner.y + row + 1;

    let max_content_chars = compose.suggestions.items.iter().map(|a| a.to_string().chars().count() as u16).max().unwrap_or(10);
    let Some(popup) = popup_rect(frame.area(), x, y, max_content_chars, 10, 50, compose.suggestions.items.len()) else {
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
        .title(Span::styled(" body ", Style::new().fg(theme.bright_foreground)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = inner.height.max(1);
    let scroll_y = (compose.body.cursor_row as u16).saturating_sub(visible_height - 1);

    let flagged_style = Style::new().fg(theme.red).add_modifier(Modifier::UNDERLINED);
    let selection_style = Style::new().bg(theme.selection).fg(theme.bright_foreground);
    let no_misspellings = HashMap::new();
    let text = Text::from(
        compose
            .body
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let misspelled = compose.misspelled_by_line.get(i).unwrap_or(&no_misspellings);
                let line = spellcheck_line(l, misspelled, flagged_style);
                match compose.vim.selection_on_row(&compose.body, i) {
                    // A selected empty line still shows as one highlighted cell.
                    Some(_) if l.is_empty() => Line::from(Span::styled(" ", selection_style)),
                    Some((start, end)) => overlay_style(line, start, end, selection_style),
                    None => line,
                }
            })
            .collect::<Vec<_>>(),
    );
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false }).scroll((scroll_y, 0));
    frame.render_widget(paragraph, inner);

    // In Command mode the cursor belongs to the command line (`draw_help`).
    if focused && !matches!(compose.vim.mode, VimMode::Command(_)) {
        let y = (compose.body.cursor_row as u16).saturating_sub(scroll_y);
        frame.set_cursor_position((inner.x + compose.body.cursor_col as u16, inner.y + y));
        if matches!(compose.vim.mode, VimMode::Normal | VimMode::Insert) {
            draw_misspelling_suggestions(frame, inner, y, compose, theme);
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
    cursor_screen_row: u16,
    compose: &ComposeState,
    theme: &Theme,
) {
    let Some((word, suggestions)) = compose.misspelled_word_at_cursor() else { return };
    if suggestions.is_empty() {
        return;
    }

    let x = body_inner.x + (compose.body.cursor_col as u16).min(body_inner.width.saturating_sub(1));
    let y = body_inner.y + cursor_screen_row + 1;

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
            .title(Span::styled(format!(" {word}? "), Style::new().fg(theme.bright_foreground))),
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
        Line::from(Span::styled("\u{2191}/\u{2193} choose suggestion   Enter/Tab accept   Esc dismiss", muted))
    } else if body_focused {
        let (label, hint) = body_mode_text(compose.vim.mode, compose.vim.pending_keys());
        let mut spans = Vec::new();
        if !label.is_empty() {
            spans.push(Span::styled(label, Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)));
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
        VimMode::Insert => ("-- INSERT --".to_string(), "Esc normal mode   Tab next field   Ctrl+S send"),
        VimMode::Visual => ("-- VISUAL --".to_string(), VISUAL_HINT),
        VimMode::VisualLine => ("-- VISUAL LINE --".to_string(), VISUAL_HINT),
        VimMode::Normal | VimMode::Command(_) => {
            (pending.to_string(), "i insert   v visual   u undo   :wq send   :q! discard   Tab next field")
        }
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
        let line = spellcheck_line("hello wrold.", &misspelled, Style::new().add_modifier(Modifier::UNDERLINED));

        let flagged_spans: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(flagged_spans, vec!["wrold."], "the whole token (punctuation included) is styled, only the lookup key is stripped");
    }

    #[test]
    fn spellcheck_line_reconstructs_the_exact_original_line() {
        // Regression guard for the byte-offset splicing: whatever spans
        // come out, concatenating their content must reproduce the input
        // exactly — including with multibyte characters, where an
        // off-by-one would panic rather than just mismatch.
        let misspelled = flagged(&["wrold", "häuser"]);
        for line in ["hello wrold today", "  wrold  ", "Häuser sind schön wrold.", "no matches at all"] {
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
        let line = Line::from(vec![Span::raw("say "), Span::styled("wrold", underlined), Span::raw(" now")]);
        let out = overlay_style(line, 2, 7, selection());
        assert_eq!(text_of(&out), "say wrold now");
        assert_eq!(highlighted(&out), "y wro");
        let still_underlined: String = out
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(still_underlined, "wrold", "the misspelling underline survives the selection");
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
        assert_eq!(body_mode_text(VimMode::Normal, "").0, "", "Normal mode has no label, like vim");
        assert_eq!(body_mode_text(VimMode::Normal, "2d").0, "2d", "shows the partly typed command");
        assert!(body_mode_text(VimMode::Normal, "").1.contains(":wq send"));
    }
}
