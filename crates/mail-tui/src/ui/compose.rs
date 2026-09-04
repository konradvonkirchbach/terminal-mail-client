use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{ComposeField, ComposeState};
use crate::theme::Theme;

const LABEL_WIDTH: usize = 9; // fits "Subject:" (8) plus a trailing space

pub fn draw(frame: &mut Frame, area: Rect, compose: &ComposeState, theme: &Theme) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // header block: 4 fields + top/bottom border
            Constraint::Min(3),    // body
            Constraint::Length(1), // help/status line
        ])
        .split(area);

    draw_header(frame, rows[0], compose, theme);
    draw_body(frame, rows[1], compose, theme);
    draw_help(frame, rows[2], compose, theme);
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
    ];
    frame.render_widget(Paragraph::new(lines), inner);

    if let Some((row, input)) = focused_input_row(compose) {
        frame.set_cursor_position((inner.x + LABEL_WIDTH as u16 + input.cursor as u16, inner.y + row));
    }
}

fn focused_input_row(compose: &ComposeState) -> Option<(u16, &crate::editable::TextInput)> {
    match compose.focus {
        ComposeField::To => Some((0, &compose.to)),
        ComposeField::Cc => Some((1, &compose.cc)),
        ComposeField::Bcc => Some((2, &compose.bcc)),
        ComposeField::Subject => Some((3, &compose.subject)),
        ComposeField::Body => None,
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

    let text = Text::from(
        compose
            .body
            .lines
            .iter()
            .map(|l| Line::from(l.clone()))
            .collect::<Vec<_>>(),
    );
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false }).scroll((scroll_y, 0));
    frame.render_widget(paragraph, inner);

    if focused {
        let y = (compose.body.cursor_row as u16).saturating_sub(scroll_y);
        frame.set_cursor_position((inner.x + compose.body.cursor_col as u16, inner.y + y));
    }
}

fn draw_help(frame: &mut Frame, area: Rect, compose: &ComposeState, theme: &Theme) {
    let text = if compose.sending {
        Line::from(Span::styled("Sending...", Style::new().fg(theme.accent)))
    } else if let Some(err) = &compose.error {
        Line::from(Span::styled(format!("Send failed: {err}"), Style::new().fg(theme.red)))
    } else {
        Line::from(Span::styled(
            "Tab/Shift+Tab move field   Ctrl+S send   Esc cancel",
            Style::new().fg(theme.muted),
        ))
    };
    frame.render_widget(Paragraph::new(text).style(Style::new().bg(theme.background)), area);
}
