use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let count = app.envelopes.len();
    let hints = if app.account_emails.len() > 1 {
        "j/k move  gg/G top/bottom  Ctrl-d/u page  / find  Enter read  d delete  a download  c compose  r reply  S sync  Tab switch  D set default  q quit"
    } else {
        "j/k move  gg/G top/bottom  Ctrl-d/u page  / find  Enter read  d delete  a download  c compose  r reply  S sync  q quit"
    };
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.current_account_email()),
            Style::new().bg(theme.accent).fg(theme.background),
        ),
        Span::raw(format!(" {count} messages  ")),
    ];

    if app.confirm_delete.is_some() {
        spans.push(Span::styled(
            "Delete this message? [y/N]",
            Style::new().fg(theme.red),
        ));
    } else {
        match &app.status_message {
            Some((msg, _)) => spans.push(Span::styled(msg.clone(), Style::new().fg(theme.green))),
            None => spans.push(Span::styled(hints, Style::new().fg(theme.muted))),
        }
    }
    let line = Line::from(spans);

    let paragraph = Paragraph::new(line).style(Style::new().bg(theme.background));
    frame.render_widget(paragraph, area);
}
