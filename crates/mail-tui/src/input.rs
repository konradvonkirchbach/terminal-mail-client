//! Key handling: routes each key press to whichever mode currently owns
//! the keyboard (file browser, compose, search, the main list) and turns
//! it into `App` state changes or background work.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::{body_change, App, BodyChange, BodyState, BrowserPurpose, ComposeField, ComposeState, ListState};
use crate::attach;
use crate::bg;
use crate::editable::TextInput;
use crate::filebrowser::FileBrowser;
use crate::session::Session;
use crate::spellcheck::SpellCheckHandle;
use crate::vim::{Mode as VimMode, Outcome};

fn default_browse_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn default_download_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(Path::to_path_buf))
        .unwrap_or_else(default_browse_dir)
}

pub fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, session: &mut Session) {
    if app.file_browser.is_some() {
        handle_browser_key(app, code);
        return;
    }

    if app.compose.is_some() {
        handle_compose_key(app, code, modifiers, session);
        return;
    }

    if app.search_editing {
        handle_search_key(app, code, session);
        return;
    }

    if let Some(uid) = app.confirm_delete.take() {
        if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            app.set_status("Deleting...");
            bg::spawn_delete_message(session.accounts.current().clone(), uid, session.tx.clone());
        }
        return;
    }

    // `gg` needs to see two `g` presses in a row — any other key cancels
    // the pending first one instead of being swallowed by it.
    if !matches!(code, KeyCode::Char('g')) {
        app.pending_g = false;
    }

    match code {
        KeyCode::Char('q') => app.should_quit = true,
        // Esc backs out of whatever is open — a search filter first, then
        // the open message — but never quits: that's `q`'s job, so a
        // reflexive Esc can't close the app.
        KeyCode::Esc => {
            if app.search().is_some() {
                app.clear_search();
            } else {
                app.body = BodyState::Empty;
                app.body_scroll = 0;
            }
        }
        KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('n') => {
            app.select_next();
            maybe_load_more(app, session);
        }
        KeyCode::Char('k') | KeyCode::Up | KeyCode::Char('N') => app.select_prev(),
        KeyCode::Char('g') => {
            if app.pending_g {
                app.pending_g = false;
                app.select_top();
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => {
            app.select_bottom();
            maybe_load_more(app, session);
        }
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.select_page_down();
            maybe_load_more(app, session);
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => app.select_page_up(),
        // Reading pane scrolling: line-wise on J/K, a screenful on
        // Space/Ctrl-f and back on Ctrl-b.
        KeyCode::Char('J') => app.scroll_body(1),
        KeyCode::Char('K') => app.scroll_body(-1),
        KeyCode::Char(' ') => app.page_body(true),
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => app.page_body(true),
        KeyCode::Char('b') if modifiers.contains(KeyModifiers::CONTROL) => app.page_body(false),
        KeyCode::Char('/') => app.start_search(),
        KeyCode::Char('S') => {
            app.list_state = ListState::Loading;
            bg::spawn_sync(session.accounts.current().clone(), session.tx.clone());
        }
        KeyCode::Char('c') => {
            app.status_message = None;
            app.compose = Some(ComposeState::blank());
            bg::spawn_spellchecker_reload(session.tx.clone());
        }
        KeyCode::Char('r') => {
            let reply = app.selected_envelope().map(ComposeState::reply);
            if let Some(reply) = reply {
                app.status_message = None;
                app.compose = Some(reply);
                bg::spawn_spellchecker_reload(session.tx.clone());
            }
        }
        KeyCode::Enter => {
            if let Some(uid) = app.selected_uid() {
                app.start_loading_body(uid);
                bg::spawn_fetch_body(session.accounts.current().clone(), uid, session.tx.clone());
            }
        }
        KeyCode::Char('[') => {
            if let BodyState::Loaded(message) = &app.body {
                if !message.attachments.is_empty() {
                    app.selected_attachment = app.selected_attachment.saturating_sub(1);
                }
            }
        }
        KeyCode::Char(']') => {
            if let BodyState::Loaded(message) = &app.body {
                if !message.attachments.is_empty() {
                    app.selected_attachment = (app.selected_attachment + 1).min(message.attachments.len() - 1);
                }
            }
        }
        KeyCode::Char('a') => start_download(app),
        KeyCode::Char('d') => {
            if let Some(uid) = app.selected_uid() {
                app.confirm_delete = Some(uid);
            }
        }
        KeyCode::Char('D') => set_default_account(app, session),
        KeyCode::Tab if session.accounts.list.len() > 1 => {
            let next = (session.accounts.current + 1) % session.accounts.list.len();
            switch_account(app, session, next);
        }
        KeyCode::BackTab if session.accounts.list.len() > 1 => {
            let len = session.accounts.list.len();
            let prev = (session.accounts.current + len - 1) % len;
            switch_account(app, session, prev);
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = c as usize - '1' as usize;
            if idx < session.accounts.list.len() && idx != session.accounts.current {
                switch_account(app, session, idx);
            }
        }
        _ => {}
    }
}

/// A bracketed paste: inserted as text wherever text is being typed, and
/// ignored everywhere else, so pasted text can never run as commands.
pub fn handle_paste(app: &mut App, text: &str, spellcheck: Option<&SpellCheckHandle>) {
    // Single-line fields take the text with line breaks flattened.
    let single_line = || {
        text.chars()
            .map(|c| if c == '\n' { ' ' } else { c })
            .filter(|c| !c.is_control())
    };

    if let Some((browser, _)) = &mut app.file_browser {
        if browser.filename_focused {
            if let Some(filename) = &mut browser.filename {
                single_line().for_each(|c| filename.insert(c));
                browser.error = None;
                browser.confirm_overwrite = None;
            }
        }
        return;
    }

    if let Some(compose) = &mut app.compose {
        if compose.sending {
            return;
        }
        match compose.focus {
            ComposeField::Body => match compose.vim.mode {
                VimMode::Insert => {
                    for c in text.chars() {
                        match c {
                            '\n' => compose.body.newline(),
                            '\t' => (0..4).for_each(|_| compose.body.insert(' ')),
                            c if c.is_control() => {}
                            c => compose.body.insert(c),
                        }
                    }
                    if let Some(handle) = spellcheck {
                        handle.request_full(compose.id, &compose.body.lines);
                    }
                }
                VimMode::Command(_) => single_line().for_each(|c| compose.vim.command_line.push(c)),
                _ => compose.notice = Some("Paste in Insert mode (press i first)".to_string()),
            },
            ComposeField::Attachments => {}
            _ => {
                single_line().for_each(|c| edit_input(compose, |f| f.insert(c)));
                compose.refresh_suggestions(&app.known_senders);
            }
        }
        return;
    }

    if app.search_editing {
        single_line().for_each(|c| app.edit_search(|s| s.insert(c)));
    }
}

/// Switches the active account: updates the index, resets whatever was
/// specific to the previous one, and kicks off both a cache read (fast,
/// for the "instant" feel) and a fresh sync (in case it's gone stale
/// since the startup sync).
fn switch_account(app: &mut App, session: &mut Session, index: usize) {
    session.accounts.current = index;
    app.current_account = index;
    app.reset_for_account_switch();

    let ctx = session.accounts.current().clone();
    bg::spawn_account_envelopes(ctx.clone(), session.tx.clone());
    bg::spawn_known_senders(ctx.clone(), session.tx.clone());
    bg::spawn_sync(ctx, session.tx.clone());
}

/// Checks whether the selection just landed on the last visible row with
/// nothing else in flight — the shared trigger for backfilling older mail,
/// used by every key that can reach the end of the list (`j`, `G`,
/// `Ctrl-d`). Only fires against the real, unfiltered list: reaching the
/// end of a search's matches doesn't mean the cache itself is exhausted.
fn maybe_load_more(app: &mut App, session: &Session) {
    if app.search().is_none() && app.has_more_older && !app.loading_more && app.is_at_end_of_list() {
        app.loading_more = true;
        bg::spawn_fetch_older(session.accounts.current().clone(), session.tx.clone());
    }
}

fn start_download(app: &mut App) {
    let BodyState::Loaded(message) = &app.body else { return };
    let Some(attachment) = message.attachments.get(app.selected_attachment) else {
        return;
    };

    // mail-core skips loading the content of a pathologically large
    // attachment (see MAX_ATTACHMENT_LOAD_BYTES) — bytes empty but a
    // nonzero size means "too large," not "actually empty."
    if attachment.bytes.is_empty() && attachment.size_bytes > 0 {
        app.set_status(format!(
            "{} is too large to download ({}).",
            attachment.filename,
            attach::human_size(attachment.size_bytes)
        ));
        return;
    }

    let browser = FileBrowser::open_for_save(default_download_dir(), attachment.filename.clone());
    let purpose = BrowserPurpose::SaveAttachment {
        bytes: attachment.bytes.clone(),
    };
    app.file_browser = Some((browser, purpose));
}

/// Marks the currently active account as the one that opens by default
/// on the next launch, persisted straight to config.toml. Local/sync file
/// I/O, so this stays synchronous rather than round-tripping through a
/// background task.
fn set_default_account(app: &mut App, session: &Session) {
    let email = session.accounts.current().account.config.email.clone();
    let result = mail_core::config::Config::load().and_then(|mut config| {
        config.default_account = Some(email.clone());
        config.save()
    });
    match result {
        Ok(()) => app.set_status(format!("{email} set as default account.")),
        Err(e) => app.set_status(format!("Failed to set default account: {e}")),
    }
}

fn handle_search_key(app: &mut App, code: KeyCode, session: &Session) {
    match code {
        KeyCode::Esc => app.clear_search(),
        KeyCode::Enter => {
            app.search_editing = false;
            // Local search came up empty — fall back to a server-side
            // search, since the cache is only ever a bounded recent
            // window and "not cached" doesn't mean "doesn't exist".
            if let Some(query) = app.search_query().map(str::to_string) {
                if app.visible_indices().is_empty() {
                    app.set_status(format!("Searching server for \"{query}\"..."));
                    bg::spawn_remote_search(session.accounts.current().clone(), query, session.tx.clone());
                }
            }
        }
        KeyCode::Char(c) => app.edit_search(|s| s.insert(c)),
        KeyCode::Backspace => app.edit_search(TextInput::backspace),
        KeyCode::Left => app.edit_search(TextInput::left),
        KeyCode::Right => app.edit_search(TextInput::right),
        _ => {}
    }
}

fn handle_compose_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, session: &Session) {
    let Some(compose) = &mut app.compose else { return };
    if compose.sending {
        return;
    }
    // Like vim's message line: whatever the last key reported goes away
    // on the next one.
    compose.notice = None;

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('s') | KeyCode::Char('S') => {
                start_send(compose, session);
                return;
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                app.file_browser = Some((FileBrowser::open(default_browse_dir()), BrowserPurpose::AttachToCompose));
                return;
            }
            // The body's editor has Ctrl chords of its own (Ctrl-r redo,
            // Ctrl-w/Ctrl-u in Insert mode, ...); the header fields have
            // none, so swallow them there rather than typing them.
            _ if compose.focus != ComposeField::Body => return,
            _ => {}
        }
    }

    if compose.focus == ComposeField::Body {
        handle_body_key(app, code, modifiers, session);
    } else {
        handle_header_key(app, code);
    }
}

