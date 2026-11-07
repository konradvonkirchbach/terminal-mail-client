use mail_core::{Folder, FolderRole};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::theme::Theme;

/// Every account, and under the current one its folders with their unread
/// counts. Other accounts show their inbox's unread count next to them.
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
    let width = block.inner(area).width as usize;

    let mut items = Vec::new();
    let mut current_row = None;
    for (i, email) in app.account_emails.iter().enumerate() {
        let active = i == app.current_account;
        let folders = app.folders.get(i).map(Vec::as_slice).unwrap_or_default();

        let (email_style, unseen) = if active {
            (Style::new().fg(theme.bright_foreground).bold(), 0)
        } else {
            let inbox_unseen = folders
                .iter()
                .find(|f| f.role == Some(FolderRole::Inbox))
                .and_then(|f| f.unseen)
                .unwrap_or(0);
            (Style::new().fg(theme.muted), inbox_unseen)
        };
        items.push(ListItem::new(row(email, "", unseen, width, email_style, theme)));
        if !active {
            continue;
        }

        if folders.is_empty() {
            // Not listed yet (first launch): the inbox is all there is.
            current_row = Some(items.len());
            let style = Style::new().fg(theme.accent).bold();
            items.push(ListItem::new(row("Inbox", "  ", 0, width, style, theme)));
            continue;
        }
        for folder in folders {
            let current = folder.id == app.current_folder_id;
            if current {
                current_row = Some(items.len());
            }
            items.push(ListItem::new(folder_row(folder, current, width, theme)));
        }
    }

    // Selecting the current folder's row keeps it scrolled into view when
    // there are more folders than fit.
    let mut state = ListState::default();
    state.select(current_row);
    frame.render_stateful_widget(List::new(items).block(block), area, &mut state);
}

fn folder_row(folder: &Folder, current: bool, width: usize, theme: &Theme) -> Line<'static> {
    let indent = " ".repeat(2 + 2 * folder.indent());
    let unseen = folder.unseen.unwrap_or(0);
    let style = if current {
        Style::new().fg(theme.accent).bold()
    } else if !folder.selectable {
        Style::new().fg(theme.muted).italic()
    } else if unseen > 0 {
        Style::new().fg(theme.bright_foreground)
    } else {
        Style::new().fg(theme.foreground)
    };
    row(&folder.display_name(), &indent, unseen, width, style, theme)
}

/// `indent` + `label`, with the unread count (if any) right-aligned; the
/// label is cut short rather than pushing the count out of view.
fn row(label: &str, indent: &str, unseen: u32, width: usize, style: Style, theme: &Theme) -> Line<'static> {
    let count = if unseen > 0 {
        format!(" {unseen}")
    } else {
        String::new()
    };
    let room = width.saturating_sub(indent.len() + count.len());
    let mut text = String::new();
    for c in label.chars() {
        if (text.as_str().width() + c.to_string().width()) > room {
            break;
        }
        text.push(c);
    }
    let pad = room.saturating_sub(text.width());
    Line::from(vec![
        Span::styled(format!("{indent}{text}{}", " ".repeat(pad)), style),
        Span::styled(count, Style::new().fg(theme.accent).bold()),
    ])
}
