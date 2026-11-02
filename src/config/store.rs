//! Config revisions in the state DB. TOML files are how a config
//! gets in and out; each revision keeps the TOML text verbatim (comments
//! included) next to the validated, normalized JSON.

use std::fmt::{self, Write as _};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::load::{ConfigError, Loaded, load_str};

/// Where a revision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Import,
    Ui,
    Rollback,
    Merge,
}

impl Source {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Import => "import",
            Source::Ui => "ui",
            Source::Rollback => "rollback",
            Source::Merge => "merge",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "ui" => Source::Ui,
            "rollback" => Source::Rollback,
            "merge" => Source::Merge,
            _ => Source::Import,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stored config revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub rev: i64,
    pub created_at: String,
    pub source: Source,
    /// Imported file path; for rollbacks, `rev:<n>`.
    pub source_path: Option<String>,
    /// SHA-256 of the TOML text.
    pub source_hash: String,
    pub toml: String,
    pub json: String,
}

/// What an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Imported {
    /// Identical to the current revision; nothing stored.
    Unchanged(i64),
    Created(i64),
}

impl Imported {
    #[must_use]
    pub fn rev(self) -> i64 {
        match self {
            Imported::Unchanged(rev) | Imported::Created(rev) => rev,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("revision {0} does not exist (see `tidepool config history`)")]
    NoSuchRevision(i64),
    #[error(
        "tidepool has no config yet; run `tidepool init` (or `tidepool config import <path>`) first"
    )]
    Empty,
    #[error(
        "{path} differs from the stored config, which was changed by {changed_by} (revision {rev}) after it was last imported.\n\
         Refusing to overwrite those edits. Compare with `tidepool config diff {path}`, \
         keep them with `tidepool config export --show-secrets > {path}`, or pass \
         --force-config to import anyway."
    )]
    Conflict {
        path: String,
        changed_by: Source,
        rev: i64,
    },
}

/// SHA-256 of `text`, hex-encoded.
#[must_use]
pub fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Config revisions.
pub struct ConfigStore<'a> {
    conn: &'a Connection,
}

const COLUMNS: &str = "rev, created_at, source, source_path, source_hash, toml, json";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Revision> {
    Ok(Revision {
        rev: r.get(0)?,
        created_at: r.get(1)?,
        source: Source::parse(&r.get::<_, String>(2)?),
        source_path: r.get(3)?,
        source_hash: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        toml: r.get(5)?,
        json: r.get(6)?,
    })
}

