//! What key handling and background-result handling share for one run of
//! the app: every configured account, which one is shown, the channel
//! back to the event loop, and optional session-scoped capabilities.

use mail_core::{Account, Store};
use tokio::sync::mpsc;

use crate::bg::BgMsg;
use crate::spellcheck::SpellCheckHandle;

/// Ceiling on how many cached envelopes we'll ever read back from the
/// store for display in one go. Deliberately not the same as
/// `fetch_limit` (which sizes one "page" fetched from the server, for
/// both the initial sync and each "load more"): `fetch_limit` bounds a
/// single network round-trip, while this just needs to be generous
/// enough to show everything pagination has accumulated so far without
/// an unbounded query against a mailbox synced over a very long time.
pub const DISPLAY_LIMIT: u32 = 5000;

/// Everything a background task needs to talk to IMAP and the local
/// cache for one account. Cheap to clone — `Account`/`Store` are just
/// handles.
#[derive(Clone)]
pub struct AccountCtx {
    pub account: Account,
    pub store: Store,
    pub account_id: i64,
    /// The folder being shown while this is the current account.
    pub folder_id: i64,
    pub inbox_id: i64,
}

/// Every configured account plus which one is currently shown. Background
/// tasks get a cloned `AccountCtx` for one specific account, never this
/// whole collection.
pub struct Accounts {
    pub list: Vec<AccountCtx>,
    pub current: usize,
}

impl Accounts {
    pub fn current(&self) -> &AccountCtx {
        &self.list[self.current]
    }

    pub fn find(&self, account_id: i64) -> Option<&AccountCtx> {
        self.list.iter().find(|a| a.account_id == account_id)
    }

    pub fn is_current(&self, account_id: i64) -> bool {
        self.current().account_id == account_id
    }

    /// Whether `folder_id` of `account_id` is what's on screen — results
    /// for any other folder only went to the cache.
    pub fn is_current_folder(&self, account_id: i64, folder_id: i64) -> bool {
        let current = self.current();
        current.account_id == account_id && current.folder_id == folder_id
    }

    pub fn index_of(&self, account_id: i64) -> Option<usize> {
        self.list.iter().position(|a| a.account_id == account_id)
    }
}

/// Everything key-handling needs beyond the specific key itself and the
/// mutable `App` UI state. Grouped here instead of as separate parameters
/// so the next such capability doesn't grow every key-handling
/// function's signature again.
pub struct Session {
    pub accounts: Accounts,
    pub tx: mpsc::Sender<BgMsg>,
    pub spellcheck: Option<SpellCheckHandle>,
}
