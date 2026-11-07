//! Building replies and forwards from a received message: who they go to,
//! their subject, the quoted or forwarded text, and the headers that keep
//! a reply in its thread.

use std::collections::HashSet;

use mail_core::{Address, Message};

/// What compose is being opened to do with an existing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Respond {
    Reply,
    ReplyAll,
    Forward,
}

impl Respond {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::ReplyAll => "reply to all",
            Self::Forward => "forward",
        }
    }
}

/// Who a reply goes to: the sender's `Reply-To` if set, otherwise the
/// sender — or, for one's own sent message, its original recipients. A
/// reply-all also copies everyone else the message went to. The account's
/// own address and duplicates are left out.
pub fn reply_recipients(message: &Message, own_email: &str, all: bool) -> (Vec<Address>, Vec<Address>) {
    let mut primary = if message.reply_to.is_empty() {
        message.from.clone()
    } else {
        message.reply_to.clone()
    };
    if primary.iter().all(|a| a.email.eq_ignore_ascii_case(own_email)) && !message.to.is_empty() {
        primary = message.to.clone();
    }

    let mut seen = HashSet::from([own_email.to_lowercase()]);
    let mut unique = |list: Vec<Address>| -> Vec<Address> {
        list.into_iter()
            .filter(|a| seen.insert(a.email.to_lowercase()))
            .collect()
    };
    let to = unique(primary);
    let cc = if all {
        unique(message.to.iter().chain(&message.cc).cloned().collect())
    } else {
        Vec::new()
    };
    (to, cc)
}

