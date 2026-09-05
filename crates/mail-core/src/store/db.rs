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
            .map_err(|e| Error::Config(format!("failed to spawn store thread: {e}")))?;

        ready_rx
            .recv()
            .map_err(|_| Error::Config("store thread exited during init".into()))??;

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
        let (resp_tx, resp_rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = resp_tx.send(f(conn));
        });
        self.tx
            .send(job)
            .map_err(|_| Error::Config("store thread is no longer running".into()))?;
        resp_rx
            .await
            .map_err(|_| Error::Config("store thread dropped the response".into()))?
    }
}

fn init_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        conn.execute_batch(super::schema::SCHEMA_V1)?;
        conn.pragma_update(None, "user_version", 2)?;
    } else if version == 1 {
        // messages_folder_date was never read by any query (all of them
        // filter/sort by (folder_id, uid), already covered by the
        // UNIQUE(folder_id, uid) constraint's own index) — just dead
        // weight on every insert/update. Drop it for databases created
        // before this was noticed; SCHEMA_V1 no longer creates it at all.
        conn.execute_batch("DROP INDEX IF EXISTS messages_folder_date;")?;
        conn.pragma_update(None, "user_version", 2)?;
    }
    Ok(())
}
