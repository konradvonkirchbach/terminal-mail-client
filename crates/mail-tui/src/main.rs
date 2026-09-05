mod app;
mod editable;
mod setup;
mod theme;
mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use mail_core::send::SendOutcome;
use mail_core::{Account, Envelope, Message, Store};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::{App, BodyState, ComposeField, ComposeState, ListState};
use editable::{TextArea, TextInput};

/// Everything a background task needs to talk to IMAP and the local
/// cache. Cheap to clone — `Account`/`Store` are just handles.
#[derive(Clone)]
struct Ctx {
    account: Account,
    store: Store,
    account_id: i64,
    folder_id: i64,
    fetch_limit: u32,
}

enum BgMsg {
    /// A sync finished (or failed) — carries the freshly re-read cached
    /// envelope list so the UI never has to reason about deltas itself.
    SyncDone(Result<Vec<Envelope>, String>),
    Body(u32, Result<Message, String>),
    SendDone(Result<SendOutcome, String>),
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
    if std::env::args().any(|a| a == "--set-password") {
        return setup::set_password();
    }

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

    let store = Store::open(&mail_core::config::db_path()?)?;
    let account_id = store
        .upsert_account(
            account.config.email.clone(),
            account.config.display_name.clone(),
        )
        .await?;
    let folder = store
        .get_or_create_folder(account_id, "INBOX".to_string())
        .await?;

    let fetch_limit = account.config.fetch_limit;
    let ctx = Ctx {
        account,
        store,
        account_id,
        folder_id: folder.id,
        fetch_limit,
    };

    let mut app = App::new(ctx.account.config.email.clone());

    // Render instantly from whatever's cached, then reconcile with the
    // server in the background — this is what makes launch feel instant
    // even before the first sync of a session completes.
    match ctx.store.list_envelopes(ctx.folder_id, ctx.fetch_limit).await {
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

    spawn_sync(ctx.clone(), tx.clone());
    spawn_flush_outbox(ctx.clone());

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
                        handle_key(&mut app, key.code, key.modifiers, &ctx, tx.clone());
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
                    Some(BgMsg::SyncDone(Ok(envelopes))) => {
                        let selected_uid = app.selected_uid();
                        app.envelopes = envelopes;
                        app.list_state = ListState::Loaded;
                        // Keep the cursor on the same message across a
                        // resync instead of snapping back to the top.
                        app.selected = selected_uid
                            .and_then(|uid| app.envelopes.iter().position(|e| e.uid == uid))
                            .unwrap_or(0);
                    }
                    Some(BgMsg::SyncDone(Err(e))) => {
                        app.list_state = ListState::Error(e);
                    }
                    Some(BgMsg::Body(uid, Ok(message))) if app.selected_uid() == Some(uid) => {
                        app.body = BodyState::Loaded(message);
                    }
                    Some(BgMsg::Body(uid, Err(e))) if app.selected_uid() == Some(uid) => {
                        app.body = BodyState::Error(e);
                    }
                    Some(BgMsg::Body(..)) => {}
                    Some(BgMsg::SendDone(Ok(outcome))) => {
                        app.compose = None;
                        app.status_message = Some(match outcome {
                            SendOutcome::Sent => "Sent.".to_string(),
                            SendOutcome::Queued => "Offline — queued, will retry.".to_string(),
                        });
                    }
                    Some(BgMsg::SendDone(Err(e))) => {
                        if let Some(compose) = &mut app.compose {
                            compose.sending = false;
                            compose.error = Some(e);
                        }
                    }
                    None => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, ctx: &Ctx, tx: mpsc::Sender<BgMsg>) {
    if app.compose.is_some() {
        handle_compose_key(app, code, modifiers, ctx, tx);
        return;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Char('S') => {
            app.list_state = ListState::Loading;
            spawn_sync(ctx.clone(), tx);
        }
        KeyCode::Char('c') => {
            app.status_message = None;
            app.compose = Some(ComposeState::blank());
        }
        KeyCode::Char('r') => {
            let reply = app.selected_envelope().map(ComposeState::reply);
            if let Some(reply) = reply {
                app.status_message = None;
                app.compose = Some(reply);
            }
        }
        KeyCode::Enter => {
            if let Some(uid) = app.selected_uid() {
                app.body = BodyState::Loading;
                spawn_fetch_body(ctx.clone(), uid, tx);
            }
        }
        _ => {}
    }
}

fn handle_compose_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, ctx: &Ctx, tx: mpsc::Sender<BgMsg>) {
    let Some(compose) = &mut app.compose else { return };
    if compose.sending {
        return;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('s') | KeyCode::Char('S')) {
        compose.sending = true;
        compose.error = None;
        let draft = compose.to_draft();
        spawn_send(ctx.clone(), draft, tx);
        return;
    }

    match code {
        KeyCode::Esc => app.compose = None,
        KeyCode::Tab => compose.next_field(),
        KeyCode::BackTab => compose.prev_field(),
        KeyCode::Char(c) => edit_field(compose, |f| f.insert(c), |a| a.insert(c)),
        KeyCode::Backspace => edit_field(compose, TextInput::backspace, TextArea::backspace),
        KeyCode::Left => edit_field(compose, TextInput::left, TextArea::left),
        KeyCode::Right => edit_field(compose, TextInput::right, TextArea::right),
        KeyCode::Up => {
            if let ComposeField::Body = compose.focus {
                compose.body.up();
            }
        }
        KeyCode::Down => {
            if let ComposeField::Body = compose.focus {
                compose.body.down();
            }
        }
        KeyCode::Enter => match compose.focus {
            ComposeField::Body => compose.body.newline(),
            _ => compose.next_field(),
        },
        _ => {}
    }
}

/// Dispatches an edit to whichever field is focused — the single-line
/// fields share one `TextInput` op, the body uses the matching `TextArea`
/// op.
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
        ComposeField::Body => area_op(&mut compose.body),
    }
}

fn spawn_sync(ctx: Ctx, tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let sync_result = mail_core::sync::sync_inbox(&ctx.account, &ctx.store).await;
        let result = match sync_result {
            Ok(_) => ctx
                .store
                .list_envelopes(ctx.folder_id, ctx.fetch_limit)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(BgMsg::SyncDone(result)).await;
    });
}

fn spawn_fetch_body(ctx: Ctx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let result = mail_core::sync::fetch_body(
            &ctx.account,
            &ctx.store,
            ctx.account_id,
            ctx.folder_id,
            uid,
        )
        .await
        .map_err(|e| e.to_string());
        let _ = tx.send(BgMsg::Body(uid, result)).await;
    });
}

fn spawn_send(ctx: Ctx, draft: mail_core::Draft, tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let result = mail_core::send::send_message(&ctx.account, &ctx.store, ctx.account_id, &draft)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(BgMsg::SendDone(result)).await;
    });
}

fn spawn_flush_outbox(ctx: Ctx) {
    tokio::spawn(async move {
        match mail_core::send::flush_outbox(&ctx.account, &ctx.store, ctx.account_id).await {
            Ok(n) if n > 0 => tracing::info!("flushed {n} queued message(s) from the outbox"),
            Ok(_) => {}
            Err(e) => tracing::warn!("outbox flush failed: {e}"),
        }
    });
}
