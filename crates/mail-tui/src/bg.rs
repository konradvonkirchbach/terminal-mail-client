//! Background work: the `spawn_*` helpers that kick off mail-core calls
//! off the event loop, the `BgMsg` each reports back with, and `apply`,
//! which folds a finished result into the UI state.

use std::collections::HashMap;
use std::future::Future;

use mail_core::send::SendOutcome;
use mail_core::{Address, AppEvent, AttachmentFile, Envelope, Message};
use tokio::sync::mpsc;

use crate::app::App;
use crate::attach::AttachmentEntry;
use crate::desktop_notify;
use crate::session::{AccountCtx, Session, DISPLAY_LIMIT};
use crate::spellcheck;

pub enum BgMsg {
    /// A sync finished (or failed) for some account — carries the
    /// freshly re-read cached envelope list so the UI never has to
    /// reason about deltas itself. Applied only if that account is still
    /// the one being viewed when it arrives.
    SyncDone {
        account_id: i64,
        result: Result<Vec<Envelope>, String>,
    },
    /// The cached envelope list for an account, re-read after an account
    /// switch, new mail, or a backfill — distinct from `SyncDone` since
    /// this is a plain cache read, not a network sync.
    AccountEnvelopes {
        account_id: i64,
        result: Result<Vec<Envelope>, String>,
    },
    Body {
        account_id: i64,
        uid: u32,
        result: Result<Message, String>,
    },
    /// Unlike the other variants, sending isn't tagged by account: the
    /// compose view captures its own `AccountCtx` at send time and blocks
    /// all other key handling (including account switching) until it
    /// resolves, so there's no other account it could apply to.
    SendDone(Result<SendOutcome, String>),
    MessageDeleted {
        account_id: i64,
        uid: u32,
        result: Result<(), String>,
    },
    /// A "load older mail" backfill finished — `result` is how many
    /// envelopes were found (`0` means the real start of the mailbox has
    /// been reached, so the caller should stop trying).
    FetchedOlder {
        account_id: i64,
        result: Result<usize, String>,
    },
    /// A server-side search finished. Carries the query it was run for so
    /// the handler can drop a stale result if the user has since changed
    /// or cleared the search box.
    RemoteSearchDone {
        account_id: i64,
        query: String,
        result: Result<Vec<Envelope>, String>,
    },
    /// A read of the local known-senders address book finished, for
    /// compose's recipient autocomplete. A failure is silently dropped —
    /// suggestions are a nice-to-have, not worth a status-bar error.
    KnownSenders {
        account_id: i64,
        result: Result<Vec<Address>, String>,
    },
    /// A spellcheck pass over a single body line finished — `line` is
    /// which one (patched in place, since most edits don't shift line
    /// indices), `words` maps each misspelled word found to Hunspell's
    /// suggested corrections for it. `compose_id` is checked against the
    /// live compose's `ComposeState::id` before applying: if compose has
    /// since closed and possibly been replaced by an unrelated new one,
    /// this stale result is simply dropped instead of silently patching
    /// text the user never typed in this session.
    SpellCheckLine {
        compose_id: u64,
        line: usize,
        words: HashMap<String, Vec<String>>,
    },
    /// A spellcheck pass over the whole body finished — one entry per
    /// line, in order, wholesale-replacing the compose's per-line state.
    /// Sent instead of `SpellCheckLine` after an edit that changes the
    /// line count, since a single patched index could no longer line up.
    /// `compose_id` is checked the same way as `SpellCheckLine`'s.
    SpellCheckFull {
        compose_id: u64,
        by_line: Vec<HashMap<String, Vec<String>>>,
    },
    /// A (re)detection of the dictionary matching the active keyboard
    /// layout finished — replaces whatever spellchecker was running.
    /// Fired at startup and again every time compose opens, so switching
    /// keyboard layout before writing a new email picks up the matching
    /// dictionary; `None` if no installed dictionary matches (or none is
    /// installed at all), which simply turns spellcheck off.
    SpellCheckerReady(Option<spellcheck::SpellCheckHandle>),
}

