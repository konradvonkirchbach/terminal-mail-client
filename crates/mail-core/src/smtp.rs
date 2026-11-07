use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as LettreMessage, Tokio1Executor};

use crate::account::Account;
use crate::error::{Error, Result};

/// Everything the user typed into the compose view. Address fields are
/// comma-separated strings straight from the input widgets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    /// For a reply: the `Message-ID` being answered, without angle
    /// brackets. Becomes `In-Reply-To`, which threads the reply.
    pub in_reply_to: Option<String>,
    /// For a reply: the thread so far, oldest first, without angle
    /// brackets. Becomes `References`.
    pub references: Vec<String>,
}

/// A file to attach, already read into memory — the compose view resolves
/// the path and reads the bytes right before sending, so this stays a
/// dumb value type with no I/O of its own.
#[derive(Debug, Clone)]
pub struct AttachmentFile {
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// A built, ready-to-send message plus the raw bytes and envelope
/// recipient list. Kept separate from `formatted()`'s output because the
/// `Bcc` header is intentionally stripped from what's transmitted/cached,
/// so the recipient list can't be recovered by re-parsing the raw bytes
/// later (e.g. from the outbox on retry).
pub struct BuiltMessage {
    pub raw: Vec<u8>,
    pub recipients: Vec<String>,
    message: LettreMessage,
}

pub fn build(account: &Account, draft: &Draft, attachments: &[AttachmentFile]) -> Result<BuiltMessage> {
    let address: lettre::Address = account
        .config
        .email
        .parse()
        .map_err(|e| Error::InvalidAddress(format!("account address: {e}")))?;
    let display_name = account.config.display_name.clone().filter(|n| !n.trim().is_empty());
    let from = Mailbox::new(display_name, address);

    let mut builder = LettreMessage::builder().from(from).subject(draft.subject.clone());
    if let Some(id) = draft.in_reply_to.as_deref().filter(|id| !id.is_empty()) {
        builder = builder.in_reply_to(format!("<{id}>"));
    }
    if !draft.references.is_empty() {
        let references: Vec<String> = draft.references.iter().map(|id| format!("<{id}>")).collect();
        builder = builder.references(references.join(" "));
    }

    let mut any_recipient = false;
    for mbox in parse_mailboxes(&draft.to)? {
        any_recipient = true;
        builder = builder.to(mbox);
    }
    for mbox in parse_mailboxes(&draft.cc)? {
        any_recipient = true;
        builder = builder.cc(mbox);
    }
    for mbox in parse_mailboxes(&draft.bcc)? {
        any_recipient = true;
        builder = builder.bcc(mbox);
    }
    if !any_recipient {
        return Err(Error::Compose("message has no recipients".into()));
    }

    let message = if attachments.is_empty() {
        builder
            .body(draft.body.clone())
            .map_err(|e| Error::Compose(e.to_string()))?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(draft.body.clone()));
        for att in attachments {
            let content_type =
                ContentType::parse(mime_guess::from_path(&att.filename).first_or_octet_stream().as_ref())
                    .map_err(|e| Error::Compose(format!("invalid attachment content type: {e}")))?;
            multipart =
                multipart.singlepart(Attachment::new(att.filename.clone()).body(att.bytes.clone(), content_type));
        }
        builder
            .multipart(multipart)
            .map_err(|e| Error::Compose(e.to_string()))?
    };

    let raw = message.formatted();
    let recipients = message.envelope().to().iter().map(|a| a.to_string()).collect();

    Ok(BuiltMessage {
        raw,
        recipients,
        message,
    })
}

pub async fn send(account: &Account, built: BuiltMessage) -> Result<()> {
    let transport = transport(account).await?;
    transport.send(built.message).await?;
    Ok(())
}

/// Resends previously-built raw bytes plus a saved recipient list — used
/// to flush the outbox, where we no longer have the original `Message`.
pub async fn send_raw(account: &Account, raw: &[u8], recipients: &[String]) -> Result<()> {
    let from: lettre::Address = account
        .config
        .email
        .parse()
        .map_err(|e| Error::InvalidAddress(format!("account address: {e}")))?;
    let to = recipients
        .iter()
        .map(|r| r.parse::<lettre::Address>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::InvalidAddress(format!("saved recipient: {e}")))?;
    let envelope =
        lettre::address::Envelope::new(Some(from), to).map_err(|e| Error::Compose(format!("invalid envelope: {e}")))?;

    let transport = transport(account).await?;
    transport.send_raw(&envelope, raw).await?;
    Ok(())
}

/// Connects and logs in to the account's SMTP server with `password`,
/// then disconnects without sending anything — the setup wizard's
/// connection test.
pub async fn check(account: &Account, password: &str) -> Result<()> {
    let transport = transport_with(account, password.to_string())?;
    match transport.test_connection().await? {
        true => Ok(()),
        false => Err(Error::Smtp {
            permanent: false,
            message: "the server closed the connection".into(),
        }),
    }
}

async fn transport(account: &Account) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    transport_with(account, account.password()?)
}

fn transport_with(account: &Account, password: String) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    crate::ensure_crypto_provider();

    let cfg = &account.config;
    let creds = Credentials::new(cfg.email.clone(), password);

    // 465 is always implicit TLS; everything else (587, 25, ...) is
    // plaintext-then-STARTTLS.
    let builder = if cfg.smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
    }
    // A bad host/TLS setup is a configuration problem, not something a
    // retry fixes — mark it permanent so it isn't queued.
    .map_err(|e| Error::Smtp {
        permanent: true,
        message: format!("invalid SMTP host {:?}: {e}", cfg.smtp_host),
    })?;

    Ok(builder.port(cfg.smtp_port).credentials(creds).build())
}

