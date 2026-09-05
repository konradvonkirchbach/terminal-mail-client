mod compose;
mod filebrowser;
mod message_list;
mod reading_pane;
mod search_bar;
mod sidebar;
mod statusline;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    draw_base(frame, app, theme);

    // The directory browser is a modal overlay on top of whatever's
    // underneath (compose or the normal 3-pane view) — it's opened from
    // either, so it's drawn here rather than owned by one of them.
    if let Some((browser, purpose)) = &app.file_browser {
        let area = frame.area();
        filebrowser::draw(frame, area, browser, purpose, theme);
    }
}

fn draw_base(frame: &mut Frame, app: &App, theme: &Theme) {
    // Compose is a full-screen takeover, not a floating popup — a flat
    // aesthetic gets nothing from stacking panes.
    if let Some(compose_state) = &app.compose {
        let area = frame.area();
        compose::draw(frame, area, compose_state, theme);
        return;
    }

    let search_bar_height = if app.search.is_some() { 1 } else { 0 };
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(search_bar_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(35),
            Constraint::Percentage(45),
        ])
        .split(root[0]);

    sidebar::draw(frame, columns[0], app, theme);
    message_list::draw(frame, columns[1], app, theme);
    reading_pane::draw(frame, columns[2], app, theme);
    if let Some(search) = &app.search {
        search_bar::draw(frame, root[1], search, app.search_editing, theme);
    }
    statusline::draw(frame, root[2], app, theme);
}
