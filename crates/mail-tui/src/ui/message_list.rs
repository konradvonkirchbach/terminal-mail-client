use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::app::{App, ListState as AppListState};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let title = match &app.list_state {
        AppListState::Loading => " inbox (loading...) ",
        AppListState::Loaded => " inbox ",
        AppListState::Error(_) => " inbox (error) ",
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(title, Style::new().fg(theme.bright_foreground)));

    if let AppListState::Error(msg) = &app.list_state {
        let items = vec![ListItem::new(Line::from(Span::styled(
            msg.clone(),
            Style::new().fg(theme.red),
        )))];
        frame.render_widget(List::new(items).block(block), area);
        return;
    }

    let items: Vec<ListItem> = app
        .envelopes
        .iter()
        .map(|env| {
            let flag = if env.flags.seen { " " } else { "*" };
            let from = env
                .from
                .first()
                .map(|a| a.name.clone().unwrap_or_else(|| a.email.clone()))
                .unwrap_or_else(|| "(unknown)".to_string());
            let date = env
                .date
                .map(|d| d.format("%m-%d %H:%M").to_string())
                .unwrap_or_default();

            let flag_color = if env.flags.seen { theme.muted } else { theme.accent };
            let subject_style = if env.flags.seen {
                Style::new().fg(theme.foreground)
            } else {
                Style::new().fg(theme.bright_foreground).bold()
            };

            ListItem::new(Line::from(vec![
                Span::styled(format!("{flag} "), Style::new().fg(flag_color)),
                Span::styled(format!("{from:<22.22} "), Style::new().fg(theme.muted)),
                Span::styled(env.subject.clone(), subject_style),
                Span::raw(format!(" {date}")),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !app.envelopes.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(theme.selection).fg(theme.bright_foreground));

    frame.render_stateful_widget(list, area, &mut state);
}