fn parse_mailboxes(field: &str) -> Result<Vec<Mailbox>> {
    split_recipients(field)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<Mailbox>()
                .map_err(|e| Error::InvalidAddress(format!("{s:?}: {e}")))
        })
        .collect()
}

/// Splits a To/Cc/Bcc field at the commas between recipients — not at one
/// inside a quoted display name like `"Doe, Jane" <jane@example.com>`.
pub fn split_recipients(field: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut in_quotes, mut escaped) = (0, false, false);
    for (i, c) in field.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&field[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&field[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, AuthMethod};

    #[test]
    fn recipients_split_at_commas_outside_quoted_names() {
        assert_eq!(
            split_recipients("\"Doe, Jane\" <jane@example.com>, bob@example.com"),
            vec!["\"Doe, Jane\" <jane@example.com>", " bob@example.com"]
        );
        assert_eq!(split_recipients("a@x.com"), vec!["a@x.com"]);
        assert_eq!(
            split_recipients("\"a \\\", b\" <a@x.com>"),
            vec!["\"a \\\", b\" <a@x.com>"]
        );
    }

    #[test]
    fn a_quoted_name_with_a_comma_parses_as_one_recipient() {
        let built = build(&test_account(), &draft("\"Doe, Jane\" <jane@example.com>", "", ""), &[]).unwrap();
        assert_eq!(built.recipients, vec!["jane@example.com".to_string()]);
    }

    fn test_account() -> Account {
        Account::new(AccountConfig {
            email: "me@example.com".to_string(),
            display_name: None,
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            auth: AuthMethod::Password,
            fetch_limit: 50,
        })
    }

    fn draft(to: &str, cc: &str, bcc: &str) -> Draft {
        Draft {
            to: to.to_string(),
            cc: cc.to_string(),
            bcc: bcc.to_string(),
            subject: "Test subject".to_string(),
            body: "Test body".to_string(),
            ..Draft::default()
        }
    }

    #[test]
    fn a_reply_carries_in_reply_to_and_references() {
        let mut reply = draft("bob@example.com", "", "");
        reply.in_reply_to = Some("b@example.com".to_string());
        reply.references = vec!["a@example.com".to_string(), "b@example.com".to_string()];
        let built = build(&test_account(), &reply, &[]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw);
        assert!(raw.contains("In-Reply-To: <b@example.com>"), "{raw}");
        assert!(raw.contains("References: <a@example.com> <b@example.com>"), "{raw}");
    }

    #[test]
    fn a_new_message_has_no_threading_headers() {
        let built = build(&test_account(), &draft("bob@example.com", "", ""), &[]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw);
        assert!(!raw.contains("In-Reply-To") && !raw.contains("References"), "{raw}");
    }

    #[test]
    fn build_produces_expected_headers_and_body() {
        let built = build(&test_account(), &draft("bob@example.com", "", ""), &[]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw);

        assert!(raw.contains("From: <me@example.com>") || raw.contains("me@example.com"));
        assert!(raw.contains("To: <bob@example.com>") || raw.contains("bob@example.com"));
        assert!(raw.contains("Subject: Test subject"));
        assert!(raw.contains("Test body"));
        assert_eq!(built.recipients, vec!["bob@example.com".to_string()]);
    }

    #[test]
    fn build_puts_the_configured_display_name_in_the_from_header() {
        let mut account = test_account();
        account.config.display_name = Some("Jane Doe".to_string());
        let built = build(&account, &draft("bob@example.com", "", ""), &[]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw);
        assert!(
            raw.contains("From: \"Jane Doe\" <me@example.com>") || raw.contains("From: Jane Doe <me@example.com>"),
            "{raw}"
        );
    }

    #[test]
    fn build_with_no_recipients_at_all_is_rejected() {
        match build(&test_account(), &draft("", "", ""), &[]) {
            Err(e) => assert!(e.to_string().contains("no recipients")),
            Ok(_) => panic!("expected an error for a message with no recipients"),
        }
    }

    #[test]
    fn build_rejects_an_unparsable_address() {
        match build(&test_account(), &draft("not-an-email", "", ""), &[]) {
            Err(Error::InvalidAddress(_)) => {}
            Err(other) => panic!("expected Error::InvalidAddress, got {other:?}"),
            Ok(_) => panic!("expected an error for an unparsable address"),
        }
    }

    #[test]
    fn build_collects_cc_and_bcc_into_envelope_recipients_but_strips_the_bcc_header() {
        let built = build(
            &test_account(),
            &draft("to@example.com", "cc@example.com", "bcc@example.com"),
            &[],
        )
        .unwrap();

        let mut recipients = built.recipients.clone();
        recipients.sort();
        assert_eq!(
            recipients,
            vec![
                "bcc@example.com".to_string(),
                "cc@example.com".to_string(),
                "to@example.com".to_string()
            ]
        );

        let raw = String::from_utf8_lossy(&built.raw);
        assert!(
            raw.contains("cc@example.com"),
            "Cc header should survive into the raw message"
        );
        assert!(
            !raw.contains("bcc@example.com"),
            "Bcc must never appear in the transmitted/cached raw bytes"
        );
    }

    #[test]
    fn build_with_an_attachment_includes_its_filename() {
        let attachment = AttachmentFile {
            filename: "notes.txt".to_string(),
            bytes: b"hello world".to_vec(),
        };
        let built = build(&test_account(), &draft("bob@example.com", "", ""), &[attachment]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw);

        assert!(raw.contains("notes.txt"));
        assert!(raw.contains("Test body"));
    }
}
