//! Checking that an account's settings work before they're saved.

use crate::account::Account;
use crate::error::Result;
use crate::imap::client;
use crate::smtp;

/// The result of logging in to each of an account's servers.
pub struct ConnectionCheck {
    pub imap: Result<()>,
    pub smtp: Result<()>,
}

impl ConnectionCheck {
    pub fn ok(&self) -> bool {
        self.imap.is_ok() && self.smtp.is_ok()
    }
}

/// Logs in to the account's IMAP and SMTP servers with `password` (not the
/// keyring's — nothing has to be stored yet), then disconnects. Both are
/// tried even if the first fails, so every problem shows at once.
pub async fn connection(account: &Account, password: &str) -> ConnectionCheck {
    let (imap, smtp) = tokio::join!(client::check_login(account, password), smtp::check(account, password));
    ConnectionCheck { imap, smtp }
}
