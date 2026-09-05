//! Lightweight change notifications, primarily driven by the IDLE loop.
//!
//! Deliberately minimal — an event only says "this account/folder
//! changed," never carrying the changed data itself. Consumers always
//! re-read from the local cache in response, keeping the store as the
//! single source of truth regardless of what triggered the change. This
//! is also the seam a future GUI frontend would subscribe to instead of
//! `mail-tui`'s own channel.

#[derive(Debug, Clone, Copy)]
pub enum AppEvent {
    NewMail { account_id: i64, folder_id: i64 },
}
