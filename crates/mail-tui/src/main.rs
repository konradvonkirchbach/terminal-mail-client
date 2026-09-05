mod app;
mod attach;
mod desktop_notify;
mod editable;
mod filebrowser;
mod fuzzy;
mod setup;
mod spellcheck;
mod theme;
mod ui;

use std::future::Future;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use mail_core::send::SendOutcome;
use mail_core::{Account, Address, AppEvent, AttachmentFile, Envelope, Message, Store};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::{App, BodyState, BrowserPurpose, ComposeField, ComposeState, ListState};
use attach::AttachmentEntry;
use editable::{TextArea, TextInput};
use filebrowser::FileBrowser;

/// Ceiling on how many cached envelopes we'll ever read back from the
/// store for display in one go. Deliberately not the same as
/// `fetch_limit` (which sizes one "page" fetched from the server, for
/// both the initial sync and each "load more"): `fetch_limit` bounds a
/// single network round-trip, while this just needs to be generous
/// enough to show everything pagination has accumulated so far without
/// an unbounded query against a mailbox synced over a very long time.
const DISPLAY_LIMIT: u32 = 5000;

/// Everything a background task needs to talk to IMAP and the local
/// cache for one account. Cheap to clone — `Account`/`Store` are just
/// handles.
#[derive(Clone)]
struct AccountCtx {
    account: Account,
    store: Store,
    account_id: i64,
    folder_id: i64,
}

/// Every configured account plus which one is currently shown. Lives only
/// in `run()`'s stack frame — background tasks get a cloned `AccountCtx`
/// for one specific account, never this whole collection.
struct Accounts {
    list: Vec<AccountCtx>,
    current: usize,
}

impl Accounts {
    fn current(&self) -> &AccountCtx {
        &self.list[self.current]
    }
}

/// Everything key-handling needs beyond the specific key itself and the
/// mutable `App` UI state: the account list, the channel back to this
/// event loop, and whichever optional session-scoped background
/// capabilities (like spellcheck) happen to be available. Grouped here
/// instead of as separate parameters so the next such capability doesn't
/// grow every key-handling function's signature again.
struct Session {
    accounts: Accounts,
    tx: mpsc::Sender<BgMsg>,
    spellcheck: Option<spellcheck::SpellCheckHandle>,
}

enum BgMsg {
    /// A sync finished (or failed) for some account — carries the
    /// freshly re-read cached envelope list so the UI never has to
    /// reason about deltas itself. Applied only if that account is still
    /// the one being viewed when it arrives.
    SyncDone { account_id: i64, result: Result<Vec<Envelope>, String> },
    /// The cached envelope list for an account just switched into —
    /// distinct from `SyncDone` since this is a plain cache read, not a
    /// network sync (that's kicked off separately, see `switch_account`).
    AccountEnvelopes { account_id: i64, result: Result<Vec<Envelope>, String> },
    Body { account_id: i64, uid: u32, result: Result<Message, String> },
    /// Unlike the other variants, sending isn't tagged by account: the
    /// compose view captures its own `AccountCtx` at send time and blocks
    /// all other key handling (including account switching) until it
    /// resolves, so there's no other account it could apply to.
    SendDone(Result<SendOutcome, String>),
    MessageDeleted { account_id: i64, uid: u32, result: Result<(), String> },
    /// A "load older mail" backfill finished — `result` is how many
    /// envelopes were found (`0` means the real start of the mailbox has
    /// been reached, so the caller should stop trying).
    FetchedOlder { account_id: i64, result: Result<usize, String> },
    /// A server-side search finished. Carries the query it was run for so
    /// the handler can drop a stale result if the user has since changed
    /// or cleared the search box.
    RemoteSearchDone { account_id: i64, query: String, result: Result<Vec<Envelope>, String> },
    /// A read of the local known-senders address book finished, for
    /// compose's recipient autocomplete. A failure is silently dropped —
    /// suggestions are a nice-to-have, not worth a status-bar error.
    KnownSenders { account_id: i64, result: Result<Vec<Address>, String> },
    /// A spellcheck pass over a single body line finished — `line` is
    /// which one (patched in place, since most edits don't shift line
    /// indices), `words` maps each misspelled word found to Hunspell's
    /// suggested corrections for it. Not tagged to a particular compose
    /// session: if compose has since closed, or its line count has since
    /// changed underneath this now-stale index, `set_line_misspellings`
    /// just grows to fit or the message is silently dropped.
    SpellCheckLine { line: usize, words: std::collections::HashMap<String, Vec<String>> },
    /// A spellcheck pass over the whole body finished — one entry per
    /// line, in order, wholesale-replacing the compose's per-line state.
    /// Sent instead of `SpellCheckLine` after an edit that changes the
    /// line count, since a single patched index could no longer line up.
    SpellCheckFull(Vec<std::collections::HashMap<String, Vec<String>>>),
    /// A (re)detection of the dictionary matching the active keyboard
    /// layout finished — replaces whatever spellchecker was running.
    /// Fired at startup and again every time compose opens, so switching
    /// keyboard layout before writing a new email picks up the matching
    /// dictionary; `None` if no installed dictionary matches (or none is
    /// installed at all), which simply turns spellcheck off.
    SpellCheckerReady(Option<spellcheck::SpellCheckHandle>),
}