/// Kicks off sending the current compose — shared by Ctrl+S and `:wq`/`:x`.
fn start_send(compose: &mut ComposeState, session: &Session) {
    compose.sending = true;
    compose.error = None;
    let draft = compose.to_draft();
    let attachments = compose.attachments.items.clone();
    bg::spawn_send(
        session.accounts.current().clone(),
        draft,
        attachments,
        session.tx.clone(),
    );
}

/// Body keys go to the vim engine, except Tab/Shift-Tab, which still move
/// between compose fields whatever mode the editor is in.
fn handle_body_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, session: &Session) {
    let Some(compose) = &mut app.compose else { return };
    match code {
        KeyCode::Tab => {
            compose.next_field();
            compose.refresh_suggestions(&app.known_senders);
            return;
        }
        KeyCode::BackTab => {
            compose.prev_field();
            compose.refresh_suggestions(&app.known_senders);
            return;
        }
        _ => {}
    }

    let before = compose.body.lines.clone();
    match compose.vim.handle_key(&mut compose.body, code, modifiers) {
        Outcome::None => {}
        Outcome::Notice(message) => compose.notice = Some(message),
        Outcome::Send => start_send(compose, session),
        Outcome::Quit { force } => {
            if force || compose.is_blank() {
                app.compose = None;
                return;
            }
            compose.notice = Some("Unsent draft — :wq to send, :q! to discard".to_string());
        }
    }

    // Recheck exactly what this key changed — whichever vim command (or
    // undo) did it — rather than guessing from the key itself.
    if let Some(handle) = &session.spellcheck {
        match body_change(&before, &compose.body.lines) {
            BodyChange::None => {}
            BodyChange::Line(row) => handle.request_line(compose.id, row, &compose.body.lines),
            BodyChange::Full => handle.request_full(compose.id, &compose.body.lines),
        }
    }
}

