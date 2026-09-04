use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let count = app.envelopes.len();
    let hints = "j/k move  Enter read  r sync  q quit";
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.account_email),
            Style::new().bg(theme.accent).fg(theme.background),
        ),
        Span::raw(format!(" {count} messages  ")),
        Span::styled(hints, Style::new().fg(theme.muted)),
    ]);

    let paragraph = Paragraph::new(line).style(Style::new().bg(theme.background));
    frame.render_widget(paragraph, area);
}
