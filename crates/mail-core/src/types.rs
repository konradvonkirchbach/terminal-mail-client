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

impl Address {
    /// The address as it goes into a To/Cc/Bcc field: `Name <email>`, with
    /// the name quoted when it contains characters that would otherwise
    /// split or break it (a comma in `Doe, Jane`, a dot, brackets, ...).
    pub fn to_recipient(&self) -> String {
        match self.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) if name.chars().any(|c| ",;:<>@()[]\".\\".contains(c)) => {
                let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\" <{}>", self.email)
            }
            Some(name) => format!("{name} <{}>", self.email),
            None => self.email.clone(),
        }
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) if !name.trim().is_empty() => write!(f, "{name} <{}>", self.email),
            _ => write!(f, "{}", self.email),
        }
    }
}

/// A message flag the user can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    Seen,
    Flagged,
    Answered,
}

impl FlagKind {
    /// The IMAP system flag's name.
    pub fn imap_name(self) -> &'static str {
        match self {
            Self::Seen => "\\Seen",
            Self::Flagged => "\\Flagged",
            Self::Answered => "\\Answered",
        }
    }
}

impl Flags {
    pub fn get(&self, flag: FlagKind) -> bool {
        match flag {
            FlagKind::Seen => self.seen,
            FlagKind::Flagged => self.flagged,
            FlagKind::Answered => self.answered,
        }
    }

    pub fn set(&mut self, flag: FlagKind, on: bool) {
        match flag {
            FlagKind::Seen => self.seen = on,
            FlagKind::Flagged => self.flagged = on,
            FlagKind::Answered => self.answered = on,
        }
    }
}

/// A fully parsed message body, fetched lazily when the user opens a
/// message in the reading pane.
#[derive(Debug, Clone, Default)]
pub struct Message {
    pub uid: u32,
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Where replies should go instead of `from`, if the sender asked.
    pub reply_to: Vec<Address>,
    pub date: Option<DateTime<Utc>>,
    /// The `Message-ID` header, without its angle brackets.
    pub message_id: Option<String>,
    /// The thread this message belongs to, oldest first, without angle
    /// brackets: its `References`, or failing that its `In-Reply-To`.
    pub references: Vec<String>,
    pub body_text: String,
    /// Whether there's a real HTML part (not just the plain text, which
    /// can always be rendered as HTML) worth opening in a browser.
    pub has_html: bool,
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
    fn to_recipient_quotes_names_that_would_break_a_recipient_list() {
        let addr = |name: Option<&str>| Address {
            name: name.map(str::to_string),
            email: "jane@example.com".to_string(),
        };
        assert_eq!(addr(Some("Jane Doe")).to_recipient(), "Jane Doe <jane@example.com>");
        assert_eq!(
            addr(Some("Doe, Jane")).to_recipient(),
            "\"Doe, Jane\" <jane@example.com>"
        );
        assert_eq!(
            addr(Some("J. \"JD\" Doe")).to_recipient(),
            "\"J. \\\"JD\\\" Doe\" <jane@example.com>"
        );
        assert_eq!(addr(None).to_recipient(), "jane@example.com");
        assert_eq!(addr(Some(" ")).to_recipient(), "jane@example.com");
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
