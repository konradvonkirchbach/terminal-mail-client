use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, BodyState};
use crate::attach::human_size;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(" message ", Style::new().fg(theme.bright_foreground)));

    let text = match &app.body {
        BodyState::Empty => Text::from("Select a message and press Enter to read it."),
        BodyState::Loading => Text::from("Loading..."),
        BodyState::Error(msg) => Text::from(Span::styled(msg.clone(), Style::new().fg(theme.red))),
        BodyState::Loaded(message) => {
            let from = message
                .from
                .first()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let to = message
                .to
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let date = message
                .date
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
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
                Line::from(vec![
                    Span::styled("To: ", Style::new().fg(theme.muted)),
                    Span::raw(to),
                ]),
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
                "─".repeat(area.width.saturating_sub(2) as usize),
                Style::new().fg(theme.muted),
            )));
            for line in message.body_text.lines() {
                lines.push(Line::from(line.to_string()));
            }
            Text::from(lines)
        }
    };

    let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}
