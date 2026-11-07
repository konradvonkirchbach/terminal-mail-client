//! Ties the IMAP client and the local cache together: syncs a folder's
//! envelopes into SQLite, and serves message bodies from the on-disk
//! cache when possible instead of re-fetching over the network.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::account::Account;
use crate::config;
use crate::error::Result;
use crate::folder::{self, Folder, FolderRole};
use crate::imap::client;
use crate::store::Store;
use crate::types::{Envelope, FlagKind, Flags, Message};

pub use client::DeleteOutcome;

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
    let mut session = client::open_session(account).await?;
    let outcome = sync_folder_on(&mut session, account, store, "INBOX").await;
    client::logout(&mut session).await;
    outcome
}

/// `sync_inbox` for any cached folder.
pub async fn sync_folder(account: &Account, store: &Store, folder_id: i64) -> Result<SyncOutcome> {
    let folder = store.folder(folder_id).await?;
    let mut session = client::open_session(account).await?;
    let outcome = sync_folder_on(&mut session, account, store, &folder.name).await;
    client::logout(&mut session).await;
    outcome
}

/// Syncs folder `name` over an already-open session — the IDLE loop syncs
/// on its own connection instead of opening another one per wake-up.
/// Leaves the folder selected. Also records the folder's unread count for
/// the sidebar.
///
/// Syncs of the same folder are serialized: startup, IDLE wakes, a manual
/// refresh and an account switch can all trigger one, and two running at
/// once would both "discover" the same new mail (two notifications) and
/// race on the cache.
pub(crate) async fn sync_folder_on(
    session: &mut client::ImapSession,
    account: &Account,
    store: &Store,
    name: &str,
) -> Result<SyncOutcome> {
    let lock = sync_lock(&account.config.email, name);
    let _guard = lock.lock().await;

    let account_id = store
        .upsert_account(account.config.email.clone(), account.config.display_name.clone())
        .await?;
    let folder = store.get_or_create_folder(account_id, name.to_string()).await?;

    let mailbox = client::select(session, name).await?;

    let uidvalidity_changed = folder.uidvalidity != Some(mailbox.uid_validity);
    let had_prior_sync = folder.uidvalidity.is_some();

    let new_envelopes = if uidvalidity_changed {
        if had_prior_sync {
            let stale = store.clear_folder_messages(folder.id).await?;
            remove_cached_files(stale).await;
        }
        initial_sync(session, store, folder.id, account.config.fetch_limit, mailbox.exists).await?
    } else {
        incremental_sync(session, store, folder.id, &mailbox, account.config.fetch_limit).await?
    };

    store
        .update_folder_uid_state(folder.id, mailbox.uid_validity, mailbox.uid_next)
        .await?;
    match client::count_unseen(session).await {
        Ok(unseen) => store.update_folder_counts(folder.id, unseen, mailbox.exists).await?,
        Err(e) => tracing::warn!("couldn't count unread mail in {name}: {e}"),
    }

    Ok(SyncOutcome {
        account_id,
        folder_id: folder.id,
        new_envelopes,
    })
}

/// One lock per account and folder, created on first use.
type SyncLocks = std::sync::Mutex<HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>>;

fn sync_lock(email: &str, folder: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<SyncLocks> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(locks.entry((email.to_string(), folder.to_string())).or_default())
}

/// Reads the account's folder list from the server — names, what each is
/// for, and unread/total counts — into the cache, and returns it in
/// sidebar order. Folders the server no longer has are dropped, along with
/// their cached mail.
pub async fn refresh_folders(account: &Account, store: &Store, account_id: i64) -> Result<Vec<Folder>> {
    let mut session = client::open_session(account).await?;
    let result = refresh_folders_on(&mut session, store, account_id).await;
    client::logout(&mut session).await;
    result?;
    store.list_folders(account_id).await
}

async fn refresh_folders_on(session: &mut client::ImapSession, store: &Store, account_id: i64) -> Result<()> {
    let mut remote = client::list_folders(session).await?;
    folder::assign_roles(&mut remote);
    let selectable: Vec<String> = remote.iter().filter(|f| f.selectable).map(|f| f.name.clone()).collect();

    let stale = store.replace_folders(account_id, remote).await?;
    remove_cached_files(stale).await;

    for name in selectable {
        match client::status_counts(session, &name).await {
            Ok((unseen, total)) => {
                store
                    .update_folder_counts_by_name(account_id, name, unseen, total)
                    .await?
            }
            Err(e) => tracing::warn!("couldn't read the message counts of {name}: {e}"),
        }
    }
    Ok(())
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
    if let Some(oldest) = envelopes.iter().map(|e| e.uid).min() {
        store.lower_backfill_floor(folder_id, oldest).await?;
    }
    Ok(envelopes)
}