impl<'a> ConfigStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The latest revision, if there is one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails.
    pub fn current(&self) -> Result<Option<Revision>, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM config_revisions ORDER BY rev DESC LIMIT 1");
        Ok(self.conn.query_row(&sql, [], row).optional()?)
    }

    /// Revision `rev`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails, and
    /// [`StoreError::NoSuchRevision`] when there's no revision `rev`.
    pub fn get(&self, rev: i64) -> Result<Revision, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM config_revisions WHERE rev = ?1");
        self.conn
            .query_row(&sql, [rev], row)
            .optional()?
            .ok_or(StoreError::NoSuchRevision(rev))
    }

    /// All revisions, oldest first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails.
    pub fn history(&self) -> Result<Vec<Revision>, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM config_revisions ORDER BY rev");
        let mut stmt = self.conn.prepare(&sql)?;
        let revisions = stmt.query_map([], row)?.collect::<Result<_, _>>()?;
        Ok(revisions)
    }

    /// The most recent revision imported from a file.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails.
    pub fn last_import(&self) -> Result<Option<Revision>, StoreError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM config_revisions WHERE source = 'import' AND source_path IS NOT NULL \
             ORDER BY rev DESC LIMIT 1"
        );
        Ok(self.conn.query_row(&sql, [], row).optional()?)
    }

    /// The current revision, re-validated against this binary's schema.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails,
    /// [`StoreError::Empty`] when there's no revision yet, and
    /// [`StoreError::Config`] when the stored config no longer loads.
    pub fn load_current(&self) -> Result<(Revision, Loaded), StoreError> {
        let current = self.current()?.ok_or(StoreError::Empty)?;
        let loaded = load_str(&format!("config revision {}", current.rev), &current.toml)?;
        Ok((current, loaded))
    }

    fn insert(
        &self,
        source: Source,
        path: Option<&str>,
        text: &str,
        loaded: &Loaded,
    ) -> Result<i64, StoreError> {
        let now = jiff::Timestamp::now().to_string();
        self.conn.execute(
            "INSERT INTO config_revisions (created_at, source, source_path, source_hash, toml, json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now, source.as_str(), path, hash(text), text, loaded.normalized()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Store `text` (already validated as `loaded`) as a new revision, unless
    /// the current revision has the same text and source path.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails.
    pub fn import(
        &self,
        path: Option<&str>,
        text: &str,
        loaded: &Loaded,
    ) -> Result<Imported, StoreError> {
        if let Some(current) = self.current()? {
            let same_path = path.is_none() || current.source_path.as_deref() == path;
            if current.source_hash == hash(text) && same_path {
                return Ok(Imported::Unchanged(current.rev));
            }
        }
        Ok(Imported::Created(self.insert(
            Source::Import,
            path,
            text,
            loaded,
        )?))
    }

    /// `--config <path>`: import if the file changed. If the current revision
    /// came from the UI, a rollback or a merge, a differing file is a conflict
    /// unless `force`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails,
    /// [`StoreError::Conflict`] for a conflict, and [`StoreError::Config`]
    /// when `text` doesn't load.
    pub fn import_if_changed(
        &self,
        path: &str,
        text: &str,
        force: bool,
    ) -> Result<Imported, StoreError> {
        if let Some(current) = self.current()? {
            if current.source_hash == hash(text) {
                return Ok(Imported::Unchanged(current.rev));
            }
            if current.source != Source::Import && !force {
                return Err(StoreError::Conflict {
                    path: path.to_string(),
                    changed_by: current.source,
                    rev: current.rev,
                });
            }
        }
        let loaded = load_str(path, text)?;
        self.import(Some(path), text, &loaded)
    }

    /// Store text edited in the web UI as a new revision.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails.
    pub fn save_edit(&self, text: &str, loaded: &Loaded) -> Result<i64, StoreError> {
        self.insert(Source::Ui, None, text, loaded)
    }

    /// Make `rev` current again, as a new revision.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the database query fails,
    /// [`StoreError::NoSuchRevision`] when there's no revision `rev`, and
    /// [`StoreError::Config`] when it no longer loads.
    pub fn rollback(&self, rev: i64) -> Result<i64, StoreError> {
        let old = self.get(rev)?;
        let loaded = load_str(&format!("config revision {rev}"), &old.toml)?;
        self.insert(
            Source::Rollback,
            Some(&format!("rev:{rev}")),
            &old.toml,
            &loaded,
        )
    }
}

/// Write a config to `path`, keeping what was there as `<path>.bak` (with
/// its permissions: it may hold an API key). Returns the backup's path.
///
/// # Errors
///
/// When backing up the old file, or writing and moving the new one into
/// place, fails.
pub fn write_file(
    path: &std::path::Path,
    text: &str,
) -> std::io::Result<Option<std::path::PathBuf>> {
    let with = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        std::path::PathBuf::from(name)
    };
    let permissions = std::fs::metadata(path).map(|m| m.permissions()).ok();
    let backup = if path.exists() {
        let backup = with(".bak");
        std::fs::copy(path, &backup)?;
        Some(backup)
    } else {
        None
    };
    let staged = with(".tidepool-partial");
    std::fs::write(&staged, text)?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&staged, permissions)?;
    }
    std::fs::rename(&staged, path)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    const A: &str = "# my playlists\n[sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLaaaaaaaa'] # comment kept\n";
    const B: &str = "[sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLbbbbbbbb']\n";

    fn import(store: &ConfigStore<'_>, path: &str, text: &str) -> Imported {
        store
            .import(Some(path), text, &load_str(path, text).unwrap())
            .unwrap()
    }

    #[test]
    fn import_is_a_no_op_when_unchanged_and_keeps_text_verbatim() {
        let state = State::open_in_memory().unwrap();
        let store = ConfigStore::new(&state.conn);
        assert!(store.current().unwrap().is_none());
        assert_eq!(import(&store, "/c.toml", A), Imported::Created(1));
        assert_eq!(import(&store, "/c.toml", A), Imported::Unchanged(1));
        assert_eq!(import(&store, "/c.toml", B), Imported::Created(2));
        assert_eq!(store.get(1).unwrap().toml, A, "comments survive");
        let current = store.current().unwrap().unwrap();
        assert_eq!(current.source, Source::Import);
        assert_eq!(current.source_path.as_deref(), Some("/c.toml"));
        assert_eq!(
            current.json,
            r#"{"sources":{"lofi":{"urls":["https://www.youtube.com/playlist?list=PLbbbbbbbb"]}}}"#
        );
    }

    #[test]
    fn rollback_creates_a_new_revision() {
        let state = State::open_in_memory().unwrap();
        let store = ConfigStore::new(&state.conn);
        import(&store, "/c.toml", A);
        import(&store, "/c.toml", B);
        assert_eq!(store.rollback(1).unwrap(), 3);
        let current = store.current().unwrap().unwrap();
        assert_eq!(
            (current.source, current.toml.as_str()),
            (Source::Rollback, A)
        );
        assert_eq!(store.history().unwrap().len(), 3);
        assert!(matches!(
            store.rollback(9),
            Err(StoreError::NoSuchRevision(9))
        ));
    }

    #[test]
    fn import_if_changed_guards_against_overwriting_other_edits() {
        let state = State::open_in_memory().unwrap();
        let store = ConfigStore::new(&state.conn);
        assert_eq!(
            store.import_if_changed("/c.toml", A, false).unwrap(),
            Imported::Created(1)
        );
        assert_eq!(
            store.import_if_changed("/c.toml", A, false).unwrap(),
            Imported::Unchanged(1)
        );
        assert_eq!(
            store.import_if_changed("/c.toml", B, false).unwrap(),
            Imported::Created(2)
        );

        store.rollback(1).unwrap(); // stands in for a UI edit
        let err = store.import_if_changed("/c.toml", B, false).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Conflict {
                    changed_by: Source::Rollback,
                    rev: 3,
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(
            store.import_if_changed("/c.toml", B, true).unwrap(),
            Imported::Created(4)
        );
    }

    #[test]
    fn invalid_files_are_never_stored() {
        let state = State::open_in_memory().unwrap();
        let store = ConfigStore::new(&state.conn);
        assert!(store.import_if_changed("/c.toml", "[nope]", false).is_err());
        assert!(store.current().unwrap().is_none());
    }

    #[test]
    fn load_current_revalidates() {
        let state = State::open_in_memory().unwrap();
        let store = ConfigStore::new(&state.conn);
        assert!(matches!(store.load_current(), Err(StoreError::Empty)));
        import(&store, "/c.toml", A);
        let (rev, loaded) = store.load_current().unwrap();
        assert_eq!(rev.rev, 1);
        assert!(loaded.config.sources.contains_key("lofi"));

        // Simulate a revision stored by a build with a different schema.
        state
            .conn
            .execute(
                "UPDATE config_revisions SET toml = '[general]\nconcurency = 2\n'",
                [],
            )
            .unwrap();
        let err = store.load_current().unwrap_err().to_string();
        assert_eq!(
            err,
            "config revision 1:2:1 general.concurency: unknown key \"concurency\""
        );
    }
}
