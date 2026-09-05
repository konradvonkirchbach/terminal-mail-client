use chrono::{DateTime, Utc};

/// A minimal, protocol-agnostic representation of a message's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    pub seen: bool,
    pub answered: bool,
    pub flagged: bool,
    pub deleted: bool,
    pub draft: bool,
}

/// Named accessor kept around for future per-flag matching (filters, etc.)
/// without exposing the raw bitfield shape everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
}

/// Header-only summary of a message, cheap to fetch and enough to render
/// the message list without pulling the body over the wire.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub uid: u32,
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub date: Option<DateTime<Utc>>,
    pub flags: Flags,
    pub has_attachments: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) if !name.is_empty() => write!(f, "{name} <{}>", self.email),
            _ => write!(f, "{}", self.email),
        }
    }
}

/// A fully parsed message body, fetched lazily when the user opens a
/// message in the reading pane.
#[derive(Debug, Clone)]
pub struct Message {
    pub uid: u32,
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub date: Option<DateTime<Utc>>,
    pub body_text: String,
    pub attachments: Vec<MessageAttachment>,
}

/// An attachment extracted from a received message, bytes and all — read
/// alongside the body since we've already parsed the whole raw message by
/// that point, so there's no separate fetch to save a download for later.
#[derive(Debug, Clone)]
pub struct MessageAttachment {
    pub filename: String,
    pub size_bytes: u64,
    pub bytes: Vec<u8>,
}
