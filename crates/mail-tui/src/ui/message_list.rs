use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, ListState as AppListState};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let visible = app.visible_indices();
    let title = match &app.list_state {
        AppListState::Loading => " inbox (loading...) ".to_string(),
        AppListState::Loaded if app.search.is_some() => {
            format!(" inbox ({}/{} match) ", visible.len(), app.envelopes.len())
        }
        AppListState::Loaded => " inbox ".to_string(),
        AppListState::Error(_) => " inbox (error) ".to_string(),
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.muted))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(title, Style::new().fg(theme.bright_foreground)));

    if let AppListState::Error(msg) = &app.list_state {
        // A List doesn't wrap long items — it just clips them at the pane
        // edge, which for a real IMAP error (often a whole server response
        // line) hides the one detail that explains what went wrong. Use a
        // wrapping Paragraph instead so the full message is always visible.
        let text = Line::from(Span::styled(msg.clone(), Style::new().fg(theme.red)));
        let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = visible
        .iter()
        .filter_map(|&i| app.envelopes.get(i))
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
    if !visible.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(theme.selection).fg(theme.bright_foreground));

    frame.render_stateful_widget(list, area, &mut state);
}
