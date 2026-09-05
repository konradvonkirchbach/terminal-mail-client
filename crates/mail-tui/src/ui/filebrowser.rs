use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::BrowserPurpose;
use crate::attach::human_size;
use crate::filebrowser::FileBrowser;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, browser: &FileBrowser, purpose: &BrowserPurpose, theme: &Theme) {
    let modal = centered_rect(80, 22, area);
    frame.render_widget(Clear, modal);

    let title = match purpose {
        BrowserPurpose::AttachToCompose => " attach file ",
        BrowserPurpose::SaveAttachment { .. } => " save attachment ",
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().bg(theme.background).fg(theme.foreground))
        .title(Span::styled(title, Style::new().fg(theme.bright_foreground)));

    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let has_filename = browser.filename.is_some();
    let list_height = if has_filename {
        inner.height.saturating_sub(3)
    } else {
        inner.height.saturating_sub(1)
    };

    let path_area = Rect { height: 1, ..inner };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            browser.current_dir.display().to_string(),
            Style::new().fg(theme.muted),
        ))),
        path_area,
    );

    let list_area = Rect {
        y: inner.y + 1,
        height: list_height,
        ..inner
    };

    if let Some(err) = &browser.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(err.clone(), Style::new().fg(theme.red)))),
            list_area,
        );
    } else {
        let items: Vec<ListItem> = browser
            .entries
            .iter()
            .map(|e| {
                let text = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    format!("{}  ({})", e.name, human_size(e.size))
                };
                let style = if e.is_dir {
                    Style::new().fg(theme.blue)
                } else {
                    Style::new().fg(theme.foreground)
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect();

        let mut state = ListState::default();
        if !browser.entries.is_empty() && !browser.filename_focused {
            state.select(Some(browser.selected));
        }

        let list = List::new(items)
            .highlight_style(Style::new().bg(theme.selection).fg(theme.bright_foreground));
        frame.render_stateful_widget(list, list_area, &mut state);
    }

    if let Some(filename) = &browser.filename {
        let filename_area = Rect {
            y: inner.y + inner.height.saturating_sub(2),
            height: 1,
            ..inner
        };
        let label_style = if browser.filename_focused {
            Style::new().fg(theme.accent)
        } else {
            Style::new().fg(theme.muted)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Save as: ", label_style),
                Span::styled(filename.value.clone(), Style::new().fg(theme.foreground)),
            ])),
            filename_area,
        );
        if browser.filename_focused {
            frame.set_cursor_position((
                filename_area.x + 9 + filename.cursor as u16,
                filename_area.y,
            ));
        }
    }

    let hint_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    let hint = if has_filename {
        "j/k move   Enter open/save   Tab switch field   Esc cancel"
    } else {
        "j/k move   Enter open/select   Esc cancel"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().fg(theme.muted)))),
        hint_area,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