fn init_tracing() -> anyhow::Result<()> {
    let dirs = directories::ProjectDirs::from("", "", "email_client")
        .context("could not determine state directory")?;
    let log_dir = dirs.state_dir().unwrap_or_else(|| dirs.cache_dir()).join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(log_dir, "mail-tui.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // Leak the guard for the process lifetime — logging should keep working
    // until the process exits, and this is a single long-lived binary.
    Box::leak(Box::new(guard));
    tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).init();
    Ok(())
}

fn load_or_setup_accounts() -> anyhow::Result<Vec<Account>> {
    let config = mail_core::config::Config::load()?;
    if config.accounts.is_empty() {
        return Ok(vec![setup::run()?]);
    }
    Ok(config.accounts.into_iter().map(Account::new).collect())
}

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--set-password") {
        return setup::set_password();
    }
    if std::env::args().any(|a| a == "--add-account") {
        return setup::add_account();
    }
    if std::env::args().any(|a| a == "--remove-account") {
        return setup::remove_account().await;
    }
    if std::env::args().any(|a| a == "--set-default-account") {
        return setup::set_default_account();
    }

    init_tracing()?;
    let accounts = load_or_setup_accounts()?;

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, accounts).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    accounts: Vec<Account>,
) -> anyhow::Result<()> {
    let config = mail_core::config::Config::load()?;
    let mut theme_rx = theme::watch::spawn(config.theme.mode.clone());
    let mut theme = *theme_rx.borrow_and_update();

    let store = Store::open(&mail_core::config::db_path()?)?;

    let mut list = Vec::with_capacity(accounts.len());
    for account in accounts {
        let account_id = store
            .upsert_account(account.config.email.clone(), account.config.display_name.clone())
            .await?;
        let folder = store.get_or_create_folder(account_id, "INBOX".to_string()).await?;
        list.push(AccountCtx {
            account,
            store: store.clone(),
            account_id,
            folder_id: folder.id,
        });
    }
    let current = config
        .default_account
        .as_deref()
        .and_then(|email| list.iter().position(|a| a.account.config.email == email))
        .unwrap_or(0);
    let accounts = Accounts { list, current };

    let mut app = App::new(accounts.list.iter().map(|a| a.account.config.email.clone()).collect());
    app.current_account = accounts.current;

    // Render instantly from whatever's cached, then reconcile with the
    // server in the background — this is what makes launch feel instant
    // even before the first sync of a session completes.
    let current = accounts.current();
    match current.store.list_envelopes(current.folder_id, DISPLAY_LIMIT).await {
        Ok(envelopes) => {
            app.list_state = if envelopes.is_empty() {
                ListState::Loading
            } else {
                ListState::Loaded
            };
            app.envelopes = envelopes;
        }
        Err(e) => app.list_state = ListState::Error(e.to_string()),
    }

    let (tx, mut rx) = mpsc::channel::<BgMsg>(8);
    let (app_event_tx, mut app_events) = mpsc::unbounded_channel::<AppEvent>();

    spawn_known_senders(accounts.current().clone(), tx.clone());

    // Compose's spellcheck rides on whatever Hunspell dictionary matches
    // the language actually being typed in — entirely optional, so a
    // missing binary or dictionary just leaves it off for the session
    // instead of failing startup. Re-detected on every compose open too
    // (see the 'c'/'r' key handlers), so switching keyboard layout
    // before writing a new email picks up the matching dictionary.
    let spellcheck_handle = spawn_spellchecker(tx.clone()).await;
    if spellcheck_handle.is_none() {
        let lang = spellcheck::active_language();
        app.set_status(format!(
            "Spellcheck unavailable — install hunspell and a matching dictionary (e.g. hunspell-{lang}) to enable it."
        ));
    }

    let mut session = Session { accounts, tx, spellcheck: spellcheck_handle };

    // Every account gets synced (and its outbox flushed) concurrently at
    // startup, not just whichever is currently shown — so switching to
    // one later is likely to already have fresh data waiting in the
    // cache instead of starting cold. Each also gets its own persistent
    // IDLE connection so new mail shows up as it arrives rather than only
    // on the next manual/account-switch sync.
    for account in &session.accounts.list {
        spawn_sync(account.clone(), session.tx.clone());
        spawn_flush_outbox(account.clone());
        tokio::spawn(mail_core::idle::run(
            account.account.clone(),
            account.store.clone(),
            app_event_tx.clone(),
        ));
    }

    let mut events = EventStream::new();

    // ratatui re-queries the real terminal size on every draw call, so a
    // resize corrects itself as soon as *something* triggers another
    // redraw. Terminal emulators are expected to report that as a
    // crossterm `Event::Resize` — but some WM/terminal-emulator
    // combinations (a tiling WM resizing the window rather than the user
    // dragging its edge) don't reliably deliver that, and without a key
    // or background event in the meantime we'd otherwise sit on the old
    // size indefinitely. Redrawing on an idle heartbeat instead of a long
    // sleep makes that self-heal within a fraction of a second, at a
    // negligible idle-CPU cost.
    const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(500);

    loop {
        // Clear an expired status message *before* drawing — otherwise
        // the redraw that fires exactly when the tick elapses still
        // shows the stale message for one more frame, and (since the
        // next tick then becomes the idle interval) it could show stale
        // text for up to another heartbeat.
        let tick = app.expire_status().unwrap_or(IDLE_REDRAW_INTERVAL);

        terminal.draw(|frame| ui::draw(frame, &app, &theme))?;

        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key.code, key.modifiers, &mut session);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::error!("terminal event error: {e}");
                    }
                    None => return Ok(()),
                }
            }
            msg = rx.recv() => {
                match msg {
                    Some(BgMsg::SyncDone { account_id, result }) => {
                        if account_id != session.accounts.current().account_id {
                            // Synced quietly in the background; the cache
                            // is updated either way, so switching to this
                            // account later will pick it up fresh.
                        } else {
                            match result {
                                Ok(envelopes) => {
                                    let selected_uid = app.selected_uid();
                                    app.envelopes = envelopes;
                                    app.list_state = ListState::Loaded;
                                    // Keep the cursor on the same message
                                    // across a resync instead of snapping
                                    // back to the top.
                                    app.selected = selected_uid
                                        .and_then(|uid| app.envelopes.iter().position(|e| e.uid == uid))
                                        .unwrap_or(0);
                                    // A sync can introduce new senders —
                                    // keep compose's autocomplete current.
                                    spawn_known_senders(session.accounts.current().clone(), session.tx.clone());
                                }
                                Err(e) => {
                                    tracing::error!("sync failed: {e}");
                                    app.list_state = ListState::Error(e);
                                }
                            }
                        }
                    }
                    Some(BgMsg::AccountEnvelopes { account_id, result }) if account_id == session.accounts.current().account_id => {
                        match result {
                            Ok(envelopes) => {
                                app.list_state = if envelopes.is_empty() { ListState::Loading } else { ListState::Loaded };
                                app.envelopes = envelopes;
                            }
                            Err(e) => app.list_state = ListState::Error(e),
                        }
                    }
                    Some(BgMsg::AccountEnvelopes { .. }) => {}
                    Some(BgMsg::Body { account_id, uid, result: Ok(message) })
                        if account_id == session.accounts.current().account_id && app.selected_uid() == Some(uid) =>
                    {
                        app.selected_attachment = 0;
                        app.body = BodyState::Loaded(message);
                    }
                    Some(BgMsg::Body { account_id, uid, result: Err(e) })
                        if account_id == session.accounts.current().account_id && app.selected_uid() == Some(uid) =>
                    {
                        tracing::error!("fetching body for uid {uid} failed: {e}");
                        app.body = BodyState::Error(e);
                    }
                    Some(BgMsg::Body { .. }) => {}
                    Some(BgMsg::SendDone(Ok(outcome))) => {
                        app.compose = None;
                        app.set_status(match outcome {
                            SendOutcome::Sent => "Sent.".to_string(),
                            SendOutcome::Queued => "Offline — queued, will retry.".to_string(),
                        });
                    }
                    Some(BgMsg::SendDone(Err(e))) => {
                        tracing::error!("send failed: {e}");
                        if let Some(compose) = &mut app.compose {
                            compose.sending = false;
                            compose.error = Some(e);
                        }
                    }
                    Some(BgMsg::MessageDeleted { account_id, uid, result }) if account_id == session.accounts.current().account_id => {
                        match result {
                            Ok(()) => {
                                app.remove_envelope(uid);
                                app.set_status("Message deleted.".to_string());
                            }
                            Err(e) => {
                                tracing::error!("delete failed: {e}");
                                app.set_status(format!("Delete failed: {e}"));
                            }
                        }
                    }
                    Some(BgMsg::MessageDeleted { .. }) => {}
                    Some(BgMsg::FetchedOlder { account_id, result })
                        if account_id == session.accounts.current().account_id =>
                    {
                        app.loading_more = false;
                        match result {
                            Ok(0) => app.has_more_older = false,
                            Ok(_) => spawn_account_envelopes(session.accounts.current().clone(), session.tx.clone()),
                            Err(e) => {
                                tracing::error!("fetch older failed: {e}");
                                app.set_status(format!("Couldn't load more mail: {e}"));
                            }
                        }
                    }
                    Some(BgMsg::FetchedOlder { .. }) => {}
                    Some(BgMsg::KnownSenders { account_id, result: Ok(senders) })
                        if account_id == session.accounts.current().account_id =>
                    {
                        app.known_senders = senders;
                    }
                    Some(BgMsg::KnownSenders { .. }) => {}
                    Some(BgMsg::SpellCheckLine { line, words }) => {
                        if let Some(compose) = &mut app.compose {
                            compose.set_line_misspellings(line, words);
                        }
                    }
                    Some(BgMsg::SpellCheckFull(by_line)) => {
                        if let Some(compose) = &mut app.compose {
                            compose.set_all_misspellings(by_line);
                        }
                    }
                    Some(BgMsg::SpellCheckerReady(handle)) => {
                        session.spellcheck = handle;
                    }
                    Some(BgMsg::RemoteSearchDone { account_id, query, result }) => {
                        let still_relevant = account_id == session.accounts.current().account_id
                            && app.search.as_ref().is_some_and(|s| s.value.trim() == query);
                        if still_relevant {
                            match result {
                                Ok(envelopes) => {
                                    let existing: std::collections::HashSet<u32> =
                                        app.envelopes.iter().map(|e| e.uid).collect();
                                    let new_envelopes: Vec<Envelope> = envelopes
                                        .into_iter()
                                        .filter(|e| !existing.contains(&e.uid))
                                        .collect();
                                    if new_envelopes.is_empty() {
                                        app.set_status(format!(
                                            "No matches found on the server for \"{query}\"."
                                        ));
                                    } else {
                                        app.set_status(format!(
                                            "Found {} more match(es) on the server.",
                                            new_envelopes.len()
                                        ));
                                        app.envelopes.extend(new_envelopes);
                                        app.envelopes.sort_by_key(|e| std::cmp::Reverse(e.uid));
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("remote search failed: {e}");
                                    app.set_status(format!("Server search failed: {e}"));
                                }
                            }
                        }
                    }
                    None => {}
                }
            }
            Some(AppEvent::NewMail { account_id, new_envelopes, .. }) = app_events.recv() => {
                // The IDLE loop already wrote the fresh data straight to
                // the store; only re-read into the UI if this is the
                // account currently being viewed. A background account's
                // cache is still updated either way, ready for whenever
                // the user switches to it.
                if account_id == session.accounts.current().account_id {
                    spawn_account_envelopes(session.accounts.current().clone(), session.tx.clone());
                    spawn_known_senders(session.accounts.current().clone(), session.tx.clone());
                }
                // Notifications fire regardless of which account is being
                // viewed — arguably more useful for one that isn't.
                if let Some(account) = session.accounts.list.iter().find(|a| a.account_id == account_id) {
                    desktop_notify::notify_new_mail(&account.account.config.email, &new_envelopes);
                }
            }
            Ok(()) = theme_rx.changed() => {
                theme = *theme_rx.borrow_and_update();
            }
            _ = tokio::time::sleep(tick) => {}
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, session: &mut Session) {
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
            app.set_status("Deleting...".to_string());
            spawn_delete_message(session.accounts.current().clone(), uid, session.tx.clone());
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
        KeyCode::Esc => {
            if app.search.is_some() {
                app.clear_search();
            } else {
                app.should_quit = true;
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
        KeyCode::Char('/') => app.start_search(),
        KeyCode::Char('S') => {
            app.list_state = ListState::Loading;
            spawn_sync(session.accounts.current().clone(), session.tx.clone());
        }
        KeyCode::Char('c') => {
            app.status_message = None;
            app.compose = Some(ComposeState::blank());
            spawn_spellchecker_reload(session.tx.clone());
        }
        KeyCode::Char('r') => {
            let reply = app.selected_envelope().map(ComposeState::reply);
            if let Some(reply) = reply {
                app.status_message = None;
                app.compose = Some(reply);
                spawn_spellchecker_reload(session.tx.clone());
            }
        }
        KeyCode::Enter => {
            if let Some(uid) = app.selected_uid() {
                app.body = BodyState::Loading;
                spawn_fetch_body(session.accounts.current().clone(), uid, session.tx.clone());
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
            switch_account(app, session, (session.accounts.current + 1) % session.accounts.list.len());
        }
        KeyCode::BackTab if session.accounts.list.len() > 1 => {
            switch_account(
                app,
                session,
                (session.accounts.current + session.accounts.list.len() - 1) % session.accounts.list.len(),
            );
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

/// Switches the active account: updates the index, resets whatever was
/// specific to the previous one, and kicks off both a cache read (fast,
/// for the "instant" feel) and a fresh sync (in case it's gone stale
/// since the startup sync).
fn switch_account(app: &mut App, session: &mut Session, index: usize) {
    session.accounts.current = index;
    app.current_account = index;
    app.reset_for_account_switch();

    let ctx = session.accounts.current().clone();
    spawn_account_envelopes(ctx.clone(), session.tx.clone());
    spawn_known_senders(ctx.clone(), session.tx.clone());
    spawn_sync(ctx, session.tx.clone());
}

/// Checks whether the selection just landed on the last visible row with
/// nothing else in flight — the shared trigger for backfilling older mail,
/// used by every key that can reach the end of the list (`j`, `G`,
/// `Ctrl-d`). Only fires against the real, unfiltered list: reaching the
/// end of a search's matches doesn't mean the cache itself is exhausted.
fn maybe_load_more(app: &mut App, session: &Session) {
    if app.search.is_none() && app.has_more_older && !app.loading_more && app.is_at_end_of_list() {
        app.loading_more = true;
        spawn_fetch_older(session.accounts.current().clone(), session.tx.clone());
    }
}

fn start_download(app: &mut App) {
    let BodyState::Loaded(message) = &app.body else { return };
    let Some(attachment) = message.attachments.get(app.selected_attachment) else { return };

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
            let query = app.search.as_ref().map(|s| s.value.trim().to_string()).unwrap_or_default();
            if !query.is_empty() && app.visible_indices().is_empty() {
                app.set_status(format!("Searching server for \"{query}\"..."));
                spawn_remote_search(session.accounts.current().clone(), query, session.tx.clone());
            }
        }
        KeyCode::Char(c) => {
            if let Some(search) = &mut app.search {
                search.insert(c);
            }
            app.selected = 0;
        }
        KeyCode::Backspace => {
            if let Some(search) = &mut app.search {
                search.backspace();
            }
            app.selected = 0;
        }
        KeyCode::Left => {
            if let Some(search) = &mut app.search {
                search.left();
            }
        }
        KeyCode::Right => {
            if let Some(search) = &mut app.search {
                search.right();
            }
        }
        _ => {}
    }
}

fn handle_compose_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, session: &Session) {
    let Some(compose) = &mut app.compose else { return };
    if compose.sending {
        return;
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('s') | KeyCode::Char('S') => {
                compose.sending = true;
                compose.error = None;
                let draft = compose.to_draft();
                let attachments = compose.attachments.items.clone();
                spawn_send(session.accounts.current().clone(), draft, attachments, session.tx.clone());
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                app.file_browser = Some((FileBrowser::open(default_browse_dir()), BrowserPurpose::AttachToCompose));
            }
            _ => {} // swallow unrecognized ctrl-chords rather than typing them literally
        }
        return;
    }

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

    // A Backspace at the very start of a body line joins it with the
    // previous one, shrinking the line count — captured before the edit
    // runs so the spellcheck trigger below knows to fall back to a full
    // recheck instead of patching a single (now wrong) line index.
    let backspace_will_join_body_lines = compose.focus == ComposeField::Body
        && code == KeyCode::Backspace
        && compose.body.cursor_col == 0
        && compose.body.cursor_row > 0;

    match code {
        // Returns immediately rather than falling through to the shared
        // `refresh_suggestions` call below — `compose` no longer points
        // at anything live once `app.compose` is cleared.
        KeyCode::Esc => {
            app.compose = None;
            return;
        }
        KeyCode::Tab => compose.next_field(),
        KeyCode::BackTab => compose.prev_field(),
        KeyCode::Char(c) => edit_field(compose, |f| f.insert(c), |a| a.insert(c)),
        KeyCode::Backspace => {
            if compose.focus == ComposeField::Attachments {
                compose.attachments.remove_selected();
            } else {
                edit_field(compose, TextInput::backspace, TextArea::backspace);
            }
        }
        KeyCode::Left => edit_field(compose, TextInput::left, TextArea::left),
        KeyCode::Right => edit_field(compose, TextInput::right, TextArea::right),
        KeyCode::Up => match compose.focus {
            ComposeField::Body => compose.body.up(),
            ComposeField::Attachments => compose.attachments.move_up(),
            _ => {}
        },
        KeyCode::Down => match compose.focus {
            ComposeField::Body => compose.body.down(),
            ComposeField::Attachments => compose.attachments.move_down(),
            _ => {}
        },
        KeyCode::Enter => match compose.focus {
            ComposeField::Body => compose.body.newline(),
            _ => compose.next_field(),
        },
        _ => {}
    }

    compose.refresh_suggestions(&app.known_senders);

    // Most edits only touch one body line, so only that line needs a
    // recheck; an edit that changes the line count (Enter, or a
    // line-joining Backspace) shifts every later line's index, so that
    // case rechecks the whole body instead of patching a single line.
    if compose.focus == ComposeField::Body {
        if let Some(handle) = &session.spellcheck {
            match code {
                KeyCode::Char(_) => {
                    handle.request_line(compose.body.cursor_row, compose.body.lines[compose.body.cursor_row].clone());
                }
                KeyCode::Backspace if backspace_will_join_body_lines => {
                    handle.request_full(compose.body.text());
                }
                KeyCode::Backspace => {
                    handle.request_line(compose.body.cursor_row, compose.body.lines[compose.body.cursor_row].clone());
                }
                KeyCode::Enter => handle.request_full(compose.body.text()),
                _ => {}
            }
        }
    }
}

/// Dispatches an edit to whichever field is focused — the single-line
/// fields share one `TextInput` op, the body uses the matching `TextArea`
/// op. Attachments has no text of its own (handled separately above).
fn edit_field(
    compose: &mut ComposeState,
    input_op: impl Fn(&mut TextInput),
    area_op: impl Fn(&mut TextArea),
) {
    match compose.focus {
        ComposeField::To => input_op(&mut compose.to),
        ComposeField::Cc => input_op(&mut compose.cc),
        ComposeField::Bcc => input_op(&mut compose.bcc),
        ComposeField::Subject => input_op(&mut compose.subject),
        ComposeField::Attachments => {}
        ComposeField::Body => area_op(&mut compose.body),
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

    let filename_focused = app
        .file_browser
        .as_ref()
        .map(|(b, _)| b.filename_focused)
        .unwrap_or(false);

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
    let Some((browser, purpose)) = &app.file_browser else { return };
    let BrowserPurpose::SaveAttachment { bytes, .. } = purpose else { return };
    let Some(save_path) = browser.save_path() else { return };
    let bytes = bytes.clone();

    match std::fs::write(&save_path, &bytes) {
        Ok(()) => {
            app.set_status(format!("Saved to {}", save_path.display()));
            app.file_browser = None;
        }
        Err(e) => {
            if let Some((browser, _)) = &mut app.file_browser {
                browser.error = Some(format!("{}: {e}", save_path.display()));
            }
        }
    }
}

/// Spawns `fut` and, once it resolves, hands its `Result` (with the `Err`
/// reduced to its `Display` string — the shape every `BgMsg` variant's
/// `result` field uses) to `to_msg` to build the message sent back over
/// `tx`. Nearly every "kick off some mail-core work in the background,
/// report one `BgMsg` when it's done" spawn shares exactly this shape;
/// factoring it out here means the next one is a short call instead of a
/// copy-pasted `tokio::spawn` block.
fn spawn_bg<T, E, Fut>(tx: mpsc::Sender<BgMsg>, fut: Fut, to_msg: impl FnOnce(Result<T, String>) -> BgMsg + Send + 'static)
where
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    E: std::fmt::Display,
    T: Send + 'static,
{
    tokio::spawn(async move {
        let result = fut.await.map_err(|e| e.to_string());
        let _ = tx.send(to_msg(result)).await;
    });
}

fn spawn_sync(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move {
            mail_core::sync::sync_inbox(&ctx.account, &ctx.store).await?;
            ctx.store.list_envelopes(ctx.folder_id, DISPLAY_LIMIT).await
        },
        move |result| BgMsg::SyncDone { account_id, result },
    );
}

fn spawn_account_envelopes(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { ctx.store.list_envelopes(ctx.folder_id, DISPLAY_LIMIT).await },
        move |result| BgMsg::AccountEnvelopes { account_id, result },
    );
}

fn spawn_fetch_body(ctx: AccountCtx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::fetch_body(&ctx.account, &ctx.store, ctx.account_id, ctx.folder_id, uid).await },
        move |result| BgMsg::Body { account_id, uid, result },
    );
}

