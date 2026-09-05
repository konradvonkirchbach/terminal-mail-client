/// Initial schema. Applied once, tracked via `PRAGMA user_version`. Future
/// schema changes append a new `SCHEMA_V*` const and a branch in
/// `db::init_connection` rather than editing this one in place.
pub const SCHEMA_V1: &str = r#"
CREATE TABLE accounts (
    id          INTEGER PRIMARY KEY,
    email       TEXT NOT NULL UNIQUE,
    display_name TEXT
);

CREATE TABLE folders (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    uidvalidity     INTEGER,
    uidnext         INTEGER,
    last_synced_at  INTEGER,
    UNIQUE(account_id, name)
);

CREATE TABLE messages (
    id                  INTEGER PRIMARY KEY,
    folder_id           INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid                 INTEGER NOT NULL,
    message_id_header   TEXT,
    subject             TEXT NOT NULL DEFAULT '',
    from_addrs          TEXT NOT NULL DEFAULT '[]',
    to_addrs            TEXT NOT NULL DEFAULT '[]',
    date                INTEGER,
    seen                INTEGER NOT NULL DEFAULT 0,
    answered            INTEGER NOT NULL DEFAULT 0,
    flagged             INTEGER NOT NULL DEFAULT 0,
    deleted             INTEGER NOT NULL DEFAULT 0,
    draft               INTEGER NOT NULL DEFAULT 0,
    -- No longer written or read by the app (it could only ever be false —
    -- envelope sync fetches headers only, never BODYSTRUCTURE — so real
    -- attachment detection would be a fetch-shape change, not a rename).
    -- Left in place rather than migrated away: an unused DEFAULT 0 column
    -- costs nothing, and dropping it isn't worth a schema migration.
    has_attachments     INTEGER NOT NULL DEFAULT 0,
    raw_path            TEXT,
    UNIQUE(folder_id, uid)
);
-- No separate index on (folder_id, date): every current query (list,
-- max_uid, recent_uids) filters/sorts by (folder_id, uid), which the
-- UNIQUE constraint above already indexes. An index nothing reads from
-- would just add write overhead to every sync.

CREATE TABLE attachments (
    id          INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename    TEXT,
    mime_type   TEXT,
    size_bytes  INTEGER
);

-- Unused until the sending phase; created now so that phase doesn't need
-- a schema migration of its own.
CREATE TABLE drafts (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    to_addrs    TEXT NOT NULL DEFAULT '',
    cc_addrs    TEXT NOT NULL DEFAULT '',
    bcc_addrs   TEXT NOT NULL DEFAULT '',
    subject     TEXT NOT NULL DEFAULT '',
    body        TEXT NOT NULL DEFAULT '',
    updated_at  INTEGER NOT NULL
);

CREATE TABLE outbox (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    raw_mime_path   TEXT NOT NULL,
    -- Comma-separated envelope recipients (To+Cc+Bcc). Kept separately
    -- because the Bcc header itself is stripped from raw_mime_path's
    -- contents before sending, so it can't be recovered by re-parsing.
    recipients      TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT
);
"#;
