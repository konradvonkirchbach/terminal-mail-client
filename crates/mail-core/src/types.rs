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
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) if !name.trim().is_empty() => write!(f, "{name} <{}>", self.email),
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

/// An attachment of a received message: what to show for it. The content
/// itself is extracted only when it's saved — see
/// `sync::attachment_bytes`, which takes its index in `Message::attachments`.
#[derive(Debug, Clone)]
pub struct MessageAttachment {
    /// A plain file name, safe to use as a default when saving (no path
    /// components — see `imap::client::safe_filename`).
    pub filename: String,
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_display_shows_the_name_and_email_when_a_real_name_is_present() {
        let a = Address {
            name: Some("Alice".to_string()),
            email: "alice@example.com".to_string(),
        };
        assert_eq!(a.to_string(), "Alice <alice@example.com>");
    }

    #[test]
    fn address_display_falls_back_to_the_bare_email_with_no_name() {
        let a = Address {
            name: None,
            email: "alice@example.com".to_string(),
        };
        assert_eq!(a.to_string(), "alice@example.com");
    }

    #[test]
    fn address_display_treats_a_whitespace_only_name_as_no_name() {
        // A malformed `From:` header can leave `name` as `Some(" ")`
        // rather than `None` — must not render as " <email>".
        let a = Address {
            name: Some("   ".to_string()),
            email: "alice@example.com".to_string(),
        };
        assert_eq!(a.to_string(), "alice@example.com");
    }
}
