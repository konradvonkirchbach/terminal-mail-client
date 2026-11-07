use std::sync::{Arc, OnceLock};
use std::time::Duration;

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

/// How long connecting, the TLS handshake and LOGIN may take together
/// before giving up, rather than hanging on an unresponsive server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The rustls client config, built once per process: loading and parsing
/// the system's root certificate bundle is far too slow to repeat for
/// every connection (and every sync, body fetch, delete, ... opens one).
fn tls_connector() -> TlsConnector {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
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

        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        )
    });
    TlsConnector::from(Arc::clone(config))
}

/// Makes a dead connection fail instead of hanging. After a suspend/resume
/// or a network switch the server's end is gone without a FIN or RST, and
/// an idle socket would otherwise wait forever (IDLE in particular): TCP
/// keepalive probes an idle socket and errors it out within ~2 minutes,
/// and on Linux `TCP_USER_TIMEOUT` does the same for data sent but never
/// acknowledged (a write into a dead connection, like IDLE's `DONE`), which
/// would otherwise retransmit for a quarter of an hour. Best-effort: a
/// failure here only costs that robustness.
fn harden_socket(tcp: &TcpStream) {
    let socket = socket2::SockRef::from(tcp);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(15))
        .with_retries(4);
    if let Err(e) = socket.set_tcp_keepalive(&keepalive) {
        tracing::warn!("couldn't enable TCP keepalive: {e}");
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = socket.set_tcp_user_timeout(Some(Duration::from_secs(120))) {
        tracing::warn!("couldn't set TCP_USER_TIMEOUT: {e}");
    }
}

/// Opens a fresh authenticated session. Callers drive `SELECT`/`FETCH`
/// themselves so a whole sync pass (select + several fetches) can share
/// one connection instead of reconnecting per command.
pub(crate) async fn open_session(account: &Account) -> Result<ImapSession> {
    let password = account.password()?;
    let cfg = &account.config;
    tokio::time::timeout(CONNECT_TIMEOUT, connect_and_login(account, &password))
        .await
        .map_err(|_| {
            Error::ImapConnect(format!(
                "{}:{}: timed out after {}s",
                cfg.imap_host,
                cfg.imap_port,
                CONNECT_TIMEOUT.as_secs()
            ))
        })?
}

async fn connect_and_login(account: &Account, password: &str) -> Result<ImapSession> {
    let cfg = &account.config;

    let tcp = TcpStream::connect((cfg.imap_host.as_str(), cfg.imap_port))
        .await
        .map_err(|e| Error::ImapConnect(format!("{}:{}: {e}", cfg.imap_host, cfg.imap_port)))?;
    harden_socket(&tcp);

    let server_name = rustls::pki_types::ServerName::try_from(cfg.imap_host.clone())
        .map_err(|e| Error::Tls(format!("invalid hostname {:?}: {e}", cfg.imap_host)))?;
    let tls_stream = tls_connector()
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Tls(e.to_string()))?;

    let client = async_imap::Client::new(tls_stream);
    let session = client
        .login(&cfg.email, password)
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
pub(crate) async fn fetch_envelope_seq_range(session: &mut ImapSession, start: u32, end: u32) -> Result<Vec<Envelope>> {
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
pub(crate) async fn fetch_envelopes_by_uid(session: &mut ImapSession, uid_set: &str) -> Result<Vec<Envelope>> {
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
pub(crate) async fn fetch_flags_by_uid(session: &mut ImapSession, uid_set: &str) -> Result<Vec<(u32, Flags)>> {
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
            msg.date().and_then(mail_date_to_chrono),
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

    let attachments = parsed
        .attachments()
        .enumerate()
        .map(|(i, part)| {
            let filename = part
                .attachment_name()
                .and_then(safe_filename)
                .unwrap_or_else(|| format!("attachment-{}", i + 1));
            MessageAttachment {
                filename,
                size_bytes: part.contents().len() as u64,
            }
        })
        .collect();

    Ok(Message {
        uid,
        subject: parsed.subject().unwrap_or_default().to_string(),
        from: convert_addresses(parsed.from()),
        to: convert_addresses(parsed.to()),
        date: parsed.date().and_then(mail_date_to_chrono),
        body_text: parsed
            .body_text(0)
            .or_else(|| parsed.body_html(0))
            .map(|c| c.into_owned())
            .unwrap_or_default(),
        attachments,
    })
}

/// The decoded content of the message's `index`-th attachment, in the same
/// order `message_from_raw` lists them. Extracted on demand (when the user
/// saves one) rather than copied into every `Message` up front, which used
/// to hold every attachment — inline images included — in memory just for
/// opening a message.
pub(crate) fn attachment_from_raw(uid: u32, raw: &[u8], index: usize) -> Result<Vec<u8>> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Parse(format!("failed to parse message uid {uid}")))?;
    let bytes = parsed.attachments().nth(index).map(|part| part.contents().to_vec());
    bytes.ok_or_else(|| Error::Parse(format!("message uid {uid} has no attachment #{}", index + 1)))
}

