use std::sync::Arc;

use async_imap::types::{Fetch, Flag as ImapFlag};
use async_imap::Session;
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, rustls, TlsConnector};

use crate::account::Account;
use crate::error::{Error, Result};
use mail_parser::MimeHeaders;

use crate::types::{Address, Envelope, Flags, Message, MessageAttachment};

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

/// Runs a `UID SEARCH` and returns the matching UIDs — used both to find
/// older mail to backfill (`criteria` = a UID range) and for a
/// server-side text search (`criteria` = an `OR SUBJECT ... FROM ...`).
/// Just the UID numbers come back over the wire, so this is cheap even
/// against a huge mailbox; the caller decides how many of the matches are
/// actually worth fetching.
pub(crate) async fn uid_search(session: &mut ImapSession, criteria: &str) -> Result<Vec<u32>> {
    let uids = session
        .uid_search(criteria)
        .await
        .map_err(|e| Error::Imap(format!("UID SEARCH {criteria}: {e}")))?;
    Ok(uids.into_iter().collect())
}

/// Quotes and escapes a string for use inside an IMAP quoted-string
/// search term (`"..."`), per RFC 3501: backslash and double-quote are
/// the only characters that need escaping inside one.
pub(crate) fn escape_search_term(term: &str) -> String {
    term.replace('\\', "\\\\").replace('"', "\\\"")
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

/// A message's raw bytes are already fully in memory by the time we parse
/// it (fetched whole over IMAP, or read whole from the on-disk cache), so
/// this doesn't bound total memory use — it bounds the *extra* copy each
/// attachment gets when we pull its decoded bytes out into our own
/// `MessageAttachment`. Generous relative to what any mainstream provider
/// lets through in the first place (Gmail/Yahoo ~25MB, Outlook ~20MB
/// total per message), so it should never trigger for real mail; it's a
/// backstop against a pathological/oversized message, not a normal limit.
const MAX_ATTACHMENT_LOAD_BYTES: u64 = 50 * 1024 * 1024;

/// Parses raw RFC822 bytes (freshly fetched or read back from the on-disk
/// cache) into our `Message` type.
pub(crate) fn message_from_raw(uid: u32, raw: &[u8]) -> Result<Message> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Parse(format!("failed to parse message uid {uid}")))?;

    let attachments = parsed
        .attachments()
        .enumerate()
        .map(|(i, part)| {
            let filename = part
                .attachment_name()
                .map(str::to_string)
                .unwrap_or_else(|| format!("attachment-{}", i + 1));
            let size_bytes = part.contents().len() as u64;
            let bytes = if size_bytes > MAX_ATTACHMENT_LOAD_BYTES {
                tracing::warn!(
                    "uid {uid}: attachment {filename:?} is {size_bytes} bytes, over the \
                     {MAX_ATTACHMENT_LOAD_BYTES}-byte load cap — skipping its content"
                );
                Vec::new()
            } else {
                part.contents().to_vec()
            };
            MessageAttachment { filename, size_bytes, bytes }
        })
        .collect();

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
        attachments,
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

/// Common `Trash` mailbox names across providers, same best-effort
/// approach as `SENT_FOLDER_CANDIDATES` above.
const TRASH_FOLDER_CANDIDATES: &[&str] = &["Trash", "Deleted Items", "Deleted Messages", "Bin", "[Gmail]/Trash"];

/// Deletes a message by UID from INBOX: tries moving it to a Trash-like
/// folder first via the `MOVE` extension (recoverable, matches what most
/// providers/clients do for "delete"), falling back to marking it
/// `\Deleted` and expunging in place if no such folder exists or the
/// server doesn't support `MOVE` at all — the same "try candidates on one
/// connection" shape as `append_to_sent`.
pub async fn delete_message(account: &Account, uid: u32) -> Result<()> {
    let mut session = open_session(account).await?;
    select_inbox(&mut session).await?;

    let uid_str = uid.to_string();
    let mut moved = false;
    for folder in TRASH_FOLDER_CANDIDATES {
        if session.uid_mv(&uid_str, *folder).await.is_ok() {
            moved = true;
            break;
        }
    }

    if !moved {
        session
            .uid_store(&uid_str, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| Error::Imap(format!("STORE +Deleted uid {uid}: {e}")))?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| Error::Imap(e.to_string()))?;

        let expunged = match session.uid_expunge(&uid_str).await {
            Ok(stream) => stream.try_collect::<Vec<_>>().await.is_ok(),
            Err(_) => false,
        };
        if !expunged {
            // Server lacks UIDPLUS (no UID EXPUNGE) — fall back to a
            // blind EXPUNGE. This purges *every* \Deleted message in the
            // folder, not just this one, but is the documented last
            // resort for servers without UIDPLUS.
            if let Ok(stream) = session.expunge().await {
                let _ = stream.try_collect::<Vec<_>>().await;
            }
        }
    }

    logout(&mut session).await;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_search_term_leaves_plain_text_untouched() {
        assert_eq!(escape_search_term("plain text"), "plain text");
    }

    #[test]
    fn escape_search_term_escapes_quotes_and_backslashes() {
        assert_eq!(escape_search_term(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_search_term(r"back\slash"), r"back\\slash");
        // Backslashes must be escaped before quotes, or a term ending in
        // `\"` would produce a dangling, syntax-breaking escape.
        assert_eq!(escape_search_term(r#"\""#), r#"\\\""#);
    }

    fn plain_message(body: &str) -> Vec<u8> {
        format!(
            "From: Alice <alice@example.com>\r\n\
             To: Bob <bob@example.com>\r\n\
             Subject: Hello\r\n\
             Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             {body}"
        )
        .into_bytes()
    }

    #[test]
    fn message_from_raw_parses_headers_and_plain_body() {
        let raw = plain_message("Hello, world!");
        let msg = message_from_raw(7, &raw).unwrap();

        assert_eq!(msg.uid, 7);
        assert_eq!(msg.subject, "Hello");
        assert_eq!(msg.from[0].email, "alice@example.com");
        assert_eq!(msg.to[0].email, "bob@example.com");
        assert!(msg.body_text.contains("Hello, world!"));
        assert!(msg.attachments.is_empty());
        assert!(msg.date.is_some());
    }

    fn multipart_message_with_attachment(disposition_and_filename: &str) -> Vec<u8> {
        format!(
            "From: a@example.com\r\n\
             To: b@example.com\r\n\
             Subject: With attachment\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
             \r\n\
             --BOUNDARY\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             See attached.\r\n\
             --BOUNDARY\r\n\
             Content-Type: application/octet-stream\r\n\
             {disposition_and_filename}\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             aGVsbG8gd29ybGQ=\r\n\
             --BOUNDARY--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn message_from_raw_extracts_a_named_attachment() {
        let raw = multipart_message_with_attachment(
            "Content-Disposition: attachment; filename=\"notes.txt\"",
        );
        let msg = message_from_raw(1, &raw).unwrap();

        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename, "notes.txt");
        assert_eq!(msg.attachments[0].bytes, b"hello world");
        assert_eq!(msg.attachments[0].size_bytes, 11);
    }

    #[test]
    fn message_from_raw_synthesizes_a_filename_when_missing() {
        let raw = multipart_message_with_attachment("Content-Disposition: attachment");
        let msg = message_from_raw(1, &raw).unwrap();

        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename, "attachment-1");
    }
}
