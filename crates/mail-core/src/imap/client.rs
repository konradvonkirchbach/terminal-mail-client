use std::sync::Arc;

use async_imap::types::{Fetch, Flag as ImapFlag};
use async_imap::Session;
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, rustls, TlsConnector};

use crate::account::Account;
use crate::error::{Error, Result};
use crate::types::{Address, Envelope, Flags, Message};

pub(crate) type ImapSession = Session<TlsStream<TcpStream>>;

/// The subset of `SELECT`'s response the sync engine needs to decide
/// between an initial and an incremental sync.
pub(crate) struct MailboxInfo {
    pub exists: u32,
    pub uid_validity: u32,
    pub uid_next: u32,
}

fn tls_connector() -> Result<TlsConnector> {
    crate::ensure_crypto_provider();

    let mut root_store = rustls::RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    for err in &certs.errors {
        tracing::warn!("failed to load a native cert: {err}");
    }
    let (_added, ignored) = root_store.add_parsable_certificates(certs.certs);
    if ignored > 0 {
        tracing::warn!("ignored {ignored} unparsable native root certificates");
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}

/// Opens a fresh authenticated session. Callers drive `SELECT`/`FETCH`
/// themselves so a whole sync pass (select + several fetches) can share
/// one connection instead of reconnecting per command.
pub(crate) async fn open_session(account: &Account) -> Result<ImapSession> {
    let password = account.password()?;
    let cfg = &account.config;

    let tcp = TcpStream::connect((cfg.imap_host.as_str(), cfg.imap_port))
        .await
        .map_err(|e| Error::ImapConnect(format!("{}:{}: {e}", cfg.imap_host, cfg.imap_port)))?;

    let connector = tls_connector()?;
    let server_name = rustls::pki_types::ServerName::try_from(cfg.imap_host.clone())
        .map_err(|e| Error::Tls(format!("invalid hostname {:?}: {e}", cfg.imap_host)))?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Tls(e.to_string()))?;

    let client = async_imap::Client::new(tls_stream);
    let session = client
        .login(&cfg.email, &password)
        .await
        .map_err(|(e, _client)| Error::ImapConnect(format!("login failed: {e}")))?;

    Ok(session)
}

pub(crate) async fn select_inbox(session: &mut ImapSession) -> Result<MailboxInfo> {
    let mailbox = session
        .select("INBOX")
        .await
        .map_err(|e| Error::Imap(format!("SELECT INBOX: {e}")))?;

    Ok(MailboxInfo {
        exists: mailbox.exists,
        uid_validity: mailbox.uid_validity.unwrap_or(0),
        uid_next: mailbox.uid_next.unwrap_or(mailbox.exists + 1),
    })
}

pub(crate) async fn logout(session: &mut ImapSession) {
    let _ = session.logout().await;
}

/// Fetches envelopes for a sequence-number range (`start:end`), used for
/// the initial bulk sync of a folder.
pub(crate) async fn fetch_envelope_seq_range(
    session: &mut ImapSession,
    start: u32,
    end: u32,
) -> Result<Vec<Envelope>> {
    let fetches: Vec<_> = session
        .fetch(format!("{start}:{end}"), "(UID FLAGS BODY.PEEK[HEADER])")
        .await
        .map_err(|e| Error::Imap(format!("FETCH {start}:{end}: {e}")))?
        .try_collect()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;

    Ok(fetches.iter().filter_map(envelope_from_fetch).collect())
}

/// Fetches envelopes for a specific UID set (e.g. `"42,57:60"`), used to
/// pull down mail that arrived since the last sync.
pub(crate) async fn fetch_envelopes_by_uid(
    session: &mut ImapSession,
    uid_set: &str,
) -> Result<Vec<Envelope>> {
    let fetches: Vec<_> = session
        .uid_fetch(uid_set, "(UID FLAGS BODY.PEEK[HEADER])")
        .await
        .map_err(|e| Error::Imap(format!("UID FETCH {uid_set}: {e}")))?
        .try_collect()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;

    Ok(fetches.iter().filter_map(envelope_from_fetch).collect())
}

/// Fetches just flags for a UID set — the cheap half of the incremental
/// sync's "did anything change" sweep over recently-cached messages.
pub(crate) async fn fetch_flags_by_uid(
    session: &mut ImapSession,
    uid_set: &str,
) -> Result<Vec<(u32, Flags)>> {
    let fetches: Vec<_> = session
        .uid_fetch(uid_set, "(UID FLAGS)")
        .await
        .map_err(|e| Error::Imap(format!("UID FETCH {uid_set}: {e}")))?
        .try_collect()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;

    Ok(fetches
        .iter()
        .filter_map(|f| f.uid.map(|uid| (uid, to_flags(f.flags()))))
        .collect())
}

