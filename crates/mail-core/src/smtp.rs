use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as LettreMessage, Tokio1Executor};

use crate::account::Account;
use crate::error::{Error, Result};

/// Everything the user typed into the compose view. Address fields are
/// comma-separated strings straight from the input widgets.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
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
    let from: Mailbox = account
        .config
        .email
        .parse()
        .map_err(|e| Error::Config(format!("invalid account address: {e}")))?;

    let mut builder = LettreMessage::builder()
        .from(from)
        .subject(draft.subject.clone());

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
        return Err(Error::Config("message has no recipients".into()));
    }

    let message = if attachments.is_empty() {
        builder
            .body(draft.body.clone())
            .map_err(|e| Error::Config(format!("failed to build message: {e}")))?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(draft.body.clone()));
        for att in attachments {
            let content_type = ContentType::parse(
                mime_guess::from_path(&att.filename)
                    .first_or_octet_stream()
                    .as_ref(),
            )
            .map_err(|e| Error::Config(format!("invalid attachment content type: {e}")))?;
            multipart = multipart.singlepart(Attachment::new(att.filename.clone()).body(att.bytes.clone(), content_type));
        }
        builder
            .multipart(multipart)
            .map_err(|e| Error::Config(format!("failed to build message: {e}")))?
    };

    let raw = message.formatted();
    let recipients = message
        .envelope()
        .to()
        .iter()
        .map(|a| a.to_string())
        .collect();

    Ok(BuiltMessage {
        raw,
        recipients,
        message,
    })
}

pub async fn send(account: &Account, built: BuiltMessage) -> Result<()> {
    let transport = transport(account).await?;
    transport
        .send(built.message)
        .await
        .map_err(|e| Error::Smtp(e.to_string()))?;
    Ok(())
}

/// Resends previously-built raw bytes plus a saved recipient list — used
/// to flush the outbox, where we no longer have the original `Message`.
pub async fn send_raw(account: &Account, raw: &[u8], recipients: &[String]) -> Result<()> {
    let from: lettre::Address = account
        .config
        .email
        .parse()
        .map_err(|e| Error::Config(format!("invalid account address: {e}")))?;
    let to = recipients
        .iter()
        .map(|r| r.parse::<lettre::Address>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Config(format!("invalid saved recipient: {e}")))?;
    let envelope = lettre::address::Envelope::new(Some(from), to)
        .map_err(|e| Error::Config(format!("invalid envelope: {e}")))?;

    let transport = transport(account).await?;
    transport
        .send_raw(&envelope, raw)
        .await
        .map_err(|e| Error::Smtp(e.to_string()))?;
    Ok(())
}

async fn transport(account: &Account) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    crate::ensure_crypto_provider();

    let password = account.password()?;
    let cfg = &account.config;
    let creds = Credentials::new(cfg.email.clone(), password);

    // 465 is always implicit TLS; everything else (587, 25, ...) is
    // plaintext-then-STARTTLS.
    let builder = if cfg.smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
    }
    .map_err(|e| Error::Smtp(e.to_string()))?;

    Ok(builder.port(cfg.smtp_port).credentials(creds).build())
}

fn parse_mailboxes(field: &str) -> Result<Vec<Mailbox>> {
    field
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<Mailbox>()
                .map_err(|e| Error::Config(format!("invalid address {s:?}: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, AuthMethod};

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
        }
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
    fn build_with_no_recipients_at_all_is_rejected() {
        match build(&test_account(), &draft("", "", ""), &[]) {
            Err(e) => assert!(e.to_string().contains("no recipients")),
            Ok(_) => panic!("expected an error for a message with no recipients"),
        }
    }

    #[test]
    fn build_rejects_an_unparsable_address() {
        match build(&test_account(), &draft("not-an-email", "", ""), &[]) {
            Err(Error::Config(_)) => {}
            Err(other) => panic!("expected Error::Config, got {other:?}"),
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
            vec!["bcc@example.com".to_string(), "cc@example.com".to_string(), "to@example.com".to_string()]
        );

        let raw = String::from_utf8_lossy(&built.raw);
        assert!(raw.contains("cc@example.com"), "Cc header should survive into the raw message");
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
