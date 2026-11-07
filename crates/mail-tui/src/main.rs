mod app;
mod attach;
mod bg;
mod desktop_notify;
mod editable;
mod filebrowser;
mod fuzzy;
mod input;
mod reply;
mod session;
mod setup;
mod spellcheck;
mod theme;
mod ui;
mod vim;

use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use mail_core::{Account, AppEvent, Store};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::{App, ComposeField};
use bg::BgMsg;
use session::{AccountCtx, Accounts, Session, DISPLAY_LIMIT};
use vim::Mode as VimMode;

fn init_tracing() -> anyhow::Result<()> {
    let log_dir = mail_core::config::log_dir().context("could not determine the log directory")?;
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(log_dir, "mail-tui.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // Leak the guard for the process lifetime — logging should keep working
    // until the process exits, and this is a single long-lived binary.
    Box::leak(Box::new(guard));
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();
    Ok(())
}

async fn load_or_setup_accounts() -> anyhow::Result<Vec<Account>> {
    let config = mail_core::config::Config::load()?;
    if config.accounts.is_empty() {
        return Ok(vec![setup::run().await?]);
    }
    Ok(config.accounts.into_iter().map(Account::new).collect())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--set-password") {
        return setup::set_password();
    }
    if std::env::args().any(|a| a == "--add-account") {
        return setup::add_account().await;
    }
    if std::env::args().any(|a| a == "--remove-account") {
        return setup::remove_account().await;
    }
    if std::env::args().any(|a| a == "--set-default-account") {
        return setup::set_default_account();
    }

    init_tracing()?;
    let accounts = load_or_setup_accounts().await?;

    // A panic would otherwise leave the shell in raw mode on the alternate
    // screen, with the panic message itself unreadable. Restore first, then
    // let the default hook print it.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        tracing::error!("panic: {info}");
        default_hook(info);
    }));

    enable_raw_mode()?;
    // Bracketed paste delivers a paste as one event instead of a burst of
    // key presses — which in Normal mode or the message list would run as
    // commands (a pasted "d" then "y" deletes a message).
    execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, accounts).await;

    restore_terminal();
    result
}

/// Undoes everything `main` set up on the terminal. Best-effort and
/// idempotent, since it also runs from the panic hook.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        stdout(),
        DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen,
        Show
    );
}

async fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, accounts: Vec<Account>) -> anyhow::Result<()> {
    let config = mail_core::config::Config::load()?;
    let mut theme_rx = theme::watch::spawn(config.theme.mode.clone());
    let mut theme = *theme_rx.borrow_and_update();

    let store = Store::open(&mail_core::config::db_path()?)?;

    let mut list = Vec::with_capacity(accounts.len());
    for account in accounts {
        let account_id = store
            .upsert_account(account.config.email.clone(), account.config.display_name.clone())
            .await?;
        let inbox = store.get_or_create_folder(account_id, "INBOX".to_string()).await?;
        list.push(AccountCtx {
            account,
            store: store.clone(),
            account_id,
            folder_id: inbox.id,
            inbox_id: inbox.id,
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
    app.current_folder_id = accounts.current().folder_id;
    // The folder lists as last seen, so the sidebar is complete at once.
    for (index, ctx) in accounts.list.iter().enumerate() {
        match store.list_folders(ctx.account_id).await {
            Ok(folders) => app.set_folders(index, folders),
            Err(e) => tracing::warn!("reading cached folders failed: {e}"),
        }
    }

    // Render instantly from whatever's cached, then reconcile with the
    // server in the background — this is what makes launch feel instant
    // even before the first sync of a session completes.
    let current = accounts.current();
    match current.store.list_envelopes(current.folder_id, DISPLAY_LIMIT).await {
        Ok(envelopes) => app.apply_cached_envelopes(envelopes),
        Err(e) => app.set_status(format!("Couldn't read the local cache: {e}")),
    }

    let (tx, mut rx) = mpsc::channel::<BgMsg>(8);
    let (app_event_tx, mut app_events) = mpsc::unbounded_channel::<AppEvent>();

    bg::spawn_known_senders(accounts.current().clone(), tx.clone());
    bg::spawn_outbox_counts(accounts.current().clone(), tx.clone());
    bg::spawn_clean_open_dir();

    // Spellcheck starts with the first compose (see `bg::spawn_spellchecker_reload`),
    // so launch never waits on `hyprctl`/`hunspell`.
    let mut session = Session {
        accounts,
        tx,
        spellcheck: None,
    };

    // Every account gets synced (which also flushes its outbox) concurrently at
    // startup, not just whichever is currently shown — so switching to
    // one later is likely to already have fresh data waiting in the
    // cache instead of starting cold. Each also gets its own persistent
    // IDLE connection so new mail shows up as it arrives rather than only
    // on the next manual/account-switch sync.
    for account in &session.accounts.list {
        bg::spawn_sync(account.clone(), session.tx.clone());
        bg::spawn_refresh_folders(account.clone(), session.tx.clone());
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

    let mut cursor_shape = None;

    loop {
        // Clear an expired status message *before* drawing — otherwise
        // the redraw that fires exactly when the tick elapses still
        // shows the stale message for one more frame, and (since the
        // next tick then becomes the idle interval) it could show stale
        // text for up to another heartbeat.
        let tick = app.expire_status().unwrap_or(IDLE_REDRAW_INTERVAL);

        // Keep what's being written safe from a crash or a closed terminal.
        if let Some(saved) = app.compose.as_mut().and_then(|c| c.autosave_due(Instant::now())) {
            bg::spawn_save_draft(session.accounts.current(), saved, false, session.tx.clone());
        }

        terminal.draw(|frame| ui::draw(frame, &app, &theme))?;

        // Only written when it changes — it's an escape sequence to the
        // terminal, not something worth re-sending every frame.
        let wanted = cursor_shape_for(&app);
        if cursor_shape != Some(wanted) {
            let _ = execute!(terminal.backend_mut(), wanted.style());
            cursor_shape = Some(wanted);
        }

        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        input::handle_key(&mut app, key.code, key.modifiers, &mut session);
                    }
                    Some(Ok(Event::Paste(text))) => input::handle_paste(&mut app, &text, session.spellcheck.as_ref()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::error!("terminal event error: {e}");
                    }
                    None => return Ok(()),
                }
            }
            Some(msg) = rx.recv() => bg::apply(&mut app, &mut session, msg),
            Some(event) = app_events.recv() => bg::apply_app_event(&session, event),
            Ok(()) = theme_rx.changed() => {
                theme = *theme_rx.borrow_and_update();
            }
            _ = tokio::time::sleep(tick) => {}
        }
    }
}

/// The terminal cursor's shape, following the vim convention: a bar where
/// typing inserts text, a block over the character Normal/Visual commands
/// act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorShape {
    Default,
    Bar,
    Block,
}

impl CursorShape {
    fn style(self) -> SetCursorStyle {
        match self {
            Self::Default => SetCursorStyle::DefaultUserShape,
            Self::Bar => SetCursorStyle::SteadyBar,
            Self::Block => SetCursorStyle::SteadyBlock,
        }
    }
}

fn cursor_shape_for(app: &App) -> CursorShape {
    match &app.compose {
        Some(compose) if compose.focus == ComposeField::Body => match compose.vim.mode {
            VimMode::Insert | VimMode::Command(_) => CursorShape::Bar,
            VimMode::Normal | VimMode::Visual | VimMode::VisualLine => CursorShape::Block,
        },
        Some(_) => CursorShape::Bar,
        None => CursorShape::Default,
    }
}
