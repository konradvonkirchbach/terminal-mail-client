//! Lightweight change notifications, primarily driven by the IDLE loop.
//!
//! Consumers should treat the local cache (not `new_envelopes` here) as
//! the source of truth for what to *display* — always re-read from the
//! store in response to an event rather than relying on this payload for
//! UI state, since it only reflects what one sync call happened to fetch.
//! `new_envelopes` exists for callers that want to react to specifically
//! *new* mail as it arrives (e.g. a desktop notification), which the
//! cache alone can't distinguish from "existing data reloaded." This is
//! also the seam a future GUI frontend would subscribe to instead of
//! `mail-tui`'s own channel.

use crate::types::Envelope;

#[derive(Debug, Clone)]
pub enum AppEvent {
    NewMail {
        account_id: i64,
        folder_id: i64,
        new_envelopes: Vec<Envelope>,
    },
}
