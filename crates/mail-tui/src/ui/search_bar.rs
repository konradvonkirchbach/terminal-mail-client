use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::editable::TextInput;
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, search: &TextInput, editing: bool, theme: &Theme) {
    let line = Line::from(vec![
        Span::styled("/", Style::new().fg(theme.accent)),
        Span::styled(search.value.clone(), Style::new().fg(theme.foreground)),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::new().bg(theme.background)),
        area,
    );

    if editing {
        frame.set_cursor_position((area.x + 1 + search.cursor as u16, area.y));
    }
}
