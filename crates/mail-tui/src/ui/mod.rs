mod compose;
mod drafts;
mod filebrowser;
mod layout;
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
    if let Some(drafts) = &app.drafts {
        let area = frame.area();
        drafts::draw(frame, area, drafts, theme);
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

    let search_bar_height = if app.search().is_some() { 1 } else { 0 };
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
    if let Some(search) = app.search() {
        search_bar::draw(frame, root[1], search, app.search_editing, theme);
    }
    statusline::draw(frame, root[2], app, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{BodyState, Selectable};
    use mail_core::{Address, Envelope, Flags, Folder, FolderRole, Message, MessageAttachment, SavedDraft};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn folder(id: i64, name: &str, role: Option<FolderRole>, selectable: bool, unseen: u32) -> Folder {
        Folder {
            id,
            account_id: 1,
            name: name.to_string(),
            delimiter: Some("/".to_string()),
            role,
            selectable,
            unseen: Some(unseen),
            total: Some(10),
        }
    }

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal.draw(|f| draw(f, app, &Theme::fallback())).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app() -> App {
        let mut app = App::new(vec!["me@example.com".to_string(), "other@example.com".to_string()]);
        app.set_folders(
            0,
            vec![
                folder(1, "INBOX", Some(FolderRole::Inbox), true, 3),
                folder(2, "[Gmail]/Sent Mail", Some(FolderRole::Sent), true, 0),
                folder(3, "[Gmail]", None, false, 0),
                folder(4, "[Gmail]/Important", None, true, 0),
            ],
        );
        app.set_folders(1, vec![folder(9, "INBOX", Some(FolderRole::Inbox), true, 7)]);
        app.current_folder_id = 1;
        app.replace_envelopes(vec![Envelope {
            uid: 1,
            subject: "Starred one".to_string(),
            from: vec![Address {
                name: Some("Alice".to_string()),
                email: "alice@example.com".to_string(),
            }],
            to: Vec::new(),
            date: None,
            flags: Flags {
                flagged: true,
                ..Flags::default()
            },
        }]);
        app
    }

    #[test]
    fn the_sidebar_lists_folders_with_unread_counts() {
        let screen = screen(&app());
        assert!(screen.contains("Inbox"), "{screen}");
        assert!(screen.contains("Sent Mail"));
        assert!(screen.contains("    Important"), "nested under [Gmail]");
        assert!(screen.contains(" 3"), "the inbox's unread count");
        assert!(screen.contains(" 7"), "the other account's inbox count");
    }

    #[test]
    fn the_list_marks_starred_mail_and_titles_the_folder() {
        let screen = screen(&app());
        assert!(screen.contains("*! Alice"), "{screen}");
        assert!(screen.contains(" Inbox "));
    }

    #[test]
    fn queued_mail_shows_in_the_status_bar() {
        let mut app = app();
        app.outbox.pending = 2;
        app.outbox.failed = 1;
        let screen = screen(&app);
        assert!(screen.contains("2 queued (F sends)"), "{screen}");
        assert!(screen.contains("1 unsendable"));
    }

    #[test]
    fn an_open_message_shows_cc_and_how_to_open_its_parts() {
        let mut app = app();
        app.body = BodyState::Loaded(Box::new(Message {
            uid: 1,
            subject: "Report".to_string(),
            cc: vec![Address {
                name: None,
                email: "carol@example.com".to_string(),
            }],
            has_html: true,
            attachments: vec![MessageAttachment {
                filename: "report.pdf".to_string(),
                size_bytes: 2048,
            }],
            ..Message::default()
        }));
        let screen = screen(&app);
        assert!(screen.contains("Cc: carol@example.com"), "{screen}");
        assert!(screen.contains("o open   O view as HTML"));
    }

    #[test]
    fn the_drafts_picker_lists_saved_drafts() {
        let mut app = app();
        let mut draft = SavedDraft::default();
        draft.draft.subject = "Half-written".to_string();
        draft.draft.to = "bob@example.com".to_string();
        app.drafts = Some(Selectable {
            items: vec![draft],
            selected: 0,
        });
        let screen = screen(&app);
        assert!(screen.contains("Half-written  to bob@example.com"), "{screen}");
    }
}
