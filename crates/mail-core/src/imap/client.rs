use std::sync::Arc;

use async_imap::types::Flag as ImapFlag;
use async_imap::Session;
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, rustls, TlsConnector};

use crate::account::Account;
use crate::error::{Error, Result};
use crate::types::{Address, Envelope, Flags, Message};

type ImapSession = Session<TlsStream<TcpStream>>;

fn tls_connector() -> Result<TlsConnector> {
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

async fn connect(host: &str, port: u16, email: &str, password: &str) -> Result<ImapSession> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| Error::ImapConnect(format!("{host}:{port}: {e}")))?;

    let connector = tls_connector()?;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| Error::Tls(format!("invalid hostname {host:?}: {e}")))?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Tls(e.to_string()))?;

    let client = async_imap::Client::new(tls_stream);
    let session = client
        .login(email, password)
        .await
        .map_err(|(e, _client)| Error::ImapConnect(format!("login failed: {e}")))?;

    Ok(session)
}

/// Fetches the most recent `fetch_limit` message headers from INBOX. Opens
/// a fresh connection per call — Phase 1 has no persistent session/cache
/// yet, that arrives with the sync engine.
pub async fn fetch_envelopes(account: &Account) -> Result<Vec<Envelope>> {
    let password = account.password()?;
    let cfg = &account.config;
    let mut session = connect(&cfg.imap_host, cfg.imap_port, &cfg.email, &password).await?;

    let mailbox = session
        .select("INBOX")
        .await
        .map_err(|e| Error::Imap(format!("SELECT INBOX: {e}")))?;

    if mailbox.exists == 0 {
        let _ = session.logout().await;
        return Ok(Vec::new());
    }

    let end = mailbox.exists;
    let start = end.saturating_sub(cfg.fetch_limit.saturating_sub(1)).max(1);

    let fetches: Vec<_> = session
        .fetch(format!("{start}:{end}"), "(UID FLAGS BODY.PEEK[HEADER])")
        .await
        .map_err(|e| Error::Imap(format!("FETCH {start}:{end}: {e}")))?
        .try_collect()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;

    let mut envelopes = Vec::with_capacity(fetches.len());
    for f in &fetches {
        let Some(uid) = f.uid else { continue };
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

        envelopes.push(Envelope {
            uid,
            subject,
            from,
            to,
            date,
            flags,
            has_attachments: false,
        });
    }

    // Newest first.
    envelopes.sort_by_key(|e| std::cmp::Reverse(e.uid));

    let _ = session.logout().await;
    Ok(envelopes)
}

/// Fetches and parses the full body of a single message by UID.
pub async fn fetch_message(account: &Account, uid: u32) -> Result<Message> {
    let password = account.password()?;
    let cfg = &account.config;
    let mut session = connect(&cfg.imap_host, cfg.imap_port, &cfg.email, &password).await?;

    session
        .select("INBOX")
        .await
        .map_err(|e| Error::Imap(format!("SELECT INBOX: {e}")))?;

    let fetches: Vec<_> = session
        .uid_fetch(uid.to_string(), "(UID BODY[])")
        .await
        .map_err(|e| Error::Imap(format!("UID FETCH {uid}: {e}")))?
        .try_collect()
        .await
        .map_err(|e| Error::Imap(e.to_string()))?;

    let _ = session.logout().await;

    let raw = fetches
        .first()
        .and_then(|f| f.body())
        .ok_or_else(|| Error::Imap(format!("no such message: uid {uid}")))?;

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
