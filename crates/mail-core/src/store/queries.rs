use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::folder::{Folder, FolderRole, RemoteFolder};
use crate::smtp::Draft;
use crate::types::{Address, Envelope, FlagKind, Flags};

pub struct FolderRow {
    pub id: i64,
    pub uidvalidity: Option<u32>,
}

pub fn upsert_account(conn: &Connection, email: &str, display_name: Option<&str>) -> Result<i64> {
    conn.execute(
        "INSERT INTO accounts (email, display_name) VALUES (?1, ?2)
         ON CONFLICT(email) DO UPDATE SET display_name = excluded.display_name",
        params![email, display_name],
    )?;
    let id = conn.query_row("SELECT id FROM accounts WHERE email = ?1", params![email], |row| {
        row.get(0)
    })?;
    Ok(id)
}

/// Deletes an account's row and, via `ON DELETE CASCADE`, every folder,
/// message, attachment, draft, and outbox row that referenced it. Doesn't
/// touch anything on disk (raw `.eml` files, outbox files) — those live
/// outside SQLite and are the caller's responsibility to clean up.
pub fn delete_account(conn: &Connection, account_id: i64) -> Result<()> {
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![account_id])?;
    Ok(())
}

pub fn get_or_create_folder(conn: &Connection, account_id: i64, name: &str) -> Result<FolderRow> {
    conn.execute(
        "INSERT INTO folders (account_id, name, role) VALUES (?1, ?2, CASE WHEN ?2 = 'INBOX' THEN 'inbox' END)
         ON CONFLICT(account_id, name) DO NOTHING",
        params![account_id, name],
    )?;
    conn.query_row(
        "SELECT id, uidvalidity FROM folders WHERE account_id = ?1 AND name = ?2",
        params![account_id, name],
        |row| {
            Ok(FolderRow {
                id: row.get(0)?,
                uidvalidity: row.get::<_, Option<i64>>(1)?.map(|v| v as u32),
            })
        },
    )
    .map_err(Into::into)
}

pub fn update_folder_uid_state(conn: &Connection, folder_id: i64, uidvalidity: u32, uidnext: u32) -> Result<()> {
    conn.execute(
        "UPDATE folders SET uidvalidity = ?2, uidnext = ?3, last_synced_at = strftime('%s','now')
         WHERE id = ?1",
        params![folder_id, uidvalidity as i64, uidnext as i64],
    )?;
    Ok(())
}

const FOLDER_COLUMNS: &str = "id, account_id, name, delimiter, role, selectable, unseen, total";

fn folder_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Folder> {
    let role: Option<String> = row.get(4)?;
    Ok(Folder {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        delimiter: row.get(3)?,
        role: role.as_deref().and_then(FolderRole::parse),
        selectable: row.get(5)?,
        unseen: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        total: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
    })
}

pub fn folder(conn: &Connection, folder_id: i64) -> Result<Folder> {
    conn.query_row(
        &format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE id = ?1"),
        params![folder_id],
        folder_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::Store(format!("no folder with id {folder_id}")))
}

/// Every folder of an account, in sidebar order (see `Folder::sort_key`).
pub fn list_folders(conn: &Connection, account_id: i64) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE account_id = ?1"))?;
    let mut folders = stmt
        .query_map(params![account_id], folder_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    folders.sort_by_key(Folder::sort_key);
    Ok(folders)
}