async fn incremental_sync(
    session: &mut client::ImapSession,
    store: &Store,
    folder_id: i64,
    mailbox: &client::MailboxInfo,
    fetch_limit: u32,
) -> Result<Vec<Envelope>> {
    let cached_max_uid = store.max_uid(folder_id).await?;

    let new_envelopes = match cached_max_uid {
        Some(cached_max) if mailbox.uid_next > cached_max + 1 => {
            let uid_set = format!("{}:{}", cached_max + 1, mailbox.uid_next.saturating_sub(1));
            let envelopes = client::fetch_envelopes_by_uid(session, &uid_set).await?;
            store.upsert_envelopes(folder_id, envelopes.clone()).await?;
            envelopes
        }
        Some(_) => Vec::new(),
        // Folder row already matched the server's UIDVALIDITY but we have
        // no cached messages (an empty inbox at the last sync, or every
        // cached message deleted since) — fetch a normal first page rather
        // than diffing against nothing. Not the whole mailbox: older mail
        // is still available on demand through "load more".
        None => initial_sync(session, store, folder_id, fetch_limit, mailbox.exists).await?,
    };

    let recent = store.recent_uids(folder_id, FLAG_REFRESH_WINDOW).await?;
    if !recent.is_empty() {
        let uid_set = uid_list(&recent);
        let flags = client::fetch_flags_by_uid(session, &uid_set).await?;

        // A cached UID the server no longer returns was expunged — deleted
        // or moved away from another client — so drop it here too, or it
        // would linger in the list forever. Skipped when the server
        // returned nothing at all for a non-empty mailbox: that's more
        // likely a server quirk than every recent message vanishing.
        let gone = expunged_uids(&recent, &flags);
        if !gone.is_empty() && (!flags.is_empty() || mailbox.exists == 0) {
            tracing::info!("{} cached message(s) were removed on the server", gone.len());
            let paths = store.delete_messages(folder_id, gone).await?;
            remove_cached_files(paths).await;
        }

        store.update_message_flags_batch(folder_id, flags).await?;
    }

    Ok(new_envelopes)
}

/// The UIDs from `requested` that a `UID FETCH` didn't return — on IMAP,
/// fetching a UID that no longer exists silently yields nothing.
fn expunged_uids(requested: &[u32], returned: &[(u32, Flags)]) -> Vec<u32> {
    let present: std::collections::HashSet<u32> = returned.iter().map(|(uid, _)| *uid).collect();
    requested.iter().copied().filter(|uid| !present.contains(uid)).collect()
}

/// Best-effort removal of cached `.eml` files whose rows are gone.
async fn remove_cached_files(paths: Vec<String>) {
    for path in paths {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to remove cached message {path}: {e}");
            }
        }
    }
}