/// Keys for To/Cc/Bcc/Subject/Files: plain typing fields, with recipient
/// autocomplete on the address ones.
fn handle_header_key(app: &mut App, code: KeyCode) {
    let Some(compose) = &mut app.compose else { return };

    // While the recipient-suggestion dropdown is showing, a handful of
    // keys drive it instead of their usual field-editing meaning; anything
    // else (typing, Backspace, Left/Right, …) falls through unchanged so
    // the fragment being matched keeps updating live.
    let showing_suggestions = matches!(compose.focus, ComposeField::To | ComposeField::Cc | ComposeField::Bcc)
        && !compose.suggestions.items.is_empty();
    if showing_suggestions {
        match code {
            KeyCode::Up => {
                compose.suggestions.move_up();
                return;
            }
            KeyCode::Down => {
                compose.suggestions.move_down();
                return;
            }
            KeyCode::Enter | KeyCode::Tab => {
                compose.accept_suggestion();
                return;
            }
            KeyCode::Esc => {
                compose.suggestions.clear();
                return;
            }
            _ => {}
        }
    }

    match code {
        // Esc never discards a draft: it drops into the body in Normal
        // mode, where `:q!`/`:wq` live.
        KeyCode::Esc => compose.focus_body_normal(),
        KeyCode::Tab => compose.next_field(),
        KeyCode::BackTab => compose.prev_field(),
        KeyCode::Char(c) => edit_input(compose, |f| f.insert(c)),
        KeyCode::Backspace => {
            if compose.focus == ComposeField::Attachments {
                compose.attachments.remove_selected();
            } else {
                edit_input(compose, TextInput::backspace);
            }
        }
        KeyCode::Left => edit_input(compose, TextInput::left),
        KeyCode::Right => edit_input(compose, TextInput::right),
        KeyCode::Up if compose.focus == ComposeField::Attachments => compose.attachments.move_up(),
        KeyCode::Down if compose.focus == ComposeField::Attachments => compose.attachments.move_down(),
        KeyCode::Enter => compose.next_field(),
        _ => {}
    }

    // Left/Right/Up/Down never change what's typed in a recipient field
    // (they only move within it, or do nothing there) and never change
    // focus — skip the refresh for those so an idle cursor move doesn't
    // re-run the fuzzy match against the whole known-senders list for a
    // query that hasn't actually changed.
    if !matches!(code, KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down) {
        compose.refresh_suggestions(&app.known_senders);
    }
}

