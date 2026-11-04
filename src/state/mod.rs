//! The SQLite state store.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

pub mod daemon;
pub mod items;

const MIGRATION_LIST: &[M<'static>] = &[
    M::up(include_str!("schema.sql")),
    // Which version of the kind logic decided `videos.kind`; see
    // `items::KIND_VERSION`. Kinds from before are decided again.
    M::up("ALTER TABLE videos ADD COLUMN kind_version INTEGER;"),
    // When a video's download finished (status `fetched`), so they're
    // processed in that order.
    M::up("ALTER TABLE items ADD COLUMN fetched_at TEXT;"),
    // `tidepool run --daemon`: its schedule, its services, and requests from
    // `tidepool daemon`.
    M::up(
        "CREATE TABLE daemon_state (key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE daemon_services (
           name TEXT PRIMARY KEY,
           paused INTEGER NOT NULL DEFAULT 0,
           status TEXT,                 -- running, idle, stopped, restarting
           activity TEXT,               -- what it's doing, for `daemon status`
           started_at TEXT,
           restarts INTEGER NOT NULL DEFAULT 0,
           last_error TEXT, last_error_at TEXT,
           updated_at TEXT
         );
         CREATE TABLE daemon_requests (
           id INTEGER PRIMARY KEY,
           service TEXT,                -- NULL: every service
           action TEXT NOT NULL,        -- restart, pause, resume, sync
           created_at TEXT NOT NULL,
           done_at TEXT, result TEXT
         );",
    ),
    // Work the web UI asks the daemon to do for one video (run hooks, embed,
    // download again, restore from the trash).
    M::up(
        "CREATE TABLE daemon_jobs (
           id INTEGER PRIMARY KEY,
           action TEXT NOT NULL,        -- hooks, embed, reset, reset_delete, restore
           source_key TEXT NOT NULL, video_id TEXT NOT NULL,
           created_at TEXT NOT NULL, started_at TEXT, done_at TEXT,
           ok INTEGER, result TEXT
         );",
    ),
    // Each source's own schedule: the daemon syncs a source once its
    // `next_sync_at` has passed (or it never synced). The schedule used to be
    // one time for every source.
    M::up(
        "ALTER TABLE sources ADD COLUMN last_synced_at TEXT;
         ALTER TABLE sources ADD COLUMN next_sync_at TEXT;
         DELETE FROM daemon_state WHERE key = 'next_sync_at';",
    ),
    // Videos imported from a source's own backfill folder: not on its
    // playlists, so never "removed" from them nor downloaded from YouTube.
    M::up("ALTER TABLE items ADD COLUMN local INTEGER NOT NULL DEFAULT 0;"),
];
const MIGRATIONS: Migrations<'static> = Migrations::from_slice(MIGRATION_LIST);

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state database migration: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("cannot create {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// An open state database.
#[derive(Debug)]
pub struct State {
    pub conn: Connection,
}

impl State {
    /// Open (creating if needed) the DB at `path`. The file is owner-only
    /// (0600) since it stores API keys; WAL mode, migrations applied.
    ///
    /// # Errors
    ///
    /// [`StateError::Io`] if the file or its directory can't be created or
    /// restricted, or a DB error if it can't be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        let io = |source| StateError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// An in-memory DB, for tests.
    ///
    /// # Errors
    ///
    /// If it can't be opened or migrated.
    pub fn open_in_memory() -> Result<Self, StateError> {
        Self::init(Connection::open_in_memory()?)
    }

    /// A write transaction. It takes the write lock up front, so a busy DB
    /// (another service or process writing) is waited for (`busy_timeout`)
    /// instead of failing when a read inside it turns into a write.
    ///
    /// # Errors
    ///
    /// If the write lock can't be taken (still busy after the timeout).
    pub fn write_transaction(conn: &mut Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    }

    fn init(mut conn: Connection) -> Result<Self, StateError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        MIGRATIONS.to_latest(&mut conn)?;
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        assert!(MIGRATIONS.validate().is_ok());
    }

    #[test]
    fn concurrent_writers_wait_for_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        State::open(&path).unwrap();
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut state = State::open(&path).unwrap();
                    for _ in 0..50 {
                        let tx = State::write_transaction(&mut state.conn).unwrap();
                        // Read, then write: the pattern that fails without
                        // the write lock taken up front.
                        let n: i64 = tx
                            .query_row("SELECT count(*) FROM runs", [], |r| r.get(0))
                            .unwrap();
                        tx.execute("INSERT INTO runs (command) VALUES (?1)", [n.to_string()])
                            .unwrap();
                        tx.commit().unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer failed");
        }
        let state = State::open(&path).unwrap();
        let n: i64 = state
            .conn
            .query_row("SELECT count(*) FROM runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 200);
    }

    #[test]
    fn kinds_from_before_are_decided_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // A database from before kinds had a version, with a Premiere stored
        // as a live stream.
        {
            let mut conn = Connection::open(&path).unwrap();
            Migrations::from_slice(&MIGRATION_LIST[..1])
                .to_latest(&mut conn)
                .unwrap();
            conn.execute(
                "INSERT INTO videos (id, title, kind) VALUES ('dJkbfKpZdWM', 'Infame', 'live')",
                [],
            )
            .unwrap();
        }
        let state = State::open(&path).unwrap();
        let ids = vec!["dJkbfKpZdWM".to_string()];
        assert!(
            items::known_kinds(&state.conn, &ids).unwrap().is_empty(),
            "not trusted: decided again"
        );
        assert_eq!(
            items::videos(&state.conn, &ids).unwrap()["dJkbfKpZdWM"].kind,
            None
        );
    }

    #[test]
    fn databases_from_before_fetching_was_separate_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            Migrations::from_slice(&MIGRATION_LIST[..2])
                .to_latest(&mut conn)
                .unwrap();
        }
        let state = State::open(&path).unwrap();
        state
            .conn
            .execute("INSERT INTO sources (key, lists) VALUES ('lofi', '[]')", [])
            .unwrap();
        state
            .conn
            .execute("INSERT INTO videos (id, title) VALUES ('a', 'A')", [])
            .unwrap();
        state
            .conn
            .execute(
                "INSERT INTO items (source_key, video_id, position, in_remote, status)
                 VALUES ('lofi', 'a', 1, 1, 'pending')",
                [],
            )
            .unwrap();
        items::mark_fetched(&state.conn, "lofi", "a", jiff::Timestamp::now()).unwrap();
        let queue = items::process_queue(&state.conn, &["lofi".to_string()]).unwrap();
        assert_eq!(queue.len(), 1);
        assert!(queue[0].fetched_at.is_some());
    }

    #[test]
    fn creates_owner_only_db_with_all_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib/state.db");
        let state = State::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut stmt = state
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(stmt);
        for t in [
            "config_revisions",
            "hook_runs",
            "hook_attempts",
            "items",
            "sources",
            "quota_usage",
            "runs",
            "videos",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing {t}: {tables:?}");
        }
        let mode: String = state
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(state);
        State::open(&path).expect("reopening is idempotent");
    }
}
