//! Orchestrates sending: builds the MIME message, sends it over SMTP,
//! best-effort files a copy in Sent, and — if SMTP itself is unreachable —
//! queues the message in the local outbox instead of losing it.

use crate::account::Account;
use crate::config;
use crate::error::Result;
use crate::imap::client as imap_client;
use crate::smtp::{self, Draft};
use crate::store::Store;

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
) -> Result<SendOutcome> {
    let built = smtp::build(account, draft)?;
    let raw = built.raw.clone();
    let recipients = built.recipients.clone();

    match smtp::send(account, built).await {
        Ok(()) => {
            if let Err(e) = imap_client::append_to_sent(account, &raw).await {
                tracing::warn!("sent, but failed to file a Sent copy: {e}");
            }
            Ok(SendOutcome::Sent)
        }
        Err(e) => {
            tracing::warn!("SMTP send failed, queuing to outbox: {e}");
            queue_outbox(store, account_id, &raw, &recipients).await?;
            Ok(SendOutcome::Queued)
        }
    }
}

async fn queue_outbox(
    store: &Store,
    account_id: i64,
    raw: &[u8],
    recipients: &[String],
) -> Result<()> {
    let dir = config::outbox_dir(account_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.eml", uuid_like()));
    tokio::fs::write(&path, raw).await?;
    store
        .insert_outbox(
            account_id,
            path.to_string_lossy().into_owned(),
            recipients.to_vec(),
        )
        .await?;
    Ok(())
}

/// Attempts to send every message currently sitting in the outbox.
/// Non-fatal on individual failures — those stay queued for next time.
pub async fn flush_outbox(account: &Account, store: &Store, account_id: i64) -> Result<usize> {
    let items = store.list_outbox(account_id).await?;
    let mut sent = 0;

    for item in items {
        let raw = match tokio::fs::read(&item.raw_mime_path).await {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!("outbox item {} unreadable, giving up on it: {e}", item.id);
                store.delete_outbox(item.id).await?;
                continue;
            }
        };

        match smtp::send_raw(account, &raw, &item.recipients).await {
            Ok(()) => {
                if let Err(e) = imap_client::append_to_sent(account, &raw).await {
                    tracing::warn!("outbox item {} sent, but Sent filing failed: {e}", item.id);
                }
                store.delete_outbox(item.id).await?;
                sent += 1;
            }
            Err(e) => {
                store.record_outbox_failure(item.id, e.to_string()).await?;
            }
        }
    }

    Ok(sent)
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