/// Reduces an attachment name taken from a message header to a plain file
/// name. The header is attacker-controlled, and frontends offer it as the
/// default name when saving: a name like `../../.bashrc` or
/// `/home/me/.ssh/authorized_keys` would otherwise write outside the
/// directory the user picked. Keeps only the last path component (either
/// separator), drops control characters, and rejects names that are
/// empty or just dots.
fn safe_filename(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = last.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        return None;
    }
    Some(cleaned.to_string())
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

/// Sets `\Seen` on a message server-side. Needed when a message is opened
/// from the on-disk cache: only fetching `BODY[]` over IMAP marks it read
/// implicitly, so a cache hit would otherwise leave it unread everywhere
/// else.
pub(crate) async fn mark_seen(account: &Account, uid: u32) -> Result<()> {
    let mut session = open_session(account).await?;
    select_inbox(&mut session).await?;
    session
        .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
        .await
        .map_err(|e| Error::Imap(format!("STORE +Seen uid {uid}: {e}")))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;
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

/// Converts a parsed `Date:` header to a UTC instant, honoring its
/// timezone offset (`to_timestamp` applies it). An invalid date (out-of-range
/// fields from a malformed header) yields `None` rather than a stand-in like
/// "now", which would make a broken old message look brand new.
fn mail_date_to_chrono(d: &mail_parser::DateTime) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    if !d.is_valid() {
        return None;
    }
    chrono::Utc.timestamp_opt(d.to_timestamp(), 0).single()
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

    #[test]
    fn message_from_raw_applies_the_date_headers_timezone_offset() {
        let raw = b"From: a@example.com\r\n\
                    Subject: tz\r\n\
                    Date: Mon, 1 Jan 2024 14:00:00 +0200\r\n\
                    \r\n\
                    body"
            .to_vec();
        let msg = message_from_raw(1, &raw).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z").unwrap();
        assert_eq!(msg.date.unwrap(), expected, "14:00 at +0200 is 12:00 UTC");
    }

    #[test]
    fn message_from_raw_applies_a_negative_timezone_offset() {
        let raw = b"From: a@example.com\r\n\
                    Date: Mon, 1 Jan 2024 07:30:00 -0500\r\n\
                    \r\n\
                    body"
            .to_vec();
        let msg = message_from_raw(1, &raw).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:30:00Z").unwrap();
        assert_eq!(msg.date.unwrap(), expected);
    }

    #[test]
    fn an_invalid_date_is_none_rather_than_now() {
        let invalid = mail_parser::DateTime {
            year: 2024,
            month: 13,
            day: 40,
            hour: 25,
            minute: 0,
            second: 0,
            tz_before_gmt: false,
            tz_hour: 0,
            tz_minute: 0,
        };
        assert_eq!(mail_date_to_chrono(&invalid), None);
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
        let raw = multipart_message_with_attachment("Content-Disposition: attachment; filename=\"notes.txt\"");
        let msg = message_from_raw(1, &raw).unwrap();

        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename, "notes.txt");
        assert_eq!(msg.attachments[0].size_bytes, 11);
        assert_eq!(attachment_from_raw(1, &raw, 0).unwrap(), b"hello world");
        assert!(attachment_from_raw(1, &raw, 1).is_err(), "there's only one");
    }

    #[test]
    fn safe_filename_strips_directories_and_control_characters() {
        assert_eq!(safe_filename("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe_filename("../../.bashrc").as_deref(), Some(".bashrc"));
        assert_eq!(
            safe_filename("/home/me/.ssh/authorized_keys").as_deref(),
            Some("authorized_keys")
        );
        assert_eq!(safe_filename("C:\\Windows\\evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(safe_filename("na\u{1b}[2Jme.txt").as_deref(), Some("na[2Jme.txt"));
        assert_eq!(safe_filename(".."), None);
        assert_eq!(safe_filename("dir/"), None);
        assert_eq!(safe_filename("   "), None);
    }

    #[test]
    fn message_from_raw_never_yields_a_path_as_an_attachment_name() {
        let raw = multipart_message_with_attachment("Content-Disposition: attachment; filename=\"../../evil.sh\"");
        let msg = message_from_raw(1, &raw).unwrap();
        assert_eq!(msg.attachments[0].filename, "evil.sh");
    }

    #[test]
    fn message_from_raw_synthesizes_a_filename_when_missing() {
        let raw = multipart_message_with_attachment("Content-Disposition: attachment");
        let msg = message_from_raw(1, &raw).unwrap();

        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename, "attachment-1");
    }
}
