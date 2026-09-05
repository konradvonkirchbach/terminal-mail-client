use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let count = app.envelopes.len();
    let hints = "j/k move  / find  Enter read  c compose  r reply  S sync  q quit";
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.account_email),
            Style::new().bg(theme.accent).fg(theme.background),
        ),
        Span::raw(format!(" {count} messages  ")),
    ];
    match &app.status_message {
        Some(msg) => spans.push(Span::styled(msg.clone(), Style::new().fg(theme.green))),
        None => spans.push(Span::styled(hints, Style::new().fg(theme.muted))),
    }
    let line = Line::from(spans);

    let paragraph = Paragraph::new(line).style(Style::new().bg(theme.background));
    frame.render_widget(paragraph, area);
}
