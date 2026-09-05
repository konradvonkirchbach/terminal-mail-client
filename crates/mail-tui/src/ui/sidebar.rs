use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let title = if app.account_emails.len() > 1 {
        " accounts "
    } else {
        " account "
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(title, Style::new().fg(theme.bright_foreground)));

    let mut items = Vec::with_capacity(app.account_emails.len() * 2);
    for (i, email) in app.account_emails.iter().enumerate() {
        let active = i == app.current_account;
        let email_style = if active {
            Style::new().fg(theme.bright_foreground).bold()
        } else {
            Style::new().fg(theme.muted)
        };
        let inbox_style = if active {
            Style::new().fg(theme.accent)
        } else {
            Style::new().fg(theme.muted)
        };
        items.push(ListItem::new(Line::from(Span::styled(email.clone(), email_style))));
        items.push(ListItem::new(Line::from(Span::styled("  INBOX", inbox_style))));
    }

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}
