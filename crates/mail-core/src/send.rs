//! Orchestrates sending: builds the MIME message, sends it over SMTP,
//! best-effort files a copy in Sent, and — if SMTP itself is unreachable —
//! queues the message in the local outbox instead of losing it. A failure
//! a retry can't fix (bad credentials, a rejected recipient) is returned
//! to the caller instead of queued.

use crate::account::Account;
use crate::config;
use crate::error::{Error, Result};
use crate::folder::FolderRole;
use crate::imap::client as imap_client;
use crate::smtp::{self, AttachmentFile, Draft};
use crate::store::{OutboxCounts, Store};

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Queued,
}

pub async fn send_message(
    account: &Account,
    store: &Store,
    account_id: i64,
    draft: &Draft,
    attachments: &[AttachmentFile],
) -> Result<SendOutcome> {
    let built = smtp::build(account, draft, attachments)?;
    let raw = built.raw.clone();
    let recipients = built.recipients.clone();

    match smtp::send(account, built).await {
        Ok(()) => {
            file_sent_copy(account, store, account_id, &raw).await;
            Ok(SendOutcome::Sent)
        }
        Err(e) if should_queue(&e) => {
            tracing::warn!("SMTP send failed, queuing to outbox: {e}");
            queue_outbox(store, account_id, &raw, &recipients).await?;
            Ok(SendOutcome::Queued)
        }
        Err(e) => Err(e),
    }
}

/// Best-effort: files a copy of a sent message in the account's Sent
/// folder (as discovered by the last folder refresh, if any).
async fn file_sent_copy(account: &Account, store: &Store, account_id: i64, raw: &[u8]) {
    let sent = match store.folder_by_role(account_id, FolderRole::Sent).await {
        Ok(folder) => folder.map(|f| f.name),
        Err(e) => {
            tracing::warn!("couldn't look up the Sent folder: {e}");
            None
        }
    };
    if let Err(e) = imap_client::append_to_sent(account, sent.as_deref(), raw).await {
        tracing::warn!("sent, but failed to file a Sent copy: {e}");
    }
}

/// Whether a send failure is worth queuing for a later retry: anything
/// network-shaped (connection refused, timeouts, TLS hiccups, a 4xx
/// "try again later") is; a server's permanent 5xx rejection, an invalid
/// address, or a missing password isn't — those need the user, so
/// queuing would just hide them behind "Offline — queued".
fn should_queue(e: &Error) -> bool {
    match e {
        Error::Smtp { permanent, .. } => !permanent,
        Error::Io(_) => true,
        _ => false,
    }
}

async fn queue_outbox(store: &Store, account_id: i64, raw: &[u8], recipients: &[String]) -> Result<()> {
    let dir = config::outbox_dir(account_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    config::restrict_dir(&dir)?;
    let path = dir.join(format!("{}.eml", uuid_like()));
    tokio::fs::write(&path, raw).await?;
    config::restrict_file(&path)?;
    store
        .insert_outbox(account_id, path.to_string_lossy().into_owned(), recipients.to_vec())
        .await?;
    Ok(())
}

/// After this many failed attempts a queued message is left alone (still
/// on disk and in the outbox table, for inspection) instead of being
/// retried on every sync forever — until the user asks for a retry.
const MAX_OUTBOX_ATTEMPTS: i64 = 10;

/// Serializes outbox flushes. Flushes are triggered from several places
/// (every successful sync, every IDLE wake, the user), and two running at
/// once could both read the same queued item and send it twice.
static FLUSH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What one outbox flush did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// How many queued messages it tried to send.
    pub attempted: usize,
    pub sent: usize,
    /// The last failure, if any attempt failed.
    pub last_error: Option<String>,
}

/// Attempts to send every message currently sitting in the outbox. Ones
/// that already failed `MAX_OUTBOX_ATTEMPTS` times are skipped unless
/// `retry_given_up` (the user asked to send queued mail now). Non-fatal on
/// individual failures — those stay queued for next time.
pub async fn flush_outbox(
    account: &Account,
    store: &Store,
    account_id: i64,
    retry_given_up: bool,
) -> Result<FlushReport> {
    let _guard = FLUSH_LOCK.lock().await;
    let items = store.list_outbox(account_id).await?;
    let mut report = FlushReport::default();

    for item in items {
        if item.attempts >= MAX_OUTBOX_ATTEMPTS && !retry_given_up {
            continue;
        }
        let raw = match tokio::fs::read(&item.raw_mime_path).await {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!("outbox item {} unreadable, giving up on it: {e}", item.id);
                store.delete_outbox(item.id).await?;
                continue;
            }
        };

        report.attempted += 1;
        match smtp::send_raw(account, &raw, &item.recipients).await {
            Ok(()) => {
                file_sent_copy(account, store, account_id, &raw).await;
                store.delete_outbox(item.id).await?;
                let _ = tokio::fs::remove_file(&item.raw_mime_path).await;
                report.sent += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "outbox item {} failed to send (attempt {}): {e}",
                    item.id,
                    item.attempts + 1
                );
                store.record_outbox_failure(item.id, e.to_string()).await?;
                report.last_error = Some(e.to_string());
            }
        }
    }

    Ok(report)
}

/// How many messages are waiting in an account's outbox, and how many of
/// those have been given up on (see `MAX_OUTBOX_ATTEMPTS`).
pub async fn outbox_counts(store: &Store, account_id: i64) -> Result<OutboxCounts> {
    store.outbox_counts(account_id, MAX_OUTBOX_ATTEMPTS).await
}

/// Cheap, dependency-free unique-enough id for outbox filenames.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failures_a_retry_could_fix_are_queued() {
        let transient = Error::Smtp {
            permanent: false,
            message: "connection refused".into(),
        };
        let permanent = Error::Smtp {
            permanent: true,
            message: "535 authentication failed".into(),
        };
        assert!(should_queue(&transient));
        assert!(should_queue(&Error::Io(std::io::Error::other("network down"))));
        assert!(!should_queue(&permanent), "a 5xx must reach the user, not the outbox");
        assert!(!should_queue(&Error::InvalidAddress("x".into())));
        assert!(!should_queue(&Error::MissingSecret("me@example.com".into())));
    }
}
