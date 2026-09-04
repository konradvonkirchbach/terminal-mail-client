mod app;
mod setup;
mod theme;
mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use mail_core::{Account, Envelope, Message};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::{App, BodyState, ListState};

enum BgMsg {
    Envelopes(Result<Vec<Envelope>, String>),
    Body(u32, Result<Message, String>),
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

fn load_or_setup_account() -> anyhow::Result<Account> {
    let config = mail_core::config::Config::load()?;
    if let Some(account_config) = config.accounts.into_iter().next() {
        return Ok(Account::new(account_config));
    }
    setup::run()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    let account = load_or_setup_account()?;

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, account).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    account: Account,
) -> anyhow::Result<()> {
    let config = mail_core::config::Config::load()?;
    let theme = theme::resolve(&config.theme.mode);

    let mut app = App::new(account.config.email.clone());

    let (tx, mut rx) = mpsc::channel::<BgMsg>(8);

    spawn_fetch_envelopes(account.clone(), tx.clone());

    let mut events = EventStream::new();

    loop {
        terminal.draw(|frame| ui::draw(frame, &app, &theme))?;

        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key.code, &account, tx.clone());
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
                    Some(BgMsg::Envelopes(Ok(envelopes))) => {
                        app.envelopes = envelopes;
                        app.selected = 0;
                        app.list_state = ListState::Loaded;
                    }
                    Some(BgMsg::Envelopes(Err(e))) => {
                        app.list_state = ListState::Error(e);
                    }
                    Some(BgMsg::Body(uid, Ok(message))) if app.selected_uid() == Some(uid) => {
                        app.body = BodyState::Loaded(message);
                    }
                    Some(BgMsg::Body(uid, Err(e))) if app.selected_uid() == Some(uid) => {
                        app.body = BodyState::Error(e);
                    }
                    Some(BgMsg::Body(..)) => {}
                    None => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, account: &Account, tx: mpsc::Sender<BgMsg>) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Enter => {
            if let Some(uid) = app.selected_uid() {
                app.body = BodyState::Loading;
                spawn_fetch_body(account.clone(), uid, tx);
            }
        }
        _ => {}
    }
}

fn spawn_fetch_envelopes(account: Account, tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let result = mail_core::imap::fetch_envelopes(&account)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(BgMsg::Envelopes(result)).await;
    });
}

fn spawn_fetch_body(account: Account, uid: u32, tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let result = mail_core::imap::fetch_message(&account, uid)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(BgMsg::Body(uid, result)).await;
    });
}