/// Folds one finished background result into the UI state. Results for an
/// account other than the one being viewed are dropped (its cache was
/// updated either way, so switching to it later picks them up); the
/// decisions about *how* a result changes the view live in `App`'s own
/// methods, where they're unit tested.
pub fn apply(app: &mut App, session: &mut Session, msg: BgMsg) {
    match msg {
        BgMsg::SyncDone { account_id, result } => {
            if !session.accounts.is_current(account_id) {
                return;
            }
            if let Err(e) = &result {
                tracing::error!("sync failed: {e}");
            }
            let synced = result.is_ok();
            app.apply_sync_result(result);
            if synced {
                // A sync can introduce new senders — keep compose's
                // autocomplete current.
                spawn_known_senders(session.accounts.current().clone(), session.tx.clone());
            }
        }
        BgMsg::AccountEnvelopes { account_id, result } => {
            if !session.accounts.is_current(account_id) {
                return;
            }
            match result {
                Ok(envelopes) => app.apply_cached_envelopes(envelopes),
                Err(e) => {
                    tracing::error!("reading cached envelopes failed: {e}");
                    app.set_status(format!("Couldn't read the local cache: {e}"));
                }
            }
        }
        BgMsg::Body {
            account_id,
            uid,
            result,
        } => {
            if !session.accounts.is_current(account_id) {
                return;
            }
            if let Err(e) = &result {
                tracing::error!("fetching body for uid {uid} failed: {e}");
            }
            app.finish_body(uid, result);
        }
        BgMsg::SendDone(Ok(outcome)) => {
            app.compose = None;
            app.set_status(match outcome {
                SendOutcome::Sent => "Sent.".to_string(),
                SendOutcome::Queued => "Offline — queued, will retry after the next sync.".to_string(),
            });
        }
        BgMsg::SendDone(Err(e)) => {
            tracing::error!("send failed: {e}");
            if let Some(compose) = &mut app.compose {
                compose.sending = false;
                compose.error = Some(e);
            }
        }
        BgMsg::MessageDeleted {
            account_id,
            uid,
            result,
        } => {
            if !session.accounts.is_current(account_id) {
                return;
            }
            match result {
                Ok(()) => {
                    app.remove_envelope(uid);
                    app.set_status("Message deleted.");
                }
                Err(e) => {
                    tracing::error!("delete failed: {e}");
                    app.set_status(format!("Delete failed: {e}"));
                }
            }
        }
        BgMsg::FetchedOlder { account_id, result } => {
            if !session.accounts.is_current(account_id) {
                return;
            }
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
        BgMsg::KnownSenders { account_id, result } => {
            if let (true, Ok(senders)) = (session.accounts.is_current(account_id), result) {
                app.known_senders = senders;
            }
        }
        BgMsg::SpellCheckLine {
            compose_id,
            line,
            words,
        } => {
            if let Some(compose) = app.compose.as_mut().filter(|c| c.id == compose_id) {
                compose.set_line_misspellings(line, words);
            }
        }
        BgMsg::SpellCheckFull { compose_id, by_line } => {
            if let Some(compose) = app.compose.as_mut().filter(|c| c.id == compose_id) {
                compose.set_all_misspellings(by_line);
            }
        }
        BgMsg::SpellCheckerReady(handle) => {
            session.spellcheck = handle;
        }
        BgMsg::RemoteSearchDone {
            account_id,
            query,
            result,
        } => {
            let still_relevant = session.accounts.is_current(account_id) && app.search_query() == Some(query.as_str());
            if !still_relevant {
                return;
            }
            match result {
                Ok(envelopes) => match app.merge_envelopes(envelopes) {
                    0 => app.set_status(format!("No matches found on the server for \"{query}\".")),
                    n => app.set_status(format!("Found {n} more match(es) on the server.")),
                },
                Err(e) => {
                    tracing::error!("remote search failed: {e}");
                    app.set_status(format!("Server search failed: {e}"));
                }
            }
        }
    }
}

/// Reacts to a mail-core change notification from an IDLE loop.
pub fn apply_app_event(session: &Session, event: AppEvent) {
    match event {
        AppEvent::NewMail {
            account_id,
            new_envelopes,
            ..
        } => {
            // The IDLE loop already wrote the fresh data straight to the
            // store; only re-read into the UI if this is the account
            // currently being viewed.
            if session.accounts.is_current(account_id) {
                spawn_account_envelopes(session.accounts.current().clone(), session.tx.clone());
                spawn_known_senders(session.accounts.current().clone(), session.tx.clone());
            }
            // Notifications fire regardless of which account is being
            // viewed — arguably more useful for one that isn't.
            if let Some(account) = session.accounts.find(account_id) {
                desktop_notify::notify_new_mail(&account.account.config.email, &new_envelopes);
            }
        }
    }
}

/// Spawns `fut` and, once it resolves, hands its `Result` (with the `Err`
/// reduced to its `Display` string — the shape every `BgMsg` variant's
/// `result` field uses) to `to_msg` to build the message sent back over
/// `tx`. Nearly every "kick off some mail-core work in the background,
/// report one `BgMsg` when it's done" spawn shares exactly this shape.
fn spawn_bg<T, E, Fut>(
    tx: mpsc::Sender<BgMsg>,
    fut: Fut,
    to_msg: impl FnOnce(Result<T, String>) -> BgMsg + Send + 'static,
) where
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    E: std::fmt::Display,
    T: Send + 'static,
{
    tokio::spawn(async move {
        let result = fut.await.map_err(|e| e.to_string());
        let _ = tx.send(to_msg(result)).await;
    });
}

pub fn spawn_sync(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move {
            mail_core::sync::sync_inbox(&ctx.account, &ctx.store).await?;
            // Reachable again: retry anything queued while offline, without
            // holding up the list refresh.
            spawn_flush_outbox(ctx.clone());
            ctx.store.list_envelopes(ctx.folder_id, DISPLAY_LIMIT).await
        },
        move |result| BgMsg::SyncDone { account_id, result },
    );
}

