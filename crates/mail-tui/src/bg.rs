//! Background work: the `spawn_*` helpers that kick off mail-core calls
//! off the event loop, the `BgMsg` each reports back with, and `apply`,
//! which folds a finished result into the UI state.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use mail_core::send::{FlushReport, SendOutcome};
use mail_core::sync::DeleteOutcome;
use mail_core::{Address, AppEvent, AttachmentFile, Envelope, FlagKind, Folder, Message, OutboxCounts, SavedDraft};
use tokio::sync::mpsc;

use crate::app::{App, BodyState, Selectable};
use crate::attach::{AttachmentEntry, AttachmentSource};
use crate::desktop_notify;
use crate::session::{AccountCtx, Session, DISPLAY_LIMIT};
use crate::spellcheck;

pub enum BgMsg {
    /// A sync of one folder finished (or failed) — carries the freshly
    /// re-read cached envelope list so the UI never has to reason about
    /// deltas itself. Applied only if that folder is still the one being
    /// viewed when it arrives.
    SyncDone {
        account_id: i64,
        folder_id: i64,
        result: Result<Vec<Envelope>, String>,
    },
    /// The cached envelope list for a folder, re-read after a switch, new
    /// mail, or a backfill — distinct from `SyncDone` since this is a plain
    /// cache read, not a network sync.
    AccountEnvelopes {
        account_id: i64,
        folder_id: i64,
        result: Result<Vec<Envelope>, String>,
    },
    /// An account's folder list, from the cache or a refresh from the
    /// server, for the sidebar.
    Folders {
        account_id: i64,
        result: Result<Vec<Folder>, String>,
    },
    Body {
        account_id: i64,
        folder_id: i64,
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
        folder_id: i64,
        uid: u32,
        result: Result<DeleteOutcome, String>,
    },
    /// Setting or clearing a flag on the server finished. The list already
    /// shows the change; a failure undoes it.
    FlagChanged {
        account_id: i64,
        folder_id: i64,
        uid: u32,
        flag: FlagKind,
        on: bool,
        result: Result<(), String>,
    },
    /// A "load older mail" backfill finished — `result` is how many
    /// envelopes were found (`0` means the real start of the mailbox has
    /// been reached, so the caller should stop trying).
    FetchedOlder {
        account_id: i64,
        folder_id: i64,
        result: Result<usize, String>,
    },
    /// A server-side search finished. Carries the query it was run for so
    /// the handler can drop a stale result if the user has since changed
    /// or cleared the search box.
    RemoteSearchDone {
        account_id: i64,
        folder_id: i64,
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
    /// Saving an attachment (extracting it, then writing `path`) finished.
    AttachmentSaved { path: PathBuf, result: Result<(), String> },
    /// A (re)detection of the dictionary matching the active keyboard
    /// layout finished — replaces whatever spellchecker was running.
    /// Fired every time compose opens, so switching keyboard layout before
    /// writing a new email picks up the matching dictionary. `handle` is
    /// `None` if no installed dictionary matches `language` (or Hunspell
    /// isn't installed at all), which turns spellcheck off.
    SpellCheckerReady {
        handle: Option<spellcheck::SpellCheckHandle>,
        language: String,
    },
    /// Saving the draft stored under `key` finished. `announce` is set for
    /// a save the user asked for (`:w`, closing), which gets confirmed.
    DraftSaved {
        key: String,
        announce: bool,
        result: Result<(), String>,
    },
    /// The saved drafts, read for the drafts picker.
    Drafts {
        account_id: i64,
        result: Result<Vec<SavedDraft>, String>,
    },
    OutboxCounts {
        account_id: i64,
        result: Result<OutboxCounts, String>,
    },
    /// An outbox flush finished; `manual` if the user asked for it.
    OutboxFlushed {
        account_id: i64,
        manual: bool,
        result: Result<FlushReport, String>,
    },
    /// Handing an attachment or HTML body to the desktop's default app
    /// finished.
    Opened { name: String, result: Result<(), String> },
}

/// Folds one finished background result into the UI state. Results for a
/// folder other than the one being viewed are dropped (its cache was
/// updated either way, so switching to it later picks them up); the
/// decisions about *how* a result changes the view live in `App`'s own
/// methods, where they're unit tested.
pub fn apply(app: &mut App, session: &mut Session, msg: BgMsg) {
    match msg {
        BgMsg::SyncDone {
            account_id,
            folder_id,
            result,
        } => {
            if result.is_ok() {
                // The sync recorded the folder's new unread count.
                if let Some(ctx) = session.accounts.find(account_id) {
                    spawn_load_folders(ctx.clone(), session.tx.clone());
                }
            }
            if !session.accounts.is_current_folder(account_id, folder_id) {
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
        BgMsg::AccountEnvelopes {
            account_id,
            folder_id,
            result,
        } => {
            if !session.accounts.is_current_folder(account_id, folder_id) {
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
        BgMsg::Folders { account_id, result } => match (session.accounts.index_of(account_id), result) {
            (Some(index), Ok(folders)) => app.set_folders(index, folders),
            (_, Err(e)) => tracing::warn!("reading folders failed: {e}"),
            (None, Ok(_)) => {}
        },
        BgMsg::Body {
            account_id,
            folder_id,
            uid,
            result,
        } => {
            if !session.accounts.is_current_folder(account_id, folder_id) {
                return;
            }
            if let Err(e) = &result {
                tracing::error!("fetching body for uid {uid} failed: {e}");
                if app.pending_respond.is_some_and(|(pending, _)| pending == uid) {
                    app.set_status(format!("Couldn't load the message: {e}"));
                }
            }
            app.finish_body(uid, result);
            // A reply or forward was waiting for exactly this body (unless
            // something else was opened over the main view meanwhile).
            if let (Some((pending, kind)), BodyState::Loaded(message)) = (app.pending_respond, &app.body) {
                if pending == uid && message.uid == uid && app.compose.is_none() && app.drafts.is_none() {
                    app.pending_respond = None;
                    crate::input::start_respond(app, session, kind);
                }
            }
        }
        BgMsg::SendDone(Ok(outcome)) => {
            if let Some(compose) = app.compose.take() {
                let ctx = session.accounts.current().clone();
                spawn_delete_draft(&ctx, compose.draft_key);
                if let Some((folder_id, uid)) = compose.answers {
                    if session.accounts.is_current_folder(ctx.account_id, folder_id) {
                        app.set_envelope_flag(uid, FlagKind::Answered, true);
                    }
                    spawn_set_flag(ctx, folder_id, uid, FlagKind::Answered, true, session.tx.clone());
                }
            }
            app.set_status(match outcome {
                SendOutcome::Sent => "Sent.".to_string(),
                SendOutcome::Queued => "Offline — queued, will retry after the next sync (F to retry now).".to_string(),
            });
            if outcome == SendOutcome::Queued {
                spawn_outbox_counts(session.accounts.current().clone(), session.tx.clone());
            }
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
            folder_id,
            uid,
            result,
        } => {
            if !session.accounts.is_current_folder(account_id, folder_id) {
                return;
            }
            match result {
                Ok(outcome) => {
                    app.remove_envelope(uid);
                    app.set_status(match outcome {
                        DeleteOutcome::MovedToTrash => "Moved to Trash.",
                        DeleteOutcome::Expunged => "Deleted permanently.",
                    });
                    spawn_load_folders(session.accounts.current().clone(), session.tx.clone());
                }
                Err(e) => {
                    tracing::error!("delete failed: {e}");
                    app.set_status(format!("Delete failed: {e}"));
                }
            }
        }
        BgMsg::FlagChanged {
            account_id,
            folder_id,
            uid,
            flag,
            on,
            result,
        } => {
            if let Err(e) = result {
                tracing::error!("setting {flag:?}={on} on uid {uid} failed: {e}");
                if session.accounts.is_current_folder(account_id, folder_id) {
                    app.set_envelope_flag(uid, flag, !on);
                    app.set_status(format!("Couldn't update the message on the server: {e}"));
                }
            }
        }
        BgMsg::FetchedOlder {
            account_id,
            folder_id,
            result,
        } => {
            if !session.accounts.is_current_folder(account_id, folder_id) {
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
        BgMsg::AttachmentSaved { path, result } => match result {
            Ok(()) => app.set_status(format!("Saved to {}", path.display())),
            Err(e) => {
                tracing::error!("saving attachment to {} failed: {e}", path.display());
                app.set_status(format!("Couldn't save {}: {e}", path.display()));
            }
        },
        BgMsg::SpellCheckerReady { handle, language } => {
            if let Some(compose) = &mut app.compose {
                match &handle {
                    // Anything typed before the checker came up hasn't been
                    // checked yet.
                    Some(handle) if !compose.is_blank() => handle.request_full(compose.id, &compose.body.lines),
                    Some(_) => {}
                    None => compose.spellcheck_note = Some(format!("spellcheck off: install hunspell-{language}")),
                }
            }
            session.spellcheck = handle;
        }
        BgMsg::RemoteSearchDone {
            account_id,
            folder_id,
            query,
            result,
        } => {
            let still_relevant =
                session.accounts.is_current_folder(account_id, folder_id) && app.search_query() == Some(query.as_str());
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
        BgMsg::DraftSaved { key, announce, result } => {
            let compose = app.compose.as_mut().filter(|c| c.draft_key == key);
            match (compose, result) {
                (Some(compose), Ok(())) if announce => compose.notice = Some("Draft saved".to_string()),
                (Some(compose), Err(e)) => {
                    tracing::error!("saving a draft failed: {e}");
                    compose.save_failed();
                    compose.notice = Some(format!("Couldn't save the draft: {e}"));
                }
                (None, Ok(())) if announce => app.set_status("Draft saved — P lists saved drafts."),
                (None, Err(e)) => {
                    tracing::error!("saving a draft failed: {e}");
                    app.set_status(format!("Couldn't save the draft: {e}"));
                }
                _ => {}
            }
        }
        BgMsg::Drafts { account_id, result } => {
            if !session.accounts.is_current(account_id) || app.compose.is_some() {
                return;
            }
            match result {
                Ok(drafts) if drafts.is_empty() => app.set_status("No saved drafts."),
                Ok(drafts) => {
                    app.drafts = Some(Selectable {
                        items: drafts,
                        selected: 0,
                    })
                }
                Err(e) => app.set_status(format!("Couldn't read the drafts: {e}")),
            }
        }
        BgMsg::OutboxCounts { account_id, result } => match result {
            Ok(counts) if session.accounts.is_current(account_id) => app.outbox = counts,
            Ok(_) => {}
            Err(e) => tracing::warn!("reading the outbox failed: {e}"),
        },
        BgMsg::OutboxFlushed {
            account_id,
            manual,
            result,
        } => {
            if !session.accounts.is_current(account_id) {
                return;
            }
            spawn_outbox_counts(session.accounts.current().clone(), session.tx.clone());
            match result {
                Ok(report) if report.attempted == 0 => {
                    if manual {
                        app.set_status("Nothing queued to send.");
                    }
                }
                Ok(report) if report.sent == report.attempted => {
                    app.set_status(format!("Sent {} queued message(s).", report.sent));
                }
                Ok(report) => {
                    if manual || report.sent > 0 {
                        app.set_status(format!(
                            "Sent {} of {} queued message(s); the rest still fail: {}",
                            report.sent,
                            report.attempted,
                            report.last_error.unwrap_or_default()
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!("outbox flush failed: {e}");
                    if manual {
                        app.set_status(format!("Couldn't send queued mail: {e}"));
                    }
                }
            }
        }
        BgMsg::Opened { name, result } => match result {
            Ok(()) => app.set_status(format!("Opened {name}.")),
            Err(e) => {
                tracing::error!("opening {name} failed: {e}");
                app.set_status(format!("Couldn't open {name}: {e}"));
            }
        },
    }
}

/// Reacts to a mail-core change notification from an IDLE loop.
pub fn apply_app_event(session: &Session, event: AppEvent) {
    match event {
        AppEvent::NewMail {
            account_id,
            folder_id,
            new_envelopes,
        } => {
            // The IDLE loop already wrote the fresh data straight to the
            // store; only re-read into the UI if this is the folder
            // currently being viewed.
            if session.accounts.is_current_folder(account_id, folder_id) {
                spawn_account_envelopes(session.accounts.current().clone(), session.tx.clone());
                spawn_known_senders(session.accounts.current().clone(), session.tx.clone());
            }
            if let Some(account) = session.accounts.find(account_id) {
                // The sidebar's unread counts changed with it.
                spawn_load_folders(account.clone(), session.tx.clone());
                // Notifications fire regardless of which account is being
                // viewed — arguably more useful for one that isn't.
                desktop_notify::notify_new_mail(&account.account.config.email, &new_envelopes);
            }
        }
        AppEvent::OutboxChanged { account_id } => {
            if session.accounts.is_current(account_id) {
                spawn_outbox_counts(session.accounts.current().clone(), session.tx.clone());
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

/// Syncs the folder `ctx` is showing.
pub fn spawn_sync(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let (account_id, folder_id) = (ctx.account_id, ctx.folder_id);
    let flush_tx = tx.clone();
    spawn_bg(
        tx,
        async move {
            mail_core::sync::sync_folder(&ctx.account, &ctx.store, ctx.folder_id).await?;
            // Reachable again: retry anything queued while offline, without
            // holding up the list refresh.
            spawn_flush_outbox(ctx.clone(), flush_tx, false);
            ctx.store.list_envelopes(ctx.folder_id, DISPLAY_LIMIT).await
        },
        move |result| BgMsg::SyncDone {
            account_id,
            folder_id,
            result,
        },
    );
}

/// Reads the account's folder list (and unread counts) from the server.
pub fn spawn_refresh_folders(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::refresh_folders(&ctx.account, &ctx.store, ctx.account_id).await },
        move |result| BgMsg::Folders { account_id, result },
    );
}

/// Re-reads the account's folder list from the cache.
pub fn spawn_load_folders(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { ctx.store.list_folders(ctx.account_id).await },
        move |result| BgMsg::Folders { account_id, result },
    );
}

pub fn spawn_account_envelopes(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let (account_id, folder_id) = (ctx.account_id, ctx.folder_id);
    spawn_bg(
        tx,
        async move { ctx.store.list_envelopes(ctx.folder_id, DISPLAY_LIMIT).await },
        move |result| BgMsg::AccountEnvelopes {
            account_id,
            folder_id,
            result,
        },
    );
}

pub fn spawn_fetch_body(ctx: AccountCtx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    let (account_id, folder_id) = (ctx.account_id, ctx.folder_id);
    spawn_bg(
        tx,
        async move { mail_core::sync::fetch_body(&ctx.account, &ctx.store, ctx.account_id, ctx.folder_id, uid).await },
        move |result| BgMsg::Body {
            account_id,
            folder_id,
            uid,
            result,
        },
    );
}

pub fn spawn_save_attachment(ctx: AccountCtx, uid: u32, index: usize, path: PathBuf, tx: mpsc::Sender<BgMsg>) {
    let target = path.clone();
    spawn_bg(
        tx,
        async move {
            let bytes =
                mail_core::sync::attachment_bytes(&ctx.account, &ctx.store, ctx.account_id, ctx.folder_id, uid, index)
                    .await?;
            tokio::fs::write(&target, bytes).await?;
            Ok::<_, mail_core::Error>(())
        },
        move |result| BgMsg::AttachmentSaved { path, result },
    );
}

/// How many older messages to backfill per "scrolled to the bottom"
/// trigger — deliberately the same size as a normal sync page.
const LOAD_MORE_BATCH: u32 = 50;

pub fn spawn_fetch_older(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let (account_id, folder_id) = (ctx.account_id, ctx.folder_id);
    spawn_bg(
        tx,
        async move { mail_core::sync::fetch_older(&ctx.account, &ctx.store, ctx.folder_id, LOAD_MORE_BATCH).await },
        move |result| BgMsg::FetchedOlder {
            account_id,
            folder_id,
            result,
        },
    );
}

/// How many server-side search matches to pull back and cache, newest
/// first — bounded so a single common word can't drag down a huge chunk
/// of a large mailbox.
const REMOTE_SEARCH_LIMIT: usize = 200;

pub fn spawn_remote_search(ctx: AccountCtx, query: String, tx: mpsc::Sender<BgMsg>) {
    let (account_id, folder_id) = (ctx.account_id, ctx.folder_id);
    let query_for_msg = query.clone();
    spawn_bg(
        tx,
        async move {
            mail_core::sync::search_remote(&ctx.account, &ctx.store, ctx.folder_id, &query, REMOTE_SEARCH_LIMIT).await
        },
        move |result| BgMsg::RemoteSearchDone {
            account_id,
            folder_id,
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
    let (account_id, folder_id) = (ctx.account_id, ctx.folder_id);
    spawn_bg(
        tx,
        async move { mail_core::sync::delete_message(&ctx.account, &ctx.store, ctx.folder_id, uid).await },
        move |result| BgMsg::MessageDeleted {
            account_id,
            folder_id,
            uid,
            result,
        },
    );
}

pub fn spawn_set_flag(ctx: AccountCtx, folder_id: i64, uid: u32, flag: FlagKind, on: bool, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::sync::set_flag(&ctx.account, &ctx.store, folder_id, uid, flag, on).await },
        move |result| BgMsg::FlagChanged {
            account_id,
            folder_id,
            uid,
            flag,
            on,
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
        let bytes = match &a.source {
            AttachmentSource::File(path) => tokio::fs::read(path).await?,
            AttachmentSource::Message { folder_id, uid, index } => {
                mail_core::sync::attachment_bytes(&ctx.account, &ctx.store, ctx.account_id, *folder_id, *uid, *index)
                    .await?
            }
        };
        files.push(AttachmentFile {
            filename: a.filename.clone(),
            bytes,
        });
    }
    mail_core::send::send_message(&ctx.account, &ctx.store, ctx.account_id, draft, &files).await
}

/// Retries the account's queued mail. `manual` (the user asked) also
/// retries messages given up on after too many failures, and always
/// reports back.
pub fn spawn_flush_outbox(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>, manual: bool) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::send::flush_outbox(&ctx.account, &ctx.store, ctx.account_id, manual).await },
        move |result| BgMsg::OutboxFlushed {
            account_id,
            manual,
            result,
        },
    );
}

pub fn spawn_outbox_counts(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { mail_core::send::outbox_counts(&ctx.store, ctx.account_id).await },
        move |result| BgMsg::OutboxCounts { account_id, result },
    );
}

/// Saves a draft. The store queues the write right here, before anything
/// is spawned, so saves and deletes of one draft land in the order they
/// were made.
pub fn spawn_save_draft(ctx: &AccountCtx, saved: SavedDraft, announce: bool, tx: mpsc::Sender<BgMsg>) {
    let key = saved.key.clone();
    let write = ctx.store.save_draft(ctx.account_id, saved);
    spawn_bg(tx, write, move |result| BgMsg::DraftSaved { key, announce, result });
}

/// Deletes a saved draft (if there is one under `key`); queued at once,
/// like `spawn_save_draft`.
pub fn spawn_delete_draft(ctx: &AccountCtx, key: String) {
    let delete = ctx.store.delete_draft(key);
    tokio::spawn(async move {
        if let Err(e) = delete.await {
            tracing::warn!("deleting a draft failed: {e}");
        }
    });
}

pub fn spawn_list_drafts(ctx: AccountCtx, tx: mpsc::Sender<BgMsg>) {
    let account_id = ctx.account_id;
    spawn_bg(
        tx,
        async move { ctx.store.list_drafts(ctx.account_id).await },
        move |result| BgMsg::Drafts { account_id, result },
    );
}

/// Opens attachment `index` of message `uid` in the desktop's default app
/// for its type.
pub fn spawn_open_attachment(ctx: AccountCtx, uid: u32, index: usize, name: String, tx: mpsc::Sender<BgMsg>) {
    let file_name = format!("{}-{uid}-{name}", ctx.folder_id);
    spawn_bg(
        tx,
        async move {
            let bytes =
                mail_core::sync::attachment_bytes(&ctx.account, &ctx.store, ctx.account_id, ctx.folder_id, uid, index)
                    .await
                    .map_err(|e| e.to_string())?;
            open_with_desktop(&file_name, &bytes).await
        },
        move |result| BgMsg::Opened { name, result },
    );
}

/// Opens message `uid`'s HTML body in the default browser.
pub fn spawn_open_html(ctx: AccountCtx, uid: u32, tx: mpsc::Sender<BgMsg>) {
    let file_name = format!("{}-{uid}.html", ctx.folder_id);
    spawn_bg(
        tx,
        async move {
            let html = mail_core::sync::html_body(&ctx.account, &ctx.store, ctx.account_id, ctx.folder_id, uid)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the message has no HTML version")?;
            open_with_desktop(&file_name, html.as_bytes()).await
        },
        |result| BgMsg::Opened {
            name: "the HTML version".to_string(),
            result,
        },
    );
}

/// Writes `bytes` to a private file and hands it to `xdg-open`. The file
/// has to outlive this call (the app opening it reads it later), so it's
/// left in place; `clean_open_dir` removes old ones.
async fn open_with_desktop(file_name: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = mail_core::config::open_dir().map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    mail_core::config::restrict_dir(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(file_name);
    tokio::fs::write(&path, bytes).await.map_err(|e| e.to_string())?;
    mail_core::config::restrict_file(&path).map_err(|e| e.to_string())?;

    let mut child = tokio::process::Command::new("xdg-open")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't run xdg-open: {e}"))?;
    // xdg-open usually exits as soon as it has launched the app; a quick
    // failure means it found nothing to open the file with. Still running
    // after a moment just means it's waiting on the app.
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(Ok(status)) if !status.success() => Err(format!("xdg-open failed ({status})")),
        Ok(Err(e)) => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// Removes files handed to other apps (see `open_with_desktop`) more than
/// an hour ago, at startup.
pub fn spawn_clean_open_dir() {
    tokio::task::spawn_blocking(|| {
        let Ok(dir) = mail_core::config::open_dir() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(60 * 60));
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
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
        let language = match handle {
            Some(_) => String::new(),
            None => spellcheck::active_language().await,
        };
        let _ = tx.send(BgMsg::SpellCheckerReady { handle, language }).await;
    });
}