/// Applies a text edit to whichever single-line header field is focused.
fn edit_input(compose: &mut ComposeState, op: impl Fn(&mut TextInput)) {
    match compose.focus {
        ComposeField::To => op(&mut compose.to),
        ComposeField::Cc => op(&mut compose.cc),
        ComposeField::Bcc => op(&mut compose.bcc),
        ComposeField::Subject => op(&mut compose.subject),
        ComposeField::Attachments | ComposeField::Body => {}
    }
}

/// Handles the directory browser modal, used both for picking a file to
/// attach and for picking where to save a downloaded attachment. All of
/// this is local filesystem work (stat/read-dir/write), so it stays
/// synchronous rather than round-tripping through a background task.
fn handle_browser_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.file_browser = None;
            return;
        }
        KeyCode::Tab => {
            if let Some((browser, _)) = &mut app.file_browser {
                browser.toggle_focus();
            }
            return;
        }
        _ => {}
    }

    let filename_focused = app.file_browser.as_ref().is_some_and(|(b, _)| b.filename_focused);

    if filename_focused {
        match code {
            KeyCode::Left => edit_browser_filename(app, TextInput::left),
            KeyCode::Right => edit_browser_filename(app, TextInput::right),
            KeyCode::Char(c) => edit_browser_filename(app, move |f| f.insert(c)),
            KeyCode::Backspace => edit_browser_filename(app, TextInput::backspace),
            KeyCode::Enter => confirm_save(app),
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some((browser, _)) = &mut app.file_browser {
                browser.move_down();
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some((browser, _)) = &mut app.file_browser {
                browser.move_up();
            }
        }
        KeyCode::Enter => {
            let picked = app.file_browser.as_mut().and_then(|(b, _)| b.activate());
            if let Some(path) = picked {
                handle_browser_pick(app, path);
            }
        }
        _ => {}
    }
}

