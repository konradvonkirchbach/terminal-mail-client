//! Keeps a persistent IMAP `IDLE` connection open per account so new mail
//! shows up without the user (or a poll timer) asking for it.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::account::Account;
use crate::error::{Error, Result};
use crate::events::AppEvent;
use crate::imap::client;
use crate::store::Store;
use crate::sync;

/// RFC 2177 recommends re-issuing IDLE before ~29 minutes to avoid
/// server-side inactivity timeouts; this doubles as a periodic
/// safety-net sync even if no server notification ever arrives.
const IDLE_RENEW: Duration = Duration::from_secs(29 * 60);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(15);

/// Runs forever: opens one connection to `account`'s INBOX and idles on
/// it, running an incremental sync (via the same logic manual/startup
/// syncs use) and emitting `AppEvent::NewMail` each time the server
/// reports a change or the renewal point is reached. On any error
/// (dropped connection, network blip) it backs off and reconnects rather
/// than ending push updates for the rest of the session.
pub async fn run(account: Account, store: Store, events: UnboundedSender<AppEvent>) {
    loop {
        if let Err(e) = run_once(&account, &store, &events).await {
            tracing::warn!("IDLE connection for {} dropped: {e}", account.config.email);
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

async fn run_once(account: &Account, store: &Store, events: &UnboundedSender<AppEvent>) -> Result<()> {
    let mut session = client::open_session(account).await?;
    client::select_inbox(&mut session).await?;

    loop {
        let mut handle = session.idle();
        handle.init().await.map_err(|e| Error::Imap(e.to_string()))?;
        let (wait, _stop_source) = handle.wait_with_timeout(IDLE_RENEW);
        wait.await.map_err(|e| Error::Imap(e.to_string()))?;
        session = handle.done().await.map_err(|e| Error::Imap(e.to_string()))?;

        let outcome = sync::sync_inbox(account, store).await?;
        let _ = events.send(AppEvent::NewMail {
            account_id: outcome.account_id,
            folder_id: outcome.folder_id,
        });
    }
}
