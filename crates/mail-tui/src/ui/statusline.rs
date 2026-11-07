use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use mail_core::FolderRole;

use crate::app::{App, BodyState};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let count = app.envelopes().len();
    // What's worth knowing depends on whether a message is open.
    let hints = if matches!(app.body, BodyState::Loaded(_)) {
        "J/K Space scroll  r reply  R all  f forward  [ ] attachment  a save  o open  O html  m unread  s star  d delete  Esc close"
    } else if app.account_emails.len() > 1 {
        "j/k move  Enter read  / find  c compose  P drafts  { } folder  gi inbox  m read  s star  d delete  S sync  Tab account  q quit"
    } else {
        "j/k move  Enter read  / find  c compose  P drafts  { } folder  gi inbox  m read  s star  d delete  S sync  q quit"
    };
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.current_account_email()),
            Style::new().bg(theme.accent).fg(theme.background),
        ),
        Span::raw(format!(" {count} messages  ")),
    ];
    if app.outbox.pending > 0 {
        spans.push(Span::styled(
            format!("{} queued (F sends)  ", app.outbox.pending),
            Style::new().fg(theme.yellow),
        ));
    }
    if app.outbox.failed > 0 {
        spans.push(Span::styled(
            format!("{} unsendable (F retries)  ", app.outbox.failed),
            Style::new().fg(theme.red),
        ));
    }

    if app.confirm_delete.is_some() {
        let prompt = if app.current_folder_role() == Some(FolderRole::Trash) {
            "Delete this message permanently? [y/N]"
        } else {
            "Delete this message? [y/N]"
        };
        spans.push(Span::styled(prompt, Style::new().fg(theme.red)));
    } else {
        match &app.status_message {
            Some((msg, _)) => spans.push(Span::styled(msg.clone(), Style::new().fg(theme.green))),
            None => spans.push(Span::styled(hints, Style::new().fg(theme.muted))),
        }
    }
    let line = Line::from(spans);

    let paragraph = Paragraph::new(line).style(Style::new().bg(theme.background));
    frame.render_widget(paragraph, area);
}
