use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::editable::TextInput;
use crate::theme::Theme;

use super::layout;

pub fn draw(frame: &mut Frame, area: Rect, search: &TextInput, editing: bool, theme: &Theme) {
    // Scrolled so the cursor stays visible once the query outgrows the bar.
    let (start, cursor_x) = layout::input_window(&search.value, search.cursor, area.width.saturating_sub(1) as usize);
    let line = Line::from(vec![
        Span::styled("/", Style::new().fg(theme.accent)),
        Span::styled(
            layout::from_char(&search.value, start).to_string(),
            Style::new().fg(theme.foreground),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).style(Style::new().bg(theme.background)), area);

    if editing {
        frame.set_cursor_position((area.x + 1 + cursor_x as u16, area.y));
    }
}
