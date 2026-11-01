//! The SQLite state store.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

pub mod items;

const MIGRATION_LIST: &[M<'static>] = &[
    M::up(include_str!("schema.sql")),
    // Which version of the kind logic decided `videos.kind`; see
    // `items::KIND_VERSION`. Kinds from before are decided again.
    M::up("ALTER TABLE videos ADD COLUMN kind_version INTEGER;"),
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
    pub fn open_in_memory() -> Result<Self, StateError> {
        Self::init(Connection::open_in_memory()?)
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
