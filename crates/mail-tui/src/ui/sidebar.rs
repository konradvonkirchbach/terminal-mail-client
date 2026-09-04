use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(
            " account ",
            Style::new().fg(theme.bright_foreground),
        ));

    let items = vec![
        ListItem::new(Line::from(Span::styled(
            app.account_email.clone(),
            Style::new().fg(theme.bright_foreground).bold(),
        ))),
        ListItem::new(Line::from(Span::styled(
            "  INBOX",
            Style::new().fg(theme.accent),
        ))),
    ];

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}
