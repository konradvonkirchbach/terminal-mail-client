mod compose;
mod message_list;
mod reading_pane;
mod sidebar;
mod statusline;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    // Compose is a full-screen takeover, not a floating popup — a flat
    // aesthetic gets nothing from stacking panes.
    if let Some(compose_state) = &app.compose {
        let area = frame.area();
        compose::draw(frame, area, compose_state, theme);
        return;
    }

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
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
    statusline::draw(frame, root[1], app, theme);
}
