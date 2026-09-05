//! Ties the IMAP client and the local cache together: syncs a folder's
//! envelopes into SQLite, and serves message bodies from the on-disk
//! cache when possible instead of re-fetching over the network.

use crate::account::Account;
use crate::config;
use crate::error::Result;
use crate::imap::client;
use crate::store::Store;
use crate::types::{Envelope, Message};

/// How many recently-cached messages to re-check flags for on every sync.
/// Catches read/starred/deleted changes made from another client without
/// needing `CONDSTORE`/`QRESYNC` support from the server.
const FLAG_REFRESH_WINDOW: u32 = 200;

pub struct SyncOutcome {
    pub account_id: i64,
    pub folder_id: i64,
    /// Envelopes this call fetched for the first time — on an initial
    /// sync (or after a `UIDVALIDITY` change) that's the whole backfilled
    /// batch, so callers deciding whether to do something user-visible
    /// with "new mail" (like a desktop notification) should generally
    /// only act on this from an *incremental* sync, not a first one.
    pub new_envelopes: Vec<Envelope>,
}

/// Syncs INBOX for `account` into the local cache. Safe to call repeatedly
/// (e.g. on a manual refresh) — does an initial bulk fetch the first time
/// or after a `UIDVALIDITY` change, and a cheap incremental fetch otherwise.
pub async fn sync_inbox(account: &Account, store: &Store) -> Result<SyncOutcome> {
    let account_id = store
        .upsert_account(
            account.config.email.clone(),
            account.config.display_name.clone(),
        )
        .await?;
    let folder = store
        .get_or_create_folder(account_id, "INBOX".to_string())
        .await?;

    let mut session = client::open_session(account).await?;
    let mailbox = client::select_inbox(&mut session).await?;

    let uidvalidity_changed = folder.uidvalidity != Some(mailbox.uid_validity);
    let had_prior_sync = folder.uidvalidity.is_some();

    let new_envelopes = if uidvalidity_changed {
        if had_prior_sync {
            store.clear_folder_messages(folder.id).await?;
        }
        initial_sync(
            &mut session,
            store,
            folder.id,
            account.config.fetch_limit,
            mailbox.exists,
        )
        .await?
    } else {
        incremental_sync(&mut session, store, folder.id, mailbox.uid_next).await?
    };

    client::logout(&mut session).await;
    store
        .update_folder_uid_state(folder.id, mailbox.uid_validity, mailbox.uid_next)
        .await?;

    Ok(SyncOutcome {
        account_id,
        folder_id: folder.id,
        new_envelopes,
    })
}

async fn initial_sync(
    session: &mut client::ImapSession,
    store: &Store,
    folder_id: i64,
    fetch_limit: u32,
    exists: u32,
) -> Result<Vec<Envelope>> {
    if exists == 0 {
        return Ok(Vec::new());
    }
    let end = exists;
    let start = end.saturating_sub(fetch_limit.saturating_sub(1)).max(1);

    let envelopes = client::fetch_envelope_seq_range(session, start, end).await?;
    store.upsert_envelopes(folder_id, envelopes.clone()).await?;
    Ok(envelopes)
}

async fn incremental_sync(
    session: &mut client::ImapSession,
    store: &Store,
    folder_id: i64,
    server_uid_next: u32,
) -> Result<Vec<Envelope>> {
    let cached_max_uid = store.max_uid(folder_id).await?;

    let new_envelopes = match cached_max_uid {
        Some(cached_max) if server_uid_next > cached_max + 1 => {
            let uid_set = format!("{}:{}", cached_max + 1, server_uid_next.saturating_sub(1));
            let envelopes = client::fetch_envelopes_by_uid(session, &uid_set).await?;
            store.upsert_envelopes(folder_id, envelopes.clone()).await?;
            envelopes
        }
        Some(_) => Vec::new(),
        // Folder row already matched the server's UIDVALIDITY but we have
        // no cached messages (e.g. a prior sync was interrupted) — treat
        // it like a first sync rather than diffing against nothing.
        None => {
            let mailbox = client::select_inbox(session).await?;
            initial_sync(session, store, folder_id, u32::MAX, mailbox.exists).await?
        }
    };

    let recent = store.recent_uids(folder_id, FLAG_REFRESH_WINDOW).await?;
    if !recent.is_empty() {
        let uid_set = uid_list(&recent);
        let flags = client::fetch_flags_by_uid(session, &uid_set).await?;
        for (uid, flags) in flags {
            store.update_message_flags(folder_id, uid, flags).await?;
        }
    }

    Ok(new_envelopes)
}

fn uid_list(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Returns a message body, reading it from the on-disk cache if present
/// and fetching + caching it from IMAP otherwise.
pub async fn fetch_body(
    account: &Account,
    store: &Store,
    account_id: i64,
    folder_id: i64,
    uid: u32,
) -> Result<Message> {
    if let Some(path) = store.get_message_raw_path(folder_id, uid).await? {
        if let Ok(raw) = tokio::fs::read(&path).await {
            if let Ok(message) = client::message_from_raw(uid, &raw) {
                return Ok(message);
            }
        }
        // Cache entry pointed at a file that's missing or unparsable —
        // fall through and refetch it.
    }

    let raw = client::fetch_message_raw(account, uid).await?;
    let message = client::message_from_raw(uid, &raw)?;

    if let Err(e) = cache_raw_message(store, account_id, folder_id, uid, &raw).await {
        tracing::warn!("failed to cache message uid {uid} on disk: {e}");
    }

    Ok(message)
}

/// Deletes a message on the server (moved to Trash if a candidate folder
/// works, otherwise expunged in place — see `client::delete_message`) and
/// removes it from the local cache, including its on-disk `.eml` if one
/// was cached. The server-side delete happens first: if that fails, we'd
/// rather leave the cache alone (so the message stays visible and the
/// user can retry) than have the UI drop a message that's still there.
pub async fn delete_message(account: &Account, store: &Store, folder_id: i64, uid: u32) -> Result<()> {
    client::delete_message(account, uid).await?;

    if let Some(raw_path) = store.delete_message(folder_id, uid).await? {
        let _ = tokio::fs::remove_file(raw_path).await;
    }
    Ok(())
}

async fn cache_raw_message(
    store: &Store,
    account_id: i64,
    folder_id: i64,
    uid: u32,
    raw: &[u8],
) -> Result<()> {
    let dir = config::messages_dir(account_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{uid}.eml"));
    tokio::fs::write(&path, raw).await?;
    store
        .set_message_raw_path(folder_id, uid, path.to_string_lossy().into_owned())
        .await?;
    Ok(())
}