fn edit_browser_filename(app: &mut App, op: impl Fn(&mut TextInput)) {
    if let Some((browser, _)) = &mut app.file_browser {
        if let Some(filename) = &mut browser.filename {
            op(filename);
            // A pending "already exists" prompt was about the old name.
            browser.error = None;
            browser.confirm_overwrite = None;
        }
    }
}

/// A file was activated in the browser's list (Enter on a non-directory
/// entry — directories are already descended into by `activate()`).
fn handle_browser_pick(app: &mut App, path: PathBuf) {
    let is_attach = matches!(
        app.file_browser.as_ref().map(|(_, p)| p),
        Some(BrowserPurpose::AttachToCompose)
    );

    if is_attach {
        finish_attach(app, path);
    } else {
        // Save mode: picking an existing file prefills the filename field
        // with it and hands focus there for an explicit confirm, rather
        // than silently overwriting it.
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if let Some((browser, _)) = &mut app.file_browser {
            if let Some(name) = name {
                browser.filename = Some(TextInput::with_value(name));
            }
            browser.filename_focused = true;
        }
    }
}

fn finish_attach(app: &mut App, path: PathBuf) {
    let Some(compose) = &mut app.compose else {
        app.file_browser = None;
        return;
    };
    let existing_total = compose.attachments_total_bytes();

    match attach::validate(&path, existing_total) {
        Ok(entry) => {
            compose.attachments.items.push(entry);
            compose.attachments.selected = compose.attachments.items.len() - 1;
            app.file_browser = None;
        }
        Err(e) => {
            if let Some((browser, _)) = &mut app.file_browser {
                browser.error = Some(e);
            }
        }
    }
}

