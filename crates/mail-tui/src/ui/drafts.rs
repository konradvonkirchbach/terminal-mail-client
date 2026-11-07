use mail_core::SavedDraft;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState};
use ratatui::Frame;

use crate::app::Selectable;
use crate::theme::Theme;

/// The saved-drafts picker, a modal over the main view.
pub fn draw(frame: &mut Frame, area: Rect, drafts: &Selectable<SavedDraft>, theme: &Theme) {
    let modal = super::filebrowser::centered_rect(80, (drafts.items.len() as u16 + 2).clamp(5, 20), area);
    frame.render_widget(Clear, modal);

    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(
            " drafts — Enter open   d delete   Esc close ",
            Style::new().fg(theme.bright_foreground),
        ));

    let items: Vec<ListItem> = drafts
        .items
        .iter()
        .map(|saved| {
            let subject = match saved.draft.subject.trim() {
                "" => "(no subject)".to_string(),
                s => s.to_string(),
            };
            let to = match saved.draft.to.trim() {
                "" => String::new(),
                to => format!("  to {to}"),
            };
            let when = saved
                .updated_at
                .map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M  ").to_string())
                .unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(when, Style::new().fg(theme.muted)),
                Span::styled(subject, Style::new().fg(theme.bright_foreground)),
                Span::styled(to, Style::new().fg(theme.muted)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(drafts.selected));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(theme.selection).fg(theme.bright_foreground));
    frame.render_stateful_widget(list, modal, &mut state);
}
