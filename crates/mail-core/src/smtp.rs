use lettre::message::Mailbox;
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

pub fn build(account: &Account, draft: &Draft) -> Result<BuiltMessage> {
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

    let message = builder
        .body(draft.body.clone())
        .map_err(|e| Error::Config(format!("failed to build message: {e}")))?;

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