pub fn spawn_account_envelopes(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { ctx.store.list_envelopes(ctx.folder_id, DISPLAY_LIMIT).await },
        move |result| BgMsg::AccountEnvelopes { account_id, result },
    );
}

pub fn spawn_fetch_body(ctx: AccountCtx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::fetch_body(&ctx.account, &ctx.store, ctx.account_id, ctx.folder_id, uid).await },
        move |result| BgMsg::Body {
            account_id,
            uid,
            result,
        },
    );
}

/// How many older messages to backfill per "scrolled to the bottom"
/// trigger — deliberately the same size as a normal sync page.
const LOAD_MORE_BATCH: u32 = 50;

pub fn spawn_fetch_older(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
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

pub fn spawn_remote_search(ctx: AccountCtx, query: String, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    let query_for_msg = query.clone();
    spawn_bg(
        tx,
        async move {
            mail_core::sync::search_remote(&ctx.account, &ctx.store, ctx.folder_id, &query, REMOTE_SEARCH_LIMIT).await
        },
        move |result| BgMsg::RemoteSearchDone {
            account_id,
            query: query_for_msg,
            result,
        },
    );
}

pub fn spawn_known_senders(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { ctx.store.known_senders(ctx.account_id).await },
        move |result| BgMsg::KnownSenders { account_id, result },
    );
}

pub fn spawn_delete_message(ctx: AccountCtx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::delete_message(&ctx.account, &ctx.store, ctx.folder_id, uid).await },
        move |result| BgMsg::MessageDeleted {
            account_id,
            uid,
            result,
        },
    );
}