/// How many older messages to backfill per "scrolled to the bottom"
/// trigger — deliberately the same size as a normal sync page.
const LOAD_MORE_BATCH: u32 = 50;

fn spawn_fetch_older(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::fetch_older(&ctx.account, &ctx.store, ctx.folder_id, LOAD_MORE_BATCH).await },
        move |result| BgMsg::FetchedOlder { account_id, result },
    );
}

/// How many server-side search matches to pull back and cache, newest
/// first — bounded so a single common word can't drag down a huge chunk
/// of a large mailbox.
const REMOTE_SEARCH_LIMIT: usize = 200;

fn spawn_remote_search(ctx: AccountCtx, query: String, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    let query_for_msg = query.clone();
    spawn_bg(
        tx,
        async move { mail_core::sync::search_remote(&ctx.account, &ctx.store, ctx.folder_id, &query, REMOTE_SEARCH_LIMIT).await },
        move |result| BgMsg::RemoteSearchDone { account_id, query: query_for_msg, result },
    );
}

fn spawn_known_senders(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { ctx.store.known_senders(ctx.account_id).await },
        move |result| BgMsg::KnownSenders { account_id, result },
    );
}

/// Spawns the Hunspell process for whichever dictionary matches the
/// language currently being typed in (see `spellcheck::active_language`)
/// along with the task that feeds it text and reports results back over
/// `tx`. Returns `None` — spellcheck simply stays off — when there's no
/// `hunspell` binary or no dictionary installed for that language; see
/// `spellcheck.rs`.
async fn spawn_spellchecker(tx: mpsc::Sender<BgMsg>) -> Option<spellcheck::SpellCheckHandle> {
    let dict = spellcheck::find_dictionary_for_active_language(&spellcheck::default_search_dirs())?;
    let mut checker = spellcheck::SpellChecker::spawn(std::slice::from_ref(&dict)).await.ok()?;

    let (request_tx, mut request_rx) = tokio::sync::watch::channel(spellcheck::SpellCheckRequest::default());
    tokio::spawn(async move {
        loop {
            if request_rx.changed().await.is_err() {
                return; // every SpellCheckHandle was dropped
            }
            let request = request_rx.borrow_and_update().clone();
            let msg = match request {
                spellcheck::SpellCheckRequest::None => continue,
                spellcheck::SpellCheckRequest::Line { index, text } => {
                    match spellcheck::check_one_line(&mut checker, &text).await {
                        Ok(words) => BgMsg::SpellCheckLine { line: index, words },
                        Err(e) => {
                            tracing::warn!("spellcheck failed, disabling for the rest of the session: {e}");
                            return;
                        }
                    }
                }
                spellcheck::SpellCheckRequest::Full(text) => {
                    // `split('\n')` rather than `.lines()`: it's the
                    // exact inverse of how `TextArea::text()` joins body
                    // lines, so an empty or trailing-blank line still
                    // produces the right number of entries to stay
                    // index-aligned with `body.lines`.
                    let mut by_line = Vec::new();
                    let mut failed = None;
                    for line in text.split('\n') {
                        match spellcheck::check_one_line(&mut checker, line).await {
                            Ok(words) => by_line.push(words),
                            Err(e) => {
                                failed = Some(e);
                                break;
                            }
                        }
                    }
                    match failed {
                        None => BgMsg::SpellCheckFull(by_line),
                        Some(e) => {
                            tracing::warn!("spellcheck failed, disabling for the rest of the session: {e}");
                            return;
                        }
                    }
                }
            };
            let _ = tx.send(msg).await;
        }
    });

    Some(spellcheck::SpellCheckHandle::new(request_tx))
}