fn confirm_save(app: &mut App) {
    let Some((browser, BrowserPurpose::SaveAttachment { bytes })) = &mut app.file_browser else {
        return;
    };
    let Some(save_path) = browser.save_path() else { return };

    // Never silently replace an existing file: the first Enter asks, a
    // second one on the same path confirms.
    if save_path.exists() && browser.confirm_overwrite.as_ref() != Some(&save_path) {
        browser.error = Some(format!(
            "{} already exists — press Enter again to overwrite, or edit the name.",
            save_path.display()
        ));
        browser.confirm_overwrite = Some(save_path);
        return;
    }

    match std::fs::write(&save_path, bytes) {
        Ok(()) => {
            app.set_status(format!("Saved to {}", save_path.display()));
            app.file_browser = None;
        }
        Err(e) => {
            browser.confirm_overwrite = None;
            browser.error = Some(format!("{}: {e}", save_path.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_saving(dir: &Path, name: &str, bytes: &[u8]) -> App {
        let mut app = App::new(vec!["me@example.com".to_string()]);
        app.file_browser = Some((
            FileBrowser::open_for_save(dir.to_path_buf(), name.to_string()),
            BrowserPurpose::SaveAttachment { bytes: bytes.to_vec() },
        ));
        app
    }

    fn app() -> App {
        App::new(vec!["me@example.com".to_string()])
    }

    #[test]
    fn a_paste_in_the_message_list_runs_nothing() {
        let mut app = app();
        app.replace_envelopes(vec![mail_core::Envelope {
            uid: 1,
            subject: "x".into(),
            from: Vec::new(),
            to: Vec::new(),
            date: None,
            flags: Default::default(),
        }]);
        handle_paste(&mut app, "dyq", None);
        assert!(app.confirm_delete.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn a_paste_in_insert_mode_inserts_the_text_with_its_line_breaks() {
        let mut app = app();
        let mut compose = ComposeState::blank();
        compose.focus_body_normal();
        compose.vim.enter_insert(&compose.body);
        app.compose = Some(compose);

        handle_paste(&mut app, "hello\nworld", None);

        let body = &app.compose.as_ref().unwrap().body;
        assert_eq!(body.lines, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn a_paste_in_normal_mode_does_not_run_as_vim_commands() {
        let mut app = app();
        let mut compose = ComposeState::blank();
        compose.body.lines = vec!["keep me".to_string()];
        compose.focus_body_normal();
        app.compose = Some(compose);

        handle_paste(&mut app, "dd", None);

        let compose = app.compose.as_ref().unwrap();
        assert_eq!(compose.body.lines, vec!["keep me".to_string()]);
        assert!(compose.notice.is_some());
    }

    #[test]
    fn a_paste_into_a_header_field_flattens_line_breaks() {
        let mut app = app();
        let mut compose = ComposeState::blank();
        compose.focus = ComposeField::Subject;
        app.compose = Some(compose);

        handle_paste(&mut app, "two\nlines", None);

        assert_eq!(app.compose.as_ref().unwrap().subject.value, "two lines");
    }

    /// A one-account session over a throwaway store — enough for key
    /// handling paths that don't spawn background work.
    fn test_session(dir: &Path) -> Session {
        let store = mail_core::Store::open(&dir.join("mail.db")).unwrap();
        let account = mail_core::Account::new(mail_core::config::AccountConfig {
            email: "me@example.com".to_string(),
            display_name: None,
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            auth: Default::default(),
            fetch_limit: 50,
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        Session {
            accounts: crate::session::Accounts {
                list: vec![crate::session::AccountCtx {
                    account,
                    store,
                    account_id: 1,
                    folder_id: 1,
                }],
                current: 0,
            },
            tx,
            spellcheck: None,
        }
    }

    #[test]
    fn esc_backs_out_of_search_then_the_open_message_but_never_quits() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path());
        let mut app = app();
        app.start_search();
        app.search_editing = false; // confirmed filter, back in the list
        app.body = BodyState::Loading { uid: 1 };

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut session);
        assert!(app.search().is_none(), "first Esc clears the filter");
        assert!(matches!(app.body, BodyState::Loading { .. }));

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut session);
        assert!(matches!(app.body, BodyState::Empty), "second Esc closes the message");

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &mut session);
        assert!(!app.should_quit, "Esc never quits");

        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE, &mut session);
        assert!(app.should_quit);
    }

    #[test]
    fn saving_a_new_file_writes_it_straight_away() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_saving(dir.path(), "new.txt", b"hello");

        confirm_save(&mut app);

        assert_eq!(std::fs::read(dir.path().join("new.txt")).unwrap(), b"hello");
        assert!(app.file_browser.is_none());
    }

    #[test]
    fn overwriting_an_existing_file_needs_a_second_enter() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"old").unwrap();
        let mut app = app_saving(dir.path(), "a.txt", b"new");

        confirm_save(&mut app);
        assert_eq!(std::fs::read(&target).unwrap(), b"old", "the first Enter only asks");
        assert!(app.file_browser.as_ref().unwrap().0.error.is_some());

        confirm_save(&mut app);
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(app.file_browser.is_none());
    }

    #[test]
    fn editing_the_name_cancels_a_pending_overwrite_prompt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"old").unwrap();
        let mut app = app_saving(dir.path(), "a.txt", b"new");

        confirm_save(&mut app);
        edit_browser_filename(&mut app, TextInput::backspace);
        edit_browser_filename(&mut app, |f| f.insert('t'));
        assert!(app.file_browser.as_ref().unwrap().0.confirm_overwrite.is_none());

        confirm_save(&mut app); // "a.txt" again: must ask again
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"old");
    }
}