fn envelope_from_fetch(f: &Fetch) -> Option<Envelope> {
    let uid = f.uid?;
    let flags = to_flags(f.flags());
    let header_bytes = f.header().unwrap_or_default();
    let parsed = mail_parser::MessageParser::default().parse(header_bytes);

    let (subject, from, to, date) = match &parsed {
        Some(msg) => (
            msg.subject().unwrap_or_default().to_string(),
            convert_addresses(msg.from()),
            convert_addresses(msg.to()),
            msg.date().map(mail_date_to_chrono),
        ),
        None => (String::new(), Vec::new(), Vec::new(), None),
    };

    Some(Envelope {
        uid,
        subject,
        from,
        to,
        date,
        flags,
        has_attachments: false,
    })
}

/// Fetches the raw RFC822 bytes of a single message by UID. Opens its own
/// connection — called on demand when the user opens a message that isn't
/// already cached on disk.
pub(crate) async fn fetch_message_raw(account: &Account, uid: u32) -> Result<Vec<u8>> {
    let mut session = open_session(account).await?;
    select_inbox(&mut session).await?;

    let fetches: Vec<_> = session
        .uid_fetch(uid.to_string(), "(UID BODY[])")
        .await
        .map_err(|e| Error::Imap(format!("UID FETCH {uid}: {e}")))?
        .try_collect()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;

    logout(&mut session).await;

    fetches
        .first()
        .and_then(|f| f.body())
        .map(|b| b.to_vec())
        .ok_or_else(|| Error::Imap(format!("no such message: uid {uid}")))
}

/// Parses raw RFC822 bytes (freshly fetched or read back from the on-disk
/// cache) into our `Message` type.
pub(crate) fn message_from_raw(uid: u32, raw: &[u8]) -> Result<Message> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Parse(format!("failed to parse message uid {uid}")))?;

    Ok(Message {
        uid,
        subject: parsed.subject().unwrap_or_default().to_string(),
        from: convert_addresses(parsed.from()),
        to: convert_addresses(parsed.to()),
        date: parsed.date().map(mail_date_to_chrono),
        body_text: parsed
            .body_text(0)
            .or_else(|| parsed.body_html(0))
            .map(|c| c.into_owned())
            .unwrap_or_default(),
    })
}

/// Common `Sent` mailbox names across providers. We don't yet do
/// SPECIAL-USE detection (`LIST ... RETURN (SPECIAL-USE)`), so this is a
/// best-effort fallback list rather than a guarantee — filing a sent copy
/// is not allowed to block the send itself.
const SENT_FOLDER_CANDIDATES: &[&str] = &["Sent", "Sent Mail", "Sent Items", "[Gmail]/Sent Mail"];

/// Files a copy of a just-sent message into the account's Sent folder.
/// Opens its own connection; failures are non-fatal to the caller (the
/// message was already sent via SMTP) so this returns an error only when
/// none of the candidate folder names worked.
pub async fn append_to_sent(account: &Account, raw: &[u8]) -> Result<()> {
    let mut session = open_session(account).await?;

    let mut last_err = None;
    for name in SENT_FOLDER_CANDIDATES {
        match session.append(*name, Some(r"(\Seen)"), None, raw).await {
            Ok(()) => {
                logout(&mut session).await;
                return Ok(());
            }
            Err(e) => last_err = Some(e.to_string()),
        }
    }

    logout(&mut session).await;
    Err(Error::Imap(format!(
        "could not file a Sent copy in any of {SENT_FOLDER_CANDIDATES:?}: {}",
        last_err.unwrap_or_default()
    )))
}

fn to_flags<'a>(flags: impl Iterator<Item = ImapFlag<'a>>) -> Flags {
    let mut out = Flags::default();
    for flag in flags {
        match flag {
            ImapFlag::Seen => out.seen = true,
            ImapFlag::Answered => out.answered = true,
            ImapFlag::Flagged => out.flagged = true,
            ImapFlag::Deleted => out.deleted = true,
            ImapFlag::Draft => out.draft = true,
            _ => {}
        }
    }
    out
}

fn convert_addresses(addr: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    match addr {
        mail_parser::Address::List(list) => list
            .iter()
            .filter_map(|a| {
                a.address.as_ref().map(|email| Address {
                    name: a.name.as_ref().map(|n| n.to_string()),
                    email: email.to_string(),
                })
            })
            .collect(),
        mail_parser::Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .filter_map(|a| {
                a.address.as_ref().map(|email| Address {
                    name: a.name.as_ref().map(|n| n.to_string()),
                    email: email.to_string(),
                })
            })
            .collect(),
    }
}

fn mail_date_to_chrono(d: &mail_parser::DateTime) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(
            d.year as i32,
            d.month as u32,
            d.day as u32,
            d.hour as u32,
            d.minute as u32,
            d.second as u32,
        )
        .single()
        .unwrap_or_else(chrono::Utc::now)
}
