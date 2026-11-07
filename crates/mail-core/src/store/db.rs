use std::path::Path;
use std::sync::mpsc as std_mpsc;

use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::error::{Error, Result};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// A handle to the local SQLite cache. All actual database access happens
/// on one dedicated OS thread (the "store actor") that owns the single
/// `rusqlite::Connection` in WAL mode — this sidesteps `Connection`'s lack
/// of `Sync` and avoids write contention without needing a connection pool
/// at this scale. Cloning a `Store` just clones the channel handle.
#[derive(Clone)]
pub struct Store {
    tx: std_mpsc::Sender<Job>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            crate::config::restrict_dir(parent)?;
        }

        let (tx, rx) = std_mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let path = path.to_path_buf();

        std::thread::Builder::new()
            .name("mail-store".into())
            .spawn(move || {
                let mut conn = match Connection::open(&path).map_err(Error::from) {
                    Ok(conn) => conn,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = init_connection(&conn) {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
                // Best-effort: a mailbox's cached subjects/senders/bodies
                // live in this file, so lock it down even though the
                // parent directory (already 0700) is the primary defense.
                if let Err(e) = crate::config::restrict_file(&path) {
                    tracing::warn!("failed to restrict permissions on {path:?}: {e}");
                }
                let _ = ready_tx.send(Ok(()));

                for job in rx {
                    job(&mut conn);
                }
            })
            .map_err(|e| Error::Store(format!("failed to spawn store thread: {e}")))?;

        ready_rx
            .recv()
            .map_err(|_| Error::Store("store thread exited during init".into()))??;

        Ok(Self { tx })
    }

    /// Runs `f` against the store's connection on its dedicated thread and
    /// awaits the result. `f` must not block on anything other than SQLite
    /// itself — it runs synchronously on the actor thread. Takes `&mut
    /// Connection` (rather than `&Connection`) solely so callers doing
    /// several writes can open a `Transaction` — existing closures that
    /// only need shared access are unaffected, since `&mut Connection`
    /// auto-reborrows to `&Connection` wherever that's all a callee needs.
    pub async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.submit(f).await
    }

    /// Like `run`, but queues `f` on the store thread right away, when
    /// called, rather than when the returned future is first polled. Jobs
    /// submitted one after another from the same thread therefore run in
    /// that order even when their futures are spawned onto separate tasks
    /// (whose polling order the runtime doesn't guarantee) — which saving
    /// and then deleting a draft depends on.
    pub fn submit<F, T>(&self, f: F) -> impl std::future::Future<Output = Result<T>> + Send + 'static
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (resp_tx, resp_rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = resp_tx.send(f(conn));
        });
        let queued = self
            .tx
            .send(job)
            .map_err(|_| Error::Store("store thread is no longer running".into()));
        async move {
            queued?;
            resp_rx
                .await
                .map_err(|_| Error::Store("store thread dropped the response".into()))?
        }
    }
}

pub(crate) fn init_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let initial = version;
    if version == 0 {
        // SCHEMA_V1 as it stands already includes the version-2 change.
        conn.execute_batch(super::schema::SCHEMA_V1)?;
        version = 2;
    }
    if version == 1 {
        // messages_folder_date was never read by any query (all of them
        // filter/sort by (folder_id, uid), already covered by the
        // UNIQUE(folder_id, uid) constraint's own index) — just dead
        // weight on every insert/update. Drop it for databases created
        // before this was noticed; SCHEMA_V1 no longer creates it at all.
        conn.execute_batch("DROP INDEX IF EXISTS messages_folder_date;")?;
        version = 2;
    }
    if version == 2 {
        conn.execute_batch(super::schema::MIGRATION_V3)?;
        version = 3;
    }
    if version == 3 {
        conn.execute_batch(super::schema::MIGRATION_V4)?;
        version = 4;
    }
    if version != initial {
        conn.pragma_update(None, "user_version", version)?;
    }
    Ok(())
}

/// The schema version `init_connection` brings every database to.
#[cfg(test)]
pub(crate) const CURRENT_VERSION: i64 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap()
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_some()
    }

    #[test]
    fn fresh_database_lands_on_current_schema_version() {
        let conn = Connection::open_in_memory().unwrap();
        init_connection(&conn).unwrap();
        assert_eq!(user_version(&conn), CURRENT_VERSION);
        // Sanity check the schema actually applied, not just the pragma.
        conn.execute("INSERT INTO accounts (email) VALUES ('a@example.com')", [])
            .unwrap();
    }

    #[test]
    fn init_connection_is_idempotent_across_repeated_opens() {
        let conn = Connection::open_in_memory().unwrap();
        init_connection(&conn).unwrap();
        init_connection(&conn).unwrap();
        assert_eq!(user_version(&conn), CURRENT_VERSION);
    }

    #[test]
    fn migrates_a_version_1_database_by_dropping_the_dead_index() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a database created before messages_folder_date was
        // removed from SCHEMA_V1: apply the schema, then hand-add the
        // index and stamp it as version 1, mirroring what an old install
        // would actually have on disk.
        conn.execute_batch(super::super::schema::SCHEMA_V1).unwrap();
        conn.execute_batch("CREATE INDEX messages_folder_date ON messages(folder_id, date);")
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        assert!(index_exists(&conn, "messages_folder_date"));

        init_connection(&conn).unwrap();

        assert_eq!(user_version(&conn), CURRENT_VERSION);
        assert!(!index_exists(&conn, "messages_folder_date"));
    }

    #[test]
    fn migrates_a_version_2_database_seeding_the_backfill_floor_from_the_cache() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(super::super::schema::SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, email) VALUES (1, 'a@example.com');
             INSERT INTO folders (id, account_id, name) VALUES (1, 1, 'INBOX'), (2, 1, 'Empty');
             INSERT INTO messages (folder_id, uid) VALUES (1, 40), (1, 17), (1, 99);",
        )
        .unwrap();

        init_connection(&conn).unwrap();

        assert_eq!(user_version(&conn), CURRENT_VERSION);
        let floor = |id: i64| -> Option<i64> {
            conn.query_row("SELECT backfill_floor_uid FROM folders WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(floor(1), Some(17));
        assert_eq!(floor(2), None);
    }

    #[test]
    fn migrates_a_version_3_database_marking_inbox_and_keying_drafts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(super::super::schema::SCHEMA_V1).unwrap();
        conn.execute_batch(super::super::schema::MIGRATION_V3).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, email) VALUES (1, 'a@example.com');
             INSERT INTO folders (id, account_id, name) VALUES (1, 1, 'INBOX'), (2, 1, 'Archive');",
        )
        .unwrap();

        init_connection(&conn).unwrap();

        assert_eq!(user_version(&conn), CURRENT_VERSION);
        let role = |id: i64| -> Option<String> {
            conn.query_row("SELECT role FROM folders WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(role(1).as_deref(), Some("inbox"));
        assert_eq!(role(2), None);
        assert!(index_exists(&conn, "drafts_key"));
    }
}