/// Recipients as one To/Cc field's text.
pub fn recipient_field(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(Address::to_recipient)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn reply_subject(subject: &str) -> String {
    if subject.trim_start().to_lowercase().starts_with("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

pub fn forward_subject(subject: &str) -> String {
    let lower = subject.trim_start().to_lowercase();
    if lower.starts_with("fwd:") || lower.starts_with("fw:") {
        subject.to_string()
    } else {
        format!("Fwd: {subject}")
    }
}

/// The thread a reply joins: the original's own thread plus the original
/// itself, which becomes the reply's `References`.
pub fn thread_references(message: &Message) -> Vec<String> {
    let mut references = message.references.clone();
    if let Some(id) = &message.message_id {
        if !references.contains(id) {
            references.push(id.clone());
        }
    }
    references
}

/// A reply's starting body: two empty lines to write in, then an
/// attribution line and the original text quoted with `>`.
pub fn quoted_body(message: &Message) -> Vec<String> {
    let who = sender_name(message);
    let attribution = match message.date {
        Some(date) => format!(
            "On {}, {who} wrote:",
            date.with_timezone(&chrono::Local).format("%a, %b %-d, %Y at %H:%M")
        ),
        None => format!("{who} wrote:"),
    };
    let mut lines = vec![String::new(), String::new(), attribution];
    lines.extend(original_lines(message).map(|line| {
        if line.is_empty() {
            ">".to_string()
        } else if line.starts_with('>') {
            format!(">{line}")
        } else {
            format!("> {line}")
        }
    }));
    lines
}

/// A forward's starting body: two empty lines for a note, then the
/// original's headers and text.
pub fn forwarded_body(message: &Message) -> Vec<String> {
    let list = |addresses: &[Address]| addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
    let mut lines = vec![
        String::new(),
        String::new(),
        "---------- Forwarded message ----------".to_string(),
        format!("From: {}", list(&message.from)),
    ];
    if let Some(date) = message.date {
        lines.push(format!(
            "Date: {}",
            date.with_timezone(&chrono::Local).format("%a, %b %-d, %Y at %H:%M")
        ));
    }
    lines.push(format!("Subject: {}", message.subject));
    lines.push(format!("To: {}", list(&message.to)));
    if !message.cc.is_empty() {
        lines.push(format!("Cc: {}", list(&message.cc)));
    }
    lines.push(String::new());
    lines.extend(original_lines(message).map(str::to_string));
    lines
}

/// The original's text by line, without trailing blank lines.
fn original_lines(message: &Message) -> impl Iterator<Item = &str> {
    let lines: Vec<&str> = message.body_text.lines().collect();
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    lines.into_iter().take(end)
}

fn sender_name(message: &Message) -> String {
    message
        .from
        .first()
        .map(|a| match a.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => a.email.clone(),
        })
        .unwrap_or_else(|| "Someone".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: Option<&str>, email: &str) -> Address {
        Address {
            name: name.map(str::to_string),
            email: email.to_string(),
        }
    }

    fn message() -> Message {
        Message {
            uid: 7,
            subject: "Plans".to_string(),
            from: vec![addr(Some("Alice"), "alice@example.com")],
            to: vec![addr(None, "me@example.com"), addr(Some("Bob"), "bob@example.com")],
            cc: vec![
                addr(Some("Doe, Carol"), "carol@example.com"),
                addr(None, "ALICE@example.com"),
            ],
            message_id: Some("m2@example.com".to_string()),
            references: vec!["m1@example.com".to_string()],
            body_text: "Hi,\n\n> earlier\nsee you\n\n\n".to_string(),
            ..Message::default()
        }
    }

    fn emails(list: &[Address]) -> Vec<&str> {
        list.iter().map(|a| a.email.as_str()).collect()
    }

    #[test]
    fn a_reply_goes_to_the_sender_only() {
        let (to, cc) = reply_recipients(&message(), "me@example.com", false);
        assert_eq!(emails(&to), vec!["alice@example.com"]);
        assert!(cc.is_empty());
    }

    #[test]
    fn reply_to_wins_over_from() {
        let mut m = message();
        m.reply_to = vec![addr(Some("List"), "list@example.com")];
        let (to, _) = reply_recipients(&m, "me@example.com", false);
        assert_eq!(emails(&to), vec!["list@example.com"]);
    }

    #[test]
    fn reply_all_copies_everyone_else_once_and_never_oneself() {
        let (to, cc) = reply_recipients(&message(), "ME@example.com", true);
        assert_eq!(emails(&to), vec!["alice@example.com"]);
        assert_eq!(
            emails(&cc),
            vec!["bob@example.com", "carol@example.com"],
            "me and a second Alice are left out"
        );
        assert_eq!(
            recipient_field(&cc),
            "Bob <bob@example.com>, \"Doe, Carol\" <carol@example.com>"
        );
    }

    #[test]
    fn replying_to_ones_own_message_answers_its_recipients() {
        let mut m = message();
        m.from = vec![addr(None, "me@example.com")];
        let (to, _) = reply_recipients(&m, "me@example.com", false);
        assert_eq!(emails(&to), vec!["bob@example.com"]);
    }

    #[test]
    fn subjects_get_one_prefix() {
        assert_eq!(reply_subject("Plans"), "Re: Plans");
        assert_eq!(reply_subject("RE: Plans"), "RE: Plans");
        assert_eq!(forward_subject("Plans"), "Fwd: Plans");
        assert_eq!(forward_subject("Fw: Plans"), "Fw: Plans");
    }

    #[test]
    fn a_reply_extends_the_thread() {
        assert_eq!(thread_references(&message()), vec!["m1@example.com", "m2@example.com"]);
        let mut no_id = message();
        no_id.message_id = None;
        assert_eq!(thread_references(&no_id), vec!["m1@example.com"]);
    }

    #[test]
    fn the_original_is_quoted_below_room_to_write() {
        let lines = quoted_body(&message());
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Alice wrote:");
        assert_eq!(&lines[3..], &["> Hi,", ">", ">> earlier", "> see you"]);
    }

    #[test]
    fn a_forward_carries_the_original_headers_and_text() {
        let lines = forwarded_body(&message());
        assert!(lines.contains(&"From: Alice <alice@example.com>".to_string()));
        assert!(lines.contains(&"Subject: Plans".to_string()));
        assert!(lines.iter().any(|l| l.starts_with("Cc: ")));
        assert_eq!(lines.last().map(String::as_str), Some("see you"));
    }
}
