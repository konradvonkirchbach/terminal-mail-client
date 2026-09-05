use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::types::{Address, Envelope, Flags};

pub struct FolderRow {
    pub id: i64,
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
}

pub fn upsert_account(conn: &Connection, email: &str, display_name: Option<&str>) -> Result<i64> {
    conn.execute(
        "INSERT INTO accounts (email, display_name) VALUES (?1, ?2)
         ON CONFLICT(email) DO UPDATE SET display_name = excluded.display_name",
        params![email, display_name],
    )?;
    let id = conn.query_row(
        "SELECT id FROM accounts WHERE email = ?1",
        params![email],
        |row| row.get(0),
    )?;
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
        "INSERT INTO folders (account_id, name) VALUES (?1, ?2)
         ON CONFLICT(account_id, name) DO NOTHING",
        params![account_id, name],
    )?;
    conn.query_row(
        "SELECT id, uidvalidity, uidnext FROM folders WHERE account_id = ?1 AND name = ?2",
        params![account_id, name],
        |row| {
            Ok(FolderRow {
                id: row.get(0)?,
                uidvalidity: row.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                uidnext: row.get::<_, Option<i64>>(2)?.map(|v| v as u32),
            })
        },
    )
    .map_err(Into::into)
}

pub fn update_folder_uid_state(
    conn: &Connection,
    folder_id: i64,
    uidvalidity: u32,
    uidnext: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET uidvalidity = ?2, uidnext = ?3, last_synced_at = strftime('%s','now')
         WHERE id = ?1",
        params![folder_id, uidvalidity as i64, uidnext as i64],
    )?;
    Ok(())
}

/// Wipes all cached messages for a folder — used when the server's
/// `UIDVALIDITY` no longer matches ours, meaning our UIDs are meaningless.
pub fn clear_folder_messages(conn: &Connection, folder_id: i64) -> Result<()> {
    conn.execute("DELETE FROM messages WHERE folder_id = ?1", params![folder_id])?;
    Ok(())
}

pub fn upsert_envelope(conn: &Connection, folder_id: i64, env: &Envelope) -> Result<()> {
    let from_json = serde_json::to_string(&env.from).unwrap_or_else(|_| "[]".into());
    let to_json = serde_json::to_string(&env.to).unwrap_or_else(|_| "[]".into());
    let date_ts = env.date.map(|d| d.timestamp());

    conn.execute(
        "INSERT INTO messages
            (folder_id, uid, subject, from_addrs, to_addrs, date, seen, answered, flagged, deleted, draft, has_attachments)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(folder_id, uid) DO UPDATE SET
            subject = excluded.subject,
            from_addrs = excluded.from_addrs,
            to_addrs = excluded.to_addrs,
            date = excluded.date,
            seen = excluded.seen,
            answered = excluded.answered,
            flagged = excluded.flagged,
            deleted = excluded.deleted,
            draft = excluded.draft,
            has_attachments = excluded.has_attachments",
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
            env.has_attachments,
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
        "SELECT uid, subject, from_addrs, to_addrs, date, seen, answered, flagged, deleted, draft, has_attachments
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
            has_attachments: row.get(10)?,
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

/// The lowest UID cached for a folder — the boundary "load older mail"
/// backfills below.
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
    let mut stmt = conn.prepare(
        "SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid DESC LIMIT ?2",
    )?;
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

pub struct OutboxItem {
    pub id: i64,
    pub raw_mime_path: String,
    pub recipients: Vec<String>,
    pub attempts: i64,
}

pub fn insert_outbox(
    conn: &Connection,
    account_id: i64,
    raw_mime_path: &str,
    recipients: &[String],
) -> Result<i64> {
    conn.execute(
        "INSERT INTO outbox (account_id, raw_mime_path, recipients, created_at)
         VALUES (?1, ?2, ?3, strftime('%s','now'))",
        params![account_id, raw_mime_path, recipients.join(",")],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_outbox(conn: &Connection, account_id: i64) -> Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, raw_mime_path, recipients, attempts FROM outbox WHERE account_id = ?1 ORDER BY id",
    )?;
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
            from: vec![Address { name: Some("Alice".into()), email: "alice@example.com".into() }],
            to: vec![Address { name: None, email: "bob@example.com".into() }],
            date: Utc.timestamp_opt(1_700_000_000, 0).single(),
            flags: Flags::default(),
            has_attachments: false,
        }
    }

    #[test]
    fn upsert_account_inserts_then_updates_on_conflict() {
        let conn = test_conn();
        let id1 = upsert_account(&conn, "a@example.com", Some("A")).unwrap();
        let id2 = upsert_account(&conn, "a@example.com", Some("A Updated")).unwrap();
        assert_eq!(id1, id2, "same email must resolve to the same account row");

        let name: Option<String> = conn
            .query_row("SELECT display_name FROM accounts WHERE id = ?1", params![id1], |r| r.get(0))
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

        let folder_count: i64 = conn.query_row("SELECT COUNT(*) FROM folders", [], |r| r.get(0)).unwrap();
        let message_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
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
        assert_eq!(f1.uidnext, None);
    }

    #[test]
    fn update_folder_uid_state_persists() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        update_folder_uid_state(&conn, folder.id, 42, 100).unwrap();

        let refreshed = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        assert_eq!(refreshed.uidvalidity, Some(42));
        assert_eq!(refreshed.uidnext, Some(100));
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
    fn upsert_envelope_updates_existing_row_on_conflict_instead_of_duplicating() {
        let conn = test_conn();
        let account_id = upsert_account(&conn, "a@example.com", None).unwrap();
        let folder = get_or_create_folder(&conn, account_id, "INBOX").unwrap();
        upsert_envelope(&conn, folder.id, &sample_envelope(1, "Original")).unwrap();

        let mut updated = sample_envelope(1, "Updated subject");
        updated.flags.seen = true;
        upsert_envelope(&conn, folder.id, &updated).unwrap();

        let rows = list_envelopes(&conn, folder.id, 10).unwrap();
        assert_eq!(rows.len(), 1, "re-upserting the same (folder_id, uid) must not duplicate the row");
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
            Flags { seen: true, flagged: true, ..Flags::default() },
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
        assert_eq!(get_message_raw_path(&conn, folder.id, 1).unwrap(), Some("/tmp/1.eml".to_string()));

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
}
