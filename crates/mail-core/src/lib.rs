//! mail-core: protocol, sync, storage, and auth logic for the email client.
//! This crate must never depend on a UI toolkit (ratatui/crossterm) — it is
//! the reuse seam for any future frontend (TUI today, GUI later).

pub mod account;
pub mod config;
pub mod error;
pub mod imap;
pub mod send;
pub mod smtp;
pub mod store;
pub mod sync;
pub mod types;

pub use account::Account;
pub use error::{Error, Result};
pub use smtp::Draft;
pub use store::Store;
pub use types::{Envelope, Flag, Message};

/// Both our own IMAP TLS setup and lettre's SMTP TLS setup use `rustls`,
/// but pull in different crypto backends (`aws-lc-rs` for us, `ring`
/// forced on by lettre's `rustls-tls` feature) — with both compiled in,
/// rustls can't auto-select a process-wide default and panics on first
/// use. Call this once before any network operation to pick one
/// explicitly; safe to call more than once.
pub(crate) fn ensure_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}