fn uid_list(uids: &[u32]) -> String {
    uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

/// Backfills up to `batch_size` messages older than whatever's currently
/// cached — the initial sync only ever pulls the most recent
/// `fetch_limit` messages, so this is what "scroll to the bottom to load
/// more" calls on demand instead of eagerly fetching a whole mailbox's
/// history up front. Returns how many were actually found and fetched;
/// `0` means there's nothing older left on the server.
pub async fn fetch_older(account: &Account, store: &Store, folder_id: i64, batch_size: u32) -> Result<usize> {
    // The floor, not MIN(uid): cached server-search hits can be much older
    // than the contiguous window and would make this skip the gap.
    let Some(floor) = store.backfill_floor(folder_id).await? else {
        return Ok(0); // nothing synced yet at all — a plain sync should run first
    };
    if floor <= 1 {
        return Ok(0); // already down to UID 1 — nothing older can exist
    }
    let folder = store.folder(folder_id).await?;

    let mut session = client::open_session(account).await?;
    client::select(&mut session, &folder.name).await?;

    let mut candidates = client::uid_search(&mut session, &format!("UID 1:{}", floor - 1)).await?;
    candidates.sort_unstable();
    candidates.reverse(); // newest-of-the-older first
    candidates.truncate(batch_size as usize);

    let count = if candidates.is_empty() {
        0
    } else {
        let uid_set = uid_list(&candidates);
        let envelopes = client::fetch_envelopes_by_uid(&mut session, &uid_set).await?;
        let count = envelopes.len();
        store.upsert_envelopes(folder_id, envelopes).await?;
        // Every candidate was searched for, fetched or not — the next batch
        // continues below the oldest of them.
        if let Some(&oldest) = candidates.last() {
            store.lower_backfill_floor(folder_id, oldest).await?;
        }
        count
    };

    client::logout(&mut session).await;
    Ok(count)
}

/// Searches the *server's* copy of the mailbox (not just what's locally
/// cached) for `query` matching subject or sender, for when a local
/// search comes up empty — the local cache is deliberately a bounded
/// recent window, so "not in the cache" doesn't mean "doesn't exist".
/// Matches are cached like any other fetch, so a subsequent local filter
/// picks them up automatically. Bounded to the `limit` most recent
/// matches, newest first, to avoid a single common word pulling down a
/// huge fraction of a large mailbox.
pub async fn search_remote(
    account: &Account,
    store: &Store,
    folder_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<Envelope>> {
    let folder = store.folder(folder_id).await?;
    let mut session = client::open_session(account).await?;
    client::select(&mut session, &folder.name).await?;

    let escaped = client::escape_search_term(query);
    let criteria = format!("OR SUBJECT \"{escaped}\" FROM \"{escaped}\"");
    let mut candidates = client::uid_search(&mut session, &criteria).await?;
    candidates.sort_unstable();
    candidates.reverse();
    candidates.truncate(limit);

    let envelopes = if candidates.is_empty() {
        Vec::new()
    } else {
        let uid_set = uid_list(&candidates);
        let envelopes = client::fetch_envelopes_by_uid(&mut session, &uid_set).await?;
        store.upsert_envelopes(folder_id, envelopes.clone()).await?;
        envelopes
    };

    client::logout(&mut session).await;
    Ok(envelopes)
}

/// Returns a message body, reading it from the on-disk cache if present
/// and fetching + caching it from IMAP otherwise. Marks it read, locally
/// and on the server.
pub async fn fetch_body(
    account: &Account,
    store: &Store,
    account_id: i64,
    folder_id: i64,
    uid: u32,
) -> Result<Message> {
    let folder = store.folder(folder_id).await?;
    if let Some(raw) = cached_raw(store, folder_id, uid).await? {
        if let Ok(message) = client::message_from_raw(uid, &raw) {
            // Served from the cache, so the server never saw this message
            // being read: mark it there too (in the background — the body
            // is ready now).
            if store.set_flag(folder_id, uid, FlagKind::Seen, true).await? {
                let account = account.clone();
                tokio::spawn(async move {
                    if let Err(e) = client::store_flag(&account, &folder.name, uid, FlagKind::Seen, true).await {
                        tracing::warn!("failed to mark uid {uid} read on the server: {e}");
                    }
                });
            }
            return Ok(message);
        }
        // The cached file is unparsable — fall through and refetch it.
    }
    // (No cache entry, or its file has gone missing: fetch it.)

    // Fetching `BODY[]` sets `\Seen` on the server by itself; just mirror it
    // locally so the list doesn't keep showing the message as unread until
    // the next flag sweep.
    let raw = client::fetch_message_raw(account, &folder.name, uid).await?;
    let message = client::message_from_raw(uid, &raw)?;
    store.set_flag(folder_id, uid, FlagKind::Seen, true).await?;

    if let Err(e) = cache_raw_message(store, account_id, folder_id, uid, &raw).await {
        tracing::warn!("failed to cache message uid {uid} on disk: {e}");
    }

    Ok(message)
}

/// The decoded content of attachment `index` (its position in
/// `Message::attachments`) of message `uid`, read from the on-disk cache —
/// where opening the message put it — or fetched and cached again if the
/// file has gone missing since.
pub async fn attachment_bytes(
    account: &Account,
    store: &Store,
    account_id: i64,
    folder_id: i64,
    uid: u32,
    index: usize,
) -> Result<Vec<u8>> {
    let raw = raw_message(account, store, account_id, folder_id, uid).await?;
    client::attachment_from_raw(uid, &raw, index)
}

/// The message's HTML body, if it has a real one (`Message::has_html`),
/// for opening in a browser. From the cache like `attachment_bytes`.
pub async fn html_body(
    account: &Account,
    store: &Store,
    account_id: i64,
    folder_id: i64,
    uid: u32,
) -> Result<Option<String>> {
    let raw = raw_message(account, store, account_id, folder_id, uid).await?;
    Ok(client::html_from_raw(&raw))
}

/// The raw message, from the on-disk cache or else fetched (and cached).
async fn raw_message(account: &Account, store: &Store, account_id: i64, folder_id: i64, uid: u32) -> Result<Vec<u8>> {
    if let Some(raw) = cached_raw(store, folder_id, uid).await? {
        return Ok(raw);
    }
    let folder = store.folder(folder_id).await?;
    let raw = client::fetch_message_raw(account, &folder.name, uid).await?;
    if let Err(e) = cache_raw_message(store, account_id, folder_id, uid, &raw).await {
        tracing::warn!("failed to cache message uid {uid} on disk: {e}");
    }
    Ok(raw)
}

/// The cached raw message, if one was cached and its file is still readable.
async fn cached_raw(store: &Store, folder_id: i64, uid: u32) -> Result<Option<Vec<u8>>> {
    let Some(path) = store.get_message_raw_path(folder_id, uid).await? else {
        return Ok(None);
    };
    Ok(tokio::fs::read(&path).await.ok())
}

/// Deletes a message on the server — moved to the account's Trash folder,
/// or expunged for good when it's already in Trash or there's no Trash at
/// all (see `client::delete_message`) — and removes it from the local
/// cache, including its on-disk `.eml` if one was cached. The server-side
/// delete happens first: if that fails, we'd rather leave the cache alone
/// (so the message stays visible and the user can retry) than have the UI
/// drop a message that's still there.
pub async fn delete_message(account: &Account, store: &Store, folder_id: i64, uid: u32) -> Result<DeleteOutcome> {
    let folder = store.folder(folder_id).await?;
    let trash = store.folder_by_role(folder.account_id, FolderRole::Trash).await?;
    let in_trash = folder.role == Some(FolderRole::Trash);
    let outcome = client::delete_message(
        account,
        &folder.name,
        uid,
        trash.as_ref().map(|t| t.name.as_str()),
        in_trash,
    )
    .await?;

    if let Some(raw_path) = store.delete_message(folder_id, uid).await? {
        let _ = tokio::fs::remove_file(raw_path).await;
    }
    Ok(outcome)
}

/// Sets or clears a flag (read, starred, answered) on a message: on the
/// server first, then in the cache, so a failure leaves both unchanged.
pub async fn set_flag(
    account: &Account,
    store: &Store,
    folder_id: i64,
    uid: u32,
    flag: FlagKind,
    on: bool,
) -> Result<()> {
    let folder = store.folder(folder_id).await?;
    client::store_flag(account, &folder.name, uid, flag, on).await?;
    store.set_flag(folder_id, uid, flag, on).await?;
    Ok(())
}

async fn cache_raw_message(store: &Store, account_id: i64, folder_id: i64, uid: u32, raw: &[u8]) -> Result<()> {
    let dir = config::messages_dir(account_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    config::restrict_dir(&dir)?;
    // UIDs are only unique within a folder. (INBOX bodies cached before
    // folders existed are still at `{uid}.eml`, found through `raw_path`.)
    let path = dir.join(format!("{folder_id}-{uid}.eml"));
    tokio::fs::write(&path, raw).await?;
    config::restrict_file(&path)?;
    store
        .set_message_raw_path(folder_id, uid, path.to_string_lossy().into_owned())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_lock_is_shared_per_folder_and_separate_across_accounts_and_folders() {
        let a1 = sync_lock("a@example.com", "INBOX");
        let a2 = sync_lock("a@example.com", "INBOX");
        let a_sent = sync_lock("a@example.com", "Sent");
        let b = sync_lock("b@example.com", "INBOX");
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &a_sent));
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    #[test]
    fn expunged_uids_are_the_requested_ones_the_server_did_not_return() {
        let returned = vec![(10, Flags::default()), (8, Flags::default())];
        assert_eq!(expunged_uids(&[10, 9, 8, 7], &returned), vec![9, 7]);
        assert!(expunged_uids(&[10, 8], &returned).is_empty());
        assert_eq!(expunged_uids(&[3], &[]), vec![3]);
    }
}