pub fn spawn_send(
    ctx: AccountCtx,
    draft: mail_core::Draft,
    attachments: Vec<AttachmentEntry>,
    tx: mpsc::Sender<BgMsg>,
) {
    spawn_bg(
        tx,
        async move { send_with_attachments(&ctx, &draft, &attachments).await },
        BgMsg::SendDone,
    );
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

pub fn spawn_flush_outbox(ctx: AccountCtx) {
    tokio::spawn(async move {
        match mail_core::send::flush_outbox(&ctx.account, &ctx.store, ctx.account_id).await {
            Ok(n) if n > 0 => tracing::info!("flushed {n} queued message(s) from the outbox"),
            Ok(_) => {}
            Err(e) => tracing::warn!("outbox flush failed: {e}"),
        }
    });
}

/// Spawns the Hunspell process for whichever dictionary matches the
/// language currently being typed in (see `spellcheck::active_language`)
/// along with the task that feeds it text and reports results back over
/// `tx`. Returns `None` — spellcheck simply stays off — when there's no
/// `hunspell` binary or no dictionary installed for that language; see
/// `spellcheck.rs`.
pub async fn spawn_spellchecker(tx: mpsc::Sender<BgMsg>) -> Option<spellcheck::SpellCheckHandle> {
    let dict = spellcheck::find_dictionary_for_active_language(&spellcheck::default_search_dirs()).await?;
    let mut checker = spellcheck::SpellChecker::spawn(std::slice::from_ref(&dict))
        .await
        .ok()?;

    let (handle, mut requests) = spellcheck::requests();
    tokio::spawn(async move {
        // Ends once the handle is dropped (compose reopened with a fresh
        // checker, or the app exiting), taking the Hunspell process with it.
        while let Some(request) = requests.next().await {
            let msg = match request {
                spellcheck::SpellCheckRequest::None => continue,
                spellcheck::SpellCheckRequest::Line {
                    compose_id,
                    index,
                    text,
                } => match spellcheck::check_one_line(&mut checker, &text).await {
                    Ok(words) => BgMsg::SpellCheckLine {
                        compose_id,
                        line: index,
                        words,
                    },
                    Err(e) => {
                        tracing::warn!("spellcheck failed, disabling for the rest of the session: {e}");
                        return;
                    }
                },
                spellcheck::SpellCheckRequest::Full { compose_id, text } => {
                    // `split('\n')` rather than `.lines()`: it's the
                    // exact inverse of how `TextArea::text()` joins body
                    // lines, so an empty or trailing-blank line still
                    // produces the right number of entries to stay
                    // index-aligned with `body.lines`.
                    let mut by_line = Vec::new();
                    for line in text.split('\n') {
                        match spellcheck::check_one_line(&mut checker, line).await {
                            Ok(words) => by_line.push(words),
                            Err(e) => {
                                tracing::warn!("spellcheck failed, disabling for the rest of the session: {e}");
                                return;
                            }
                        }
                    }
                    BgMsg::SpellCheckFull { compose_id, by_line }
                }
            };
            let _ = tx.send(msg).await;
        }
    });

    Some(handle)
}

/// Re-detects the active keyboard layout and (re)spawns a spellchecker
/// for whatever dictionary now matches it, replacing whatever was
/// running — called every time compose opens, so switching layout
/// before writing a new email is picked up. Always spawns fresh rather
/// than checking whether the language actually changed first: starting
/// a new Hunspell process is cheap (tens of milliseconds) and this only
/// runs once per compose session, not per keystroke.
pub fn spawn_spellchecker_reload(tx: mpsc::Sender<BgMsg>) {
    tokio::spawn(async move {
        let handle = spawn_spellchecker(tx.clone()).await;
        let _ = tx.send(BgMsg::SpellCheckerReady(handle)).await;
    });
}
