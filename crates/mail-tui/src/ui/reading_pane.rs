use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, BodyState, BodyViewport};
use crate::attach::human_size;
use crate::theme::Theme;

use super::layout;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let lines: Vec<Line<'static>> = match &app.body {
        BodyState::Empty => vec![Line::from("Select a message and press Enter to read it.")],
        BodyState::Loading { .. } => vec![Line::from("Loading...")],
        BodyState::Error(msg) => vec![Line::from(Span::styled(msg.clone(), Style::new().fg(theme.red)))],
        BodyState::Loaded(message) => {
            let from = message.from.first().map(|a| a.to_string()).unwrap_or_default();
            let to = message.to.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
            let date = message
                .date
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();

            let mut lines = vec![
                Line::from(Span::styled(
                    message.subject.clone(),
                    Style::new().fg(theme.bright_foreground).bold(),
                )),
                Line::from(vec![
                    Span::styled("From: ", Style::new().fg(theme.muted)),
                    Span::raw(from),
                ]),
                Line::from(vec![Span::styled("To: ", Style::new().fg(theme.muted)), Span::raw(to)]),
                Line::from(vec![
                    Span::styled("Date: ", Style::new().fg(theme.muted)),
                    Span::raw(date),
                ]),
            ];

            if !message.attachments.is_empty() {
                let mut spans = vec![Span::styled("Attachments: ", Style::new().fg(theme.muted))];
                for (i, a) in message.attachments.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw("  "));
                    }
                    let text = format!("{} ({})", a.filename, human_size(a.size_bytes));
                    let style = if i == app.selected_attachment {
                        Style::new().bg(theme.selection).fg(theme.bright_foreground)
                    } else {
                        Style::new().fg(theme.foreground)
                    };
                    spans.push(Span::styled(text, style));
                }
                lines.push(Line::from(spans));
                lines.push(Line::from(Span::styled(
                    "[ ] select   a download",
                    Style::new().fg(theme.muted),
                )));
            }

            lines.push(Line::from(Span::styled(
                "─".repeat(inner_width),
                Style::new().fg(theme.muted),
            )));
            for line in message.body_text.lines() {
                lines.push(Line::from(layout::expand_tabs(line)));
            }
            lines
        }
    };

    // Wrapped up front so the row count — and with it how far the pane can
    // scroll — is known exactly. The key handlers clamp against what's
    // recorded here, since only rendering knows the pane's size.
    let rows = layout::wrap_lines(&lines, inner_width);
    let height = area.height.saturating_sub(2);
    let max_scroll = rows.len().saturating_sub(height as usize).min(u16::MAX as usize) as u16;
    app.body_viewport.set(BodyViewport { height, max_scroll });
    let scroll = app.body_scroll.min(max_scroll);

    let title = if max_scroll > 0 {
        let percent = u32::from(scroll) * 100 / u32::from(max_scroll);
        format!(" message ({percent}%) ")
    } else {
        " message ".to_string()
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(title, Style::new().fg(theme.bright_foreground)));

    let paragraph = Paragraph::new(rows).block(block).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}