/// Re-detects the active keyboard layout and (re)spawns a spellchecker
/// for whatever dictionary now matches it, replacing whatever was
/// running — called every time compose opens, so switching layout
/// before writing a new email is picked up. Always spawns fresh rather
/// than checking whether the language actually changed first: starting
/// a new Hunspell process is cheap (tens of milliseconds) and this only
/// runs once per compose session, not per keystroke.
fn spawn_spellchecker_reload(tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let handle = spawn_spellchecker(tx.clone()).await;
        let _ = tx.send(BgMsg::SpellCheckerReady(handle)).await;
    });
}

fn spawn_delete_message(ctx: AccountCtx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::delete_message(&ctx.account, &ctx.store, ctx.folder_id, uid).await },
        move |result| BgMsg::MessageDeleted { account_id, uid, result },
    );
}

fn spawn_send(ctx: AccountCtx, draft: mail_core::Draft, attachments: Vec<AttachmentEntry>, tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let result = send_with_attachments(&ctx, &draft, &attachments)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(BgMsg::SendDone(result)).await;
    });
}

async fn send_with_attachments(
    ctx: &AccountCtx,
    draft: &mail_core::Draft,
    attachments: &[AttachmentEntry],
) -> mail_core::Result<SendOutcome> {
    let mut files = Vec::with_capacity(attachments.len());
    for a in attachments {
        let bytes = tokio::fs::read(&a.path).await?;
        files.push(AttachmentFile {
            filename: a.filename.clone(),
            bytes,
        });
    }
    mail_core::send::send_message(&ctx.account, &ctx.store, ctx.account_id, draft, &files).await
}

fn spawn_flush_outbox(ctx: AccountCtx) {
    tokio::spawn(async move {
        match mail_core::send::flush_outbox(&ctx.account, &ctx.store, ctx.account_id).await {
            Ok(n) if n > 0 => tracing::info!("flushed {n} queued message(s) from the outbox"),
            Ok(_) => {}
            Err(e) => tracing::warn!("outbox flush failed: {e}"),
        }
    });
}
