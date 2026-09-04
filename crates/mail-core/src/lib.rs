//! mail-core: protocol, sync, storage, and auth logic for the email client.
//! This crate must never depend on a UI toolkit (ratatui/crossterm) — it is
//! the reuse seam for any future frontend (TUI today, GUI later).

pub mod account;
pub mod config;
pub mod error;
pub mod imap;
pub mod types;

pub use account::Account;
pub use error::{Error, Result};
pub use types::{Envelope, Flag, Message};
