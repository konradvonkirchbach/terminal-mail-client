use std::collections::HashMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{ComposeField, ComposeState};
use crate::attach;
use crate::spellcheck::{self, normalize_word};
use crate::theme::Theme;

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
    let no_misspellings = HashMap::new();
    let text = Text::from(
        compose
            .body
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let misspelled = compose.misspelled_by_line.get(i).unwrap_or(&no_misspellings);
                spellcheck_line(l, misspelled, flagged_style)
            })
            .collect::<Vec<_>>(),
    );
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false }).scroll((scroll_y, 0));
    frame.render_widget(paragraph, inner);

    if focused {
        let y = (compose.body.cursor_row as u16).saturating_sub(scroll_y);
        frame.set_cursor_position((inner.x + compose.body.cursor_col as u16, inner.y + y));
        draw_misspelling_suggestions(frame, inner, y, compose, theme);
    }
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
    let text = if compose.sending {
        Line::from(Span::styled("Sending...", Style::new().fg(theme.accent)))
    } else if let Some(err) = &compose.error {
        Line::from(Span::styled(format!("Send failed: {err}"), Style::new().fg(theme.red)))
    } else if !compose.suggestions.items.is_empty() {
        Line::from(Span::styled(
            "\u{2191}/\u{2193} choose suggestion   Enter/Tab accept   Esc dismiss",
            Style::new().fg(theme.muted),
        ))
    } else {
        let hint = match compose.focus {
            ComposeField::Attachments if !compose.attachments.items.is_empty() => {
                "Ctrl+A attach   Backspace remove   Tab/Shift+Tab move field   Ctrl+S send   Esc cancel"
            }
            _ => "Tab/Shift+Tab move field   Ctrl+A attach   Ctrl+S send   Esc cancel",
        };
        Line::from(Span::styled(hint, Style::new().fg(theme.muted)))
    };
    frame.render_widget(Paragraph::new(text).style(Style::new().bg(theme.background)), area);
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
}