pub fn folder_by_role(conn: &Connection, account_id: i64, role: FolderRole) -> Result<Option<Folder>> {
    conn.query_row(
        &format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE account_id = ?1 AND role = ?2 ORDER BY id LIMIT 1"),
        params![account_id, role.as_str()],
        folder_from_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Brings an account's folder rows in line with what the server just
/// listed: adds new folders, updates the role/separator/selectability of
/// known ones, and drops those the server no longer has (deleted or
/// renamed from another client) along with their cached messages, whose
/// raw `.eml` paths are returned for the caller to delete. An empty
/// listing changes nothing — more likely a server hiccup than an account
/// with no folders at all — and INBOX is never dropped.
pub(crate) fn replace_folders(conn: &Connection, account_id: i64, remote: &[RemoteFolder]) -> Result<Vec<String>> {
    if remote.is_empty() {
        return Ok(Vec::new());
    }
    for f in remote {
        conn.execute(
            "INSERT INTO folders (account_id, name, delimiter, role, selectable) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, name) DO UPDATE SET
                delimiter = excluded.delimiter,
                role = excluded.role,
                selectable = excluded.selectable",
            params![
                account_id,
                f.name,
                f.delimiter,
                f.role.map(FolderRole::as_str),
                f.selectable
            ],
        )?;
    }

    let listed: std::collections::HashSet<&str> = remote.iter().map(|f| f.name.as_str()).collect();
    let mut stmt = conn.prepare("SELECT id, name FROM folders WHERE account_id = ?1")?;
    let gone: Vec<i64> = stmt
        .query_map(params![account_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, name)| !listed.contains(name.as_str()) && name != "INBOX")
        .map(|(id, _)| id)
        .collect();

    let mut paths = Vec::new();
    for id in gone {
        paths.extend(clear_folder_messages(conn, id)?);
        conn.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
    }
    Ok(paths)
}

/// Records the server's unread and total message counts for a folder.
pub fn update_folder_counts(conn: &Connection, folder_id: i64, unseen: u32, total: u32) -> Result<()> {
    conn.execute(
        "UPDATE folders SET unseen = ?2, total = ?3 WHERE id = ?1",
        params![folder_id, unseen, total],
    )?;
    Ok(())
}

pub fn update_folder_counts_by_name(
    conn: &Connection,
    account_id: i64,
    name: &str,
    unseen: u32,
    total: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET unseen = ?3, total = ?4 WHERE account_id = ?1 AND name = ?2",
        params![account_id, name, unseen, total],
    )?;
    Ok(())
}

/// Wipes all cached messages for a folder — used when the server's
/// `UIDVALIDITY` no longer matches ours, meaning our UIDs are meaningless.
///
/// Returns the on-disk paths of whatever raw `.eml` files those messages
/// had cached, for the caller to delete — they live outside SQLite. Also
/// resets the folder's backfill floor, since it described UIDs that no
/// longer mean anything.
pub fn clear_folder_messages(conn: &Connection, folder_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT raw_path FROM messages WHERE folder_id = ?1 AND raw_path IS NOT NULL")?;
    let paths = stmt
        .query_map(params![folder_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    conn.execute("DELETE FROM messages WHERE folder_id = ?1", params![folder_id])?;
    conn.execute(
        "UPDATE folders SET backfill_floor_uid = NULL WHERE id = ?1",
        params![folder_id],
    )?;
    Ok(paths)
}

/// The lowest UID "load older mail" has backfilled down to (or the initial
/// sync started from) — older mail is searched for strictly below this.
/// Deliberately not `MIN(uid)`: server-side search results get cached too,
/// and can be far older than the contiguous window.
pub fn backfill_floor(conn: &Connection, folder_id: i64) -> Result<Option<u32>> {
    conn.query_row(
        "SELECT backfill_floor_uid FROM folders WHERE id = ?1",
        params![folder_id],
        |row| row.get::<_, Option<i64>>(0),
    )
    .optional()
    .map(|v| v.flatten().map(|v| v as u32))
    .map_err(Into::into)
}

/// Moves the backfill floor down to `uid` — never up, so a later, newer
/// batch can't undo an earlier backfill.
pub fn lower_backfill_floor(conn: &Connection, folder_id: i64, uid: u32) -> Result<()> {
    conn.execute(
        "UPDATE folders SET backfill_floor_uid = ?2
         WHERE id = ?1 AND (backfill_floor_uid IS NULL OR backfill_floor_uid > ?2)",
        params![folder_id, uid],
    )?;
    Ok(())
}

/// Sets or clears one flag on a cached message. Returns whether that
/// changed anything (the message is cached and the flag was different).
pub fn set_flag(conn: &Connection, folder_id: i64, uid: u32, flag: FlagKind, on: bool) -> Result<bool> {
    let column = match flag {
        FlagKind::Seen => "seen",
        FlagKind::Flagged => "flagged",
        FlagKind::Answered => "answered",
    };
    let changed = conn.execute(
        &format!("UPDATE messages SET {column} = ?3 WHERE folder_id = ?1 AND uid = ?2 AND {column} != ?3"),
        params![folder_id, uid, on],
    )?;
    Ok(changed > 0)
}

pub fn upsert_envelope(conn: &Connection, folder_id: i64, env: &Envelope) -> Result<()> {
    let from_json = serde_json::to_string(&env.from).unwrap_or_else(|_| "[]".into());
    let to_json = serde_json::to_string(&env.to).unwrap_or_else(|_| "[]".into());
    let date_ts = env.date.map(|d| d.timestamp());

    conn.execute(
        "INSERT INTO messages
            (folder_id, uid, subject, from_addrs, to_addrs, date, seen, answered, flagged, deleted, draft)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(folder_id, uid) DO UPDATE SET
            subject = excluded.subject,
            from_addrs = excluded.from_addrs,
            to_addrs = excluded.to_addrs,
            date = excluded.date,
            seen = excluded.seen,
            answered = excluded.answered,
            flagged = excluded.flagged,
            deleted = excluded.deleted,
            draft = excluded.draft",
        params![
            folder_id,
            env.uid,
            env.subject,
            from_json,
            to_json,
            date_ts,
            env.flags.seen,
            env.flags.answered,
            env.flags.flagged,
            env.flags.deleted,
            env.flags.draft,
        ],
    )?;
    Ok(())
}

pub fn update_message_flags(conn: &Connection, folder_id: i64, uid: u32, flags: Flags) -> Result<()> {
    conn.execute(
        "UPDATE messages SET seen = ?3, answered = ?4, flagged = ?5, deleted = ?6, draft = ?7
         WHERE folder_id = ?1 AND uid = ?2",
        params![
            folder_id,
            uid,
            flags.seen,
            flags.answered,
            flags.flagged,
            flags.deleted,
            flags.draft,
        ],
    )?;
    Ok(())
}

pub fn list_envelopes(conn: &Connection, folder_id: i64, limit: u32) -> Result<Vec<Envelope>> {
    let mut stmt = conn.prepare(
        "SELECT uid, subject, from_addrs, to_addrs, date, seen, answered, flagged, deleted, draft
         FROM messages WHERE folder_id = ?1 ORDER BY uid DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![folder_id, limit], |row| {
        let from_json: String = row.get(2)?;
        let to_json: String = row.get(3)?;
        let date_ts: Option<i64> = row.get(4)?;
        Ok(Envelope {
            uid: row.get(0)?,
            subject: row.get(1)?,
            from: parse_addrs(&from_json),
            to: parse_addrs(&to_json),
            date: date_ts.and_then(ts_to_datetime),
            flags: Flags {
                seen: row.get(5)?,
                answered: row.get(6)?,
                flagged: row.get(7)?,
                deleted: row.get(8)?,
                draft: row.get(9)?,
            },
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn max_uid(conn: &Connection, folder_id: i64) -> Result<Option<u32>> {
    conn.query_row(
        "SELECT MAX(uid) FROM messages WHERE folder_id = ?1",
        params![folder_id],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map(|v| v.map(|v| v as u32))
    .map_err(Into::into)
}

/// The lowest UID cached for a folder. Only tests need it: "load older
/// mail" deliberately goes by `backfill_floor` instead.
#[cfg(test)]
pub fn min_uid(conn: &Connection, folder_id: i64) -> Result<Option<u32>> {
    conn.query_row(
        "SELECT MIN(uid) FROM messages WHERE folder_id = ?1",
        params![folder_id],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map(|v| v.map(|v| v as u32))
    .map_err(Into::into)
}

/// UIDs of the most recently cached messages, for the periodic flag-refresh
/// sweep (catches read/flag changes made from another client, without
/// needing CONDSTORE/QRESYNC support from the server).
pub fn recent_uids(conn: &Connection, folder_id: i64, limit: u32) -> Result<Vec<u32>> {
    let mut stmt = conn.prepare("SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid DESC LIMIT ?2")?;
    let rows = stmt.query_map(params![folder_id, limit], |row| row.get::<_, i64>(0))?;
    rows.map(|r| r.map(|v| v as u32))
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn set_message_raw_path(conn: &Connection, folder_id: i64, uid: u32, path: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET raw_path = ?3 WHERE folder_id = ?1 AND uid = ?2",
        params![folder_id, uid, path],
    )?;
    Ok(())
}

pub fn get_message_raw_path(conn: &Connection, folder_id: i64, uid: u32) -> Result<Option<String>> {
    conn.query_row(
        "SELECT raw_path FROM messages WHERE folder_id = ?1 AND uid = ?2",
        params![folder_id, uid],
        |row| row.get(0),
    )
    .optional()
    .map(|v| v.flatten())
    .map_err(Into::into)
}

/// Removes a message from the cache and returns its on-disk raw path (if
/// any) so the caller can delete that file too — it lives outside SQLite
/// so cascading deletes don't reach it.
pub fn delete_message(conn: &Connection, folder_id: i64, uid: u32) -> Result<Option<String>> {
    let raw_path = get_message_raw_path(conn, folder_id, uid)?;
    conn.execute(
        "DELETE FROM messages WHERE folder_id = ?1 AND uid = ?2",
        params![folder_id, uid],
    )?;
    Ok(raw_path)
}

/// A compose session saved for later.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavedDraft {
    /// Identifies one compose session's draft across saves: saving again
    /// under the same key replaces it.
    pub key: String,
    pub draft: Draft,
    /// The frontend's own description of the attached files, opaque to
    /// mail-core.
    pub attachments: String,
    /// The message this draft replies to, as `(folder_id, uid)`, so it
    /// can be marked answered once the reply goes out.
    pub answers: Option<(i64, u32)>,
    /// When it was last saved; filled in by the store.
    pub updated_at: Option<DateTime<Utc>>,
}

pub fn save_draft(conn: &Connection, account_id: i64, saved: &SavedDraft) -> Result<()> {
    let d = &saved.draft;
    conn.execute(
        "INSERT INTO drafts (account_id, key, to_addrs, cc_addrs, bcc_addrs, subject, body,
                             in_reply_to, references_hdr, attachments, answers_folder_id, answers_uid, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET
            to_addrs = excluded.to_addrs,
            cc_addrs = excluded.cc_addrs,
            bcc_addrs = excluded.bcc_addrs,
            subject = excluded.subject,
            body = excluded.body,
            in_reply_to = excluded.in_reply_to,
            references_hdr = excluded.references_hdr,
            attachments = excluded.attachments,
            answers_folder_id = excluded.answers_folder_id,
            answers_uid = excluded.answers_uid,
            updated_at = excluded.updated_at",
        params![
            account_id,
            saved.key,
            d.to,
            d.cc,
            d.bcc,
            d.subject,
            d.body,
            d.in_reply_to,
            d.references.join(" "),
            saved.attachments,
            saved.answers.map(|(folder_id, _)| folder_id),
            saved.answers.map(|(_, uid)| uid),
        ],
    )?;
    Ok(())
}

/// An account's saved drafts, most recently saved first.
pub fn list_drafts(conn: &Connection, account_id: i64) -> Result<Vec<SavedDraft>> {
    let mut stmt = conn.prepare(
        "SELECT key, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, references_hdr,
                attachments, answers_folder_id, answers_uid, updated_at
         FROM drafts WHERE account_id = ?1 AND key IS NOT NULL ORDER BY updated_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![account_id], |row| {
        let references: String = row.get(7)?;
        let answers = match (row.get::<_, Option<i64>>(9)?, row.get::<_, Option<i64>>(10)?) {
            (Some(folder_id), Some(uid)) => Some((folder_id, uid as u32)),
            _ => None,
        };
        Ok(SavedDraft {
            key: row.get(0)?,
            draft: Draft {
                to: row.get(1)?,
                cc: row.get(2)?,
                bcc: row.get(3)?,
                subject: row.get(4)?,
                body: row.get(5)?,
                in_reply_to: row.get(6)?,
                references: references.split_whitespace().map(str::to_string).collect(),
            },
            attachments: row.get(8)?,
            answers,
            updated_at: row.get::<_, i64>(11).ok().and_then(ts_to_datetime),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn delete_draft(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM drafts WHERE key = ?1", params![key])?;
    Ok(())
}

/// How many queued messages an account has: still being retried, and
/// given up on after `max_attempts` failures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxCounts {
    pub pending: u32,
    pub failed: u32,
}

pub fn outbox_counts(conn: &Connection, account_id: i64, max_attempts: i64) -> Result<OutboxCounts> {
    conn.query_row(
        "SELECT COALESCE(SUM(attempts < ?2), 0), COALESCE(SUM(attempts >= ?2), 0) FROM outbox WHERE account_id = ?1",
        params![account_id, max_attempts],
        |row| {
            Ok(OutboxCounts {
                pending: row.get::<_, i64>(0)? as u32,
                failed: row.get::<_, i64>(1)? as u32,
            })
        },
    )
    .map_err(Into::into)
}

pub struct OutboxItem {
    pub id: i64,
    pub raw_mime_path: String,
    pub recipients: Vec<String>,
    pub attempts: i64,
}

pub fn insert_outbox(conn: &Connection, account_id: i64, raw_mime_path: &str, recipients: &[String]) -> Result<i64> {
    conn.execute(
        "INSERT INTO outbox (account_id, raw_mime_path, recipients, created_at)
         VALUES (?1, ?2, ?3, strftime('%s','now'))",
        params![account_id, raw_mime_path, recipients.join(",")],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_outbox(conn: &Connection, account_id: i64) -> Result<Vec<OutboxItem>> {
    let mut stmt =
        conn.prepare("SELECT id, raw_mime_path, recipients, attempts FROM outbox WHERE account_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![account_id], |row| {
        let recipients: String = row.get(2)?;
        Ok(OutboxItem {
            id: row.get(0)?,
            raw_mime_path: row.get(1)?,
            recipients: recipients
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            attempts: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn delete_outbox(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn record_outbox_failure(conn: &Connection, id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
        params![id, error],
    )?;
    Ok(())
}

fn parse_addrs(json: &str) -> Vec<Address> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The set of distinct senders an account has ever received mail from
/// (across whatever's cached — currently just INBOX), for compose's
/// recipient autocomplete. Automated "do not reply" senders are dropped
/// since suggesting them back as a recipient is never useful. When the
/// same address turns up under multiple display names (or none), any
/// non-empty name seen wins over a missing one.
pub fn known_senders(conn: &Connection, account_id: i64) -> Result<Vec<Address>> {
    let mut stmt = conn.prepare(
        "SELECT m.from_addrs FROM messages m
         JOIN folders f ON f.id = m.folder_id
         WHERE f.account_id = ?1",
    )?;
    let rows = stmt.query_map(params![account_id], |row| row.get::<_, String>(0))?;
    // Mail in Sent (or a reply to oneself) has the account itself as
    // sender — never a useful suggestion.
    let own: Option<String> = conn
        .query_row("SELECT email FROM accounts WHERE id = ?1", params![account_id], |row| {
            row.get(0)
        })
        .optional()?;

    let mut by_email: HashMap<String, Address> = HashMap::new();
    for json in rows {
        for addr in parse_addrs(&json?) {
            if is_no_reply_address(&addr.email)
                || own.as_deref().is_some_and(|own| own.eq_ignore_ascii_case(&addr.email))
            {
                continue;
            }
            let key = addr.email.to_lowercase();
            match by_email.get_mut(&key) {
                Some(existing) if addr.name.is_some() => existing.name = addr.name,
                Some(_) => {}
                None => {
                    by_email.insert(key, addr);
                }
            }
        }
    }

    let mut senders: Vec<Address> = by_email.into_values().collect();
    senders.sort_by(|a, b| {
        let a_key = a.name.as_deref().unwrap_or(&a.email).to_lowercase();
        let b_key = b.name.as_deref().unwrap_or(&b.email).to_lowercase();
        a_key.cmp(&b_key)
    });
    Ok(senders)
}

/// Matches "no-reply", "noreply", "no.reply", "do-not-reply", etc.
/// case-insensitively by stripping punctuation from the local part before
/// comparing — catches more than the literal "no-reply@" without risking
/// a false positive on a real name that merely contains "reply".
fn is_no_reply_address(email: &str) -> bool {
    let local = email.split('@').next().unwrap_or(email);
    let normalized: String = local
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    normalized.contains("noreply") || normalized.contains("donotreply")
}

fn ts_to_datetime(ts: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(ts, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::init_connection;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_connection(&conn).unwrap();
        conn
    }

    fn sample_envelope(uid: u32, subject: &str) -> Envelope {
        Envelope {
            uid,
            subject: subject.to_string(),
            from: vec![Address {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
            }],
            to: vec![Address {
                name: None,
                email: "bob@example.com".into(),
            }],
            date: Utc.timestamp_opt(1_700_000_000, 0).single(),
            flags: Flags::default(),
        }
    }

    #[test]
    fn upsert_account_inserts_then_updates_on_conflict() {
        let conn = test_conn();
        let id1 = upsert_account(&conn, "a@example.com", Some("A")).unwrap();
        let id2 = upsert_account(&conn, "a@example.com", Some("A Updated")).unwrap();
        assert_eq!(id1, id2, "same email must resolve to the same account row");

        let name: Option<String> = conn
            .query_row("SELECT display_name FROM accounts WHERE id = ?1", params![id1], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name.as_deref(), Some("A Updated"));
    }

    #[test]
    fn delete_account_cascades_to_folders_and_messages() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "Hi")).unwrap();

        delete_account(&conn, account_id).unwrap();

        let folder_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM folders", [], |r| r.get(0))
            .unwrap();
        let message_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(folder_count, 0, "deleting an account must cascade to its folders");
        assert_eq!(message_count, 0, "deleting an account must cascade to its messages");
    }

    #[test]
    fn get_or_create_folder_is_idempotent() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let f1 = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        let f2 = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        assert_eq!(f1.id, f2.id);
        assert_eq!(f1.uidvalidity, None);
    }

    #[test]
    fn update_folder_uid_state_persists() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        update_folder_uid_state(&conn, folder.id, 42, 100).unwrap();

        let refreshed = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        assert_eq!(refreshed.uidvalidity, Some(42));
        let uidnext: Option<i64> = conn
            .query_row("SELECT uidnext FROM folders WHERE id = ?1", params![folder.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(uidnext, Some(100));
    }

    #[test]
    fn clear_folder_messages_removes_only_that_folders_messages() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder_a = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        let folder_b = get_or_create_folder(&conn, account_id, "Archive").unwrap();
        upsert_envelope(&conn, folder_a.id, &sample_envelope(1, "A")).unwrap();
        upsert_envelope(&conn, folder_b.id, &sample_envelope(1, "B")).unwrap();

        clear_folder_messages(&conn, folder_a.id).unwrap();

        assert!(list_envelopes(&conn, folder_a.id, 10).unwrap().is_empty());
        assert_eq!(list_envelopes(&conn, folder_b.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn clear_folder_messages_returns_cached_raw_paths_and_resets_the_floor() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "A")).unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(2, "B")).unwrap();
        set_message_raw_path(&conn, folder.id, 2, "/tmp/2.eml").unwrap();
        lower_backfill_floor(&conn, folder.id, 1).unwrap();

        let paths = clear_folder_messages(&conn, folder.id).unwrap();

        assert_eq!(paths, vec!["/tmp/2.eml".to_string()]);
        assert_eq!(backfill_floor(&conn, folder.id).unwrap(), None);
    }

    #[test]
    fn the_backfill_floor_only_ever_moves_down() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        assert_eq!(backfill_floor(&conn, folder.id).unwrap(), None);

        lower_backfill_floor(&conn, folder.id, 500).unwrap();
        assert_eq!(backfill_floor(&conn, folder.id).unwrap(), Some(500));
        lower_backfill_floor(&conn, folder.id, 800).unwrap();
        assert_eq!(
            backfill_floor(&conn, folder.id).unwrap(),
            Some(500),
            "a higher uid must not raise it"
        );
        lower_backfill_floor(&conn, folder.id, 450).unwrap();
        assert_eq!(backfill_floor(&conn, folder.id).unwrap(), Some(450));
    }

    #[test]
    fn caching_an_old_search_hit_does_not_move_the_backfill_floor() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        for uid in 900..=1000 {
            upsert_envelope(&conn, folder.id, &sample_envelope(uid, "x")).unwrap();
        }
        lower_backfill_floor(&conn, folder.id, 900).unwrap();

        upsert_envelope(&conn, folder.id, &sample_envelope(5, "ancient search hit")).unwrap();

        assert_eq!(min_uid(&conn, folder.id).unwrap(), Some(5));
        assert_eq!(backfill_floor(&conn, folder.id).unwrap(), Some(900));
    }

    #[test]
    fn set_flag_reports_whether_anything_changed() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "x")).unwrap();

        assert!(set_flag(&conn, folder.id, 1, FlagKind::Seen, true).unwrap());
        assert!(
            !set_flag(&conn, folder.id, 1, FlagKind::Seen, true).unwrap(),
            "already read"
        );
        assert!(list_envelopes(&conn, folder.id, 10).unwrap()[0].flags.seen);
        assert!(
            !set_flag(&conn, folder.id, 999, FlagKind::Seen, true).unwrap(),
            "missing uid is a no-op"
        );

        assert!(set_flag(&conn, folder.id, 1, FlagKind::Flagged, true).unwrap());
        assert!(set_flag(&conn, folder.id, 1, FlagKind::Seen, false).unwrap());
        let flags = list_envelopes(&conn, folder.id, 10).unwrap()[0].flags;
        assert!(flags.flagged && !flags.seen && !flags.answered);
    }

    fn remote(name: &str, role: Option<FolderRole>) -> RemoteFolder {
        RemoteFolder {
            name: name.to_string(),
            delimiter: Some("/".to_string()),
            role,
            selectable: true,
        }
    }

    #[test]
    fn replace_folders_adds_updates_and_drops_with_inbox_first() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let inbox = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        let old = get_or_create_folder(&conn, account_id, "Old").unwrap();
        upsert_envelope(&conn, old.id, &sample_envelope(3, "in a folder about to vanish")).unwrap();
        set_message_raw_path(&conn, old.id, 3, "/cache/3.eml").unwrap();

        let paths = replace_folders(
            &conn,
            account_id,
            &[
                remote("Trash", Some(FolderRole::Trash)),
                remote("Archive", None),
                remote("INBOX", Some(FolderRole::Inbox)),
                remote("Sent", Some(FolderRole::Sent)),
            ],
        )
        .unwrap();

        assert_eq!(paths, vec!["/cache/3.eml".to_string()]);
        let names: Vec<String> = list_folders(&conn, account_id)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["INBOX", "Sent", "Trash", "Archive"]);
        assert_eq!(folder(&conn, inbox.id).unwrap().role, Some(FolderRole::Inbox));
        assert!(folder(&conn, old.id).is_err(), "gone from the server, gone here");
        assert_eq!(
            folder_by_role(&conn, account_id, FolderRole::Trash)
                .unwrap()
                .unwrap()
                .name,
            "Trash"
        );

        // An empty listing is a hiccup, not "every folder was deleted".
        assert!(replace_folders(&conn, account_id, &[]).unwrap().is_empty());
        assert_eq!(list_folders(&conn, account_id).unwrap().len(), 4);
    }

    #[test]
    fn folder_counts_are_stored() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let inbox = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        update_folder_counts(&conn, inbox.id, 3, 120).unwrap();
        let f = folder(&conn, inbox.id).unwrap();
        assert_eq!((f.unseen, f.total), (Some(3), Some(120)));
        update_folder_counts_by_name(&conn, account_id, "INBOX", 0, 121).unwrap();
        assert_eq!(folder(&conn, inbox.id).unwrap().unseen, Some(0));
    }

    fn saved(key: &str, subject: &str) -> SavedDraft {
        SavedDraft {
            key: key.to_string(),
            draft: Draft {
                to: "bob@example.com".into(),
                subject: subject.into(),
                body: "hi".into(),
                in_reply_to: Some("orig@example.com".into()),
                references: vec!["root@example.com".into(), "orig@example.com".into()],
                ..Draft::default()
            },
            attachments: "[]".into(),
            answers: Some((1, 42)),
            updated_at: None,
        }
    }

    #[test]
    fn drafts_save_under_their_key_and_round_trip() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();

        save_draft(&conn, account_id, &saved("k1", "first")).unwrap();
        save_draft(&conn, account_id, &saved("k1", "first, edited")).unwrap();
        save_draft(&conn, account_id, &saved("k2", "second")).unwrap();

        let drafts = list_drafts(&conn, account_id).unwrap();
        assert_eq!(drafts.len(), 2, "saving again under a key replaces that draft");
        let first = drafts.iter().find(|d| d.key == "k1").unwrap();
        assert_eq!(first.draft.subject, "first, edited");
        assert_eq!(first.draft.references, saved("k1", "").draft.references);
        assert_eq!(first.draft.in_reply_to.as_deref(), Some("orig@example.com"));
        assert_eq!(first.answers, Some((1, 42)));
        assert!(first.updated_at.is_some());

        delete_draft(&conn, "k1").unwrap();
        assert_eq!(list_drafts(&conn, account_id).unwrap().len(), 1);
    }

    #[test]
    fn outbox_counts_split_pending_from_given_up() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        assert_eq!(outbox_counts(&conn, account_id, 3).unwrap(), OutboxCounts::default());

        let a = insert_outbox(&conn, account_id, "/a.eml", &["x@example.com".into()]).unwrap();
        insert_outbox(&conn, account_id, "/b.eml", &["y@example.com".into()]).unwrap();
        for _ in 0..3 {
            record_outbox_failure(&conn, a, "nope").unwrap();
        }
        assert_eq!(
            outbox_counts(&conn, account_id, 3).unwrap(),
            OutboxCounts { pending: 1, failed: 1 }
        );
    }

    #[test]
    fn upsert_envelope_updates_existing_row_on_conflict_instead_of_duplicating() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "Original")).unwrap();

        let mut updated = sample_envelope(1, "Updated subject");
        updated.flags.seen = true;
        upsert_envelope(&conn, folder.id, &updated).unwrap();

        let rows = list_envelopes(&conn, folder.id, 10).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "re-upserting the same (folder_id, uid) must not duplicate the row"
        );
        assert_eq!(rows[0].subject, "Updated subject");
        assert!(rows[0].flags.seen);
    }

    #[test]
    fn update_message_flags_changes_only_the_targeted_message() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "One")).unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(2, "Two")).unwrap();

        update_message_flags(
            &conn,
            folder.id,
            1,
            Flags {
                seen: true,
                flagged: true,
                ..Flags::default()
            },
        )
        .unwrap();

        let rows = list_envelopes(&conn, folder.id, 10).unwrap();
        let one = rows.iter().find(|e| e.uid == 1).unwrap();
        let two = rows.iter().find(|e| e.uid == 2).unwrap();
        assert!(one.flags.seen && one.flags.flagged);
        assert!(!two.flags.seen && !two.flags.flagged);
    }

    #[test]
    fn list_envelopes_orders_newest_uid_first_and_respects_limit() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        for uid in 1..=5u32 {
            upsert_envelope(&conn, folder.id, &sample_envelope(uid, &format!("Msg {uid}"))).unwrap();
        }

        let rows = list_envelopes(&conn, folder.id, 3).unwrap();
        let uids: Vec<u32> = rows.iter().map(|e| e.uid).collect();
        assert_eq!(uids, vec![5, 4, 3]);
    }

    #[test]
    fn max_and_min_uid_reflect_the_cached_range() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        assert_eq!(max_uid(&conn, folder.id).unwrap(), None);
        assert_eq!(min_uid(&conn, folder.id).unwrap(), None);

        for uid in [10u32, 3, 7] {
            upsert_envelope(&conn, folder.id, &sample_envelope(uid, "x")).unwrap();
        }

        assert_eq!(max_uid(&conn, folder.id).unwrap(), Some(10));
        assert_eq!(min_uid(&conn, folder.id).unwrap(), Some(3));
    }

    #[test]
    fn recent_uids_returns_newest_first_bounded_by_limit() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        for uid in 1..=5u32 {
            upsert_envelope(&conn, folder.id, &sample_envelope(uid, "x")).unwrap();
        }

        assert_eq!(recent_uids(&conn, folder.id, 2).unwrap(), vec![5, 4]);
    }

    #[test]
    fn raw_path_round_trips_and_delete_message_returns_it() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "x")).unwrap();

        assert_eq!(get_message_raw_path(&conn, folder.id, 1).unwrap(), None);
        set_message_raw_path(&conn, folder.id, 1, "/tmp/1.eml").unwrap();
        assert_eq!(
            get_message_raw_path(&conn, folder.id, 1).unwrap(),
            Some("/tmp/1.eml".to_string())
        );

        let removed_path = delete_message(&conn, folder.id, 1).unwrap();
        assert_eq!(removed_path, Some("/tmp/1.eml".to_string()));
        assert!(list_envelopes(&conn, folder.id, 10).unwrap().is_empty());
    }

    #[test]
    fn delete_message_on_missing_uid_is_a_harmless_no_op() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        assert_eq!(delete_message(&conn, folder.id, 999).unwrap(), None);
    }

    #[test]
    fn outbox_insert_list_and_delete_round_trip() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let id = insert_outbox(
            &conn,
            account_id,
            "/tmp/out.eml",
            &["x@example.com".to_string(), "y@example.com".to_string()],
        )
        .unwrap();

        let items = list_outbox(&conn, account_id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].recipients, vec!["x@example.com", "y@example.com"]);
        assert_eq!(items[0].attempts, 0);

        record_outbox_failure(&conn, id, "smtp timeout").unwrap();
        let items = list_outbox(&conn, account_id).unwrap();
        assert_eq!(items[0].attempts, 1);

        delete_outbox(&conn, id).unwrap();
        assert!(list_outbox(&conn, account_id).unwrap().is_empty());
    }

    #[test]
    fn parse_addrs_falls_back_to_empty_on_malformed_json() {
        assert!(parse_addrs("not json").is_empty());
        assert!(parse_addrs("[]").is_empty());
    }

    #[test]
    fn is_no_reply_address_matches_common_variants_but_not_real_senders() {
        assert!(is_no_reply_address("no-reply@example.com"));
        assert!(is_no_reply_address("NoReply@Example.com"));
        assert!(is_no_reply_address("noreply@example.com"));
        assert!(is_no_reply_address("no.reply@example.com"));
        assert!(is_no_reply_address("do-not-reply@example.com"));
        assert!(!is_no_reply_address("alice@example.com"));
        assert!(!is_no_reply_address("replyguy@example.com"));
    }

    #[test]
    fn known_senders_deduplicates_by_email_case_insensitively_and_excludes_no_reply_addresses() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "me@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();

        let mut msg1 = sample_envelope(1, "Hi");
        msg1.from = vec![Address {
            name: Some("Alice".into()),
            email: "alice@example.com".into(),
        }];
        upsert_envelope(&conn, folder.id, &msg1).unwrap();

        let mut msg2 = sample_envelope(2, "Hi again");
        msg2.from = vec![Address {
            name: None,
            email: "ALICE@example.com".into(),
        }];
        upsert_envelope(&conn, folder.id, &msg2).unwrap();

        let mut msg3 = sample_envelope(3, "Your receipt");
        msg3.from = vec![Address {
            name: Some("Shop".into()),
            email: "no-reply@shop.com".into(),
        }];
        upsert_envelope(&conn, folder.id, &msg3).unwrap();

        let senders = known_senders(&conn, account_id).unwrap();

        assert_eq!(
            senders.len(),
            1,
            "alice's two case variants must collapse into one entry, and no-reply must be dropped"
        );
        assert_eq!(senders[0].email.to_lowercase(), "alice@example.com");
        assert_eq!(
            senders[0].name.as_deref(),
            Some("Alice"),
            "a known name must survive even though a later message for the same address lacked one"
        );
    }

    #[test]
    fn known_senders_is_scoped_to_the_given_account() {
        let conn = test_conn();
        let account_a = upsert_account(&conn, "a@example.com", None).unwrap();
        let account_b = upsert_account(&conn, "b@example.com", None).unwrap();
        let folder_a = get_or_create_folder(&conn, account_a, "INBOX").unwrap();
        let folder_b = get_or_create_folder(&conn, account_b, "INBOX").unwrap();

        upsert_envelope(&conn, folder_a.id, &sample_envelope(1, "Hi")).unwrap();
        upsert_envelope(&conn, folder_b.id, &sample_envelope(1, "Hi")).unwrap();

        assert_eq!(known_senders(&conn, account_a).unwrap().len(), 1);

        let account_c = upsert_account(&conn, "c@example.com", None).unwrap(); // an account with no mail at all
        assert!(known_senders(&conn, account_c).unwrap().is_empty());
    }
}
