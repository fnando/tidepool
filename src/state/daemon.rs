//! What `tidepool run --daemon` keeps in the state DB: its schedule, how its
//! services are doing, and requests from `tidepool daemon`.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

pub const NEXT_SYNC_AT: &str = "next_sync_at";
pub const LAST_SYNC_STARTED_AT: &str = "last_sync_started_at";
pub const LAST_SYNC_FINISHED_AT: &str = "last_sync_finished_at";

pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM daemon_state WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()
    .map(Option::flatten)
}

pub fn get_time(conn: &Connection, key: &str) -> rusqlite::Result<Option<Timestamp>> {
    Ok(get(conn, key)?.and_then(|t| t.parse().ok()))
}

pub fn set(conn: &Connection, key: &str, value: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO daemon_state (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn set_time(conn: &Connection, key: &str, at: Timestamp) -> rusqlite::Result<()> {
    set(conn, key, Some(&at.to_string()))
}

/// A service's row, as `tidepool daemon status` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceRow {
    pub name: String,
    pub paused: bool,
    pub status: Option<String>,
    pub activity: Option<String>,
    pub started_at: Option<String>,
    pub restarts: i64,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
}

pub fn service(conn: &Connection, name: &str) -> rusqlite::Result<ServiceRow> {
    Ok(conn
        .query_row(
            "SELECT paused, status, activity, started_at, restarts, last_error, last_error_at
             FROM daemon_services WHERE name = ?1",
            [name],
            |r| {
                Ok(ServiceRow {
                    name: name.to_string(),
                    paused: r.get(0)?,
                    status: r.get(1)?,
                    activity: r.get(2)?,
                    started_at: r.get(3)?,
                    restarts: r.get(4)?,
                    last_error: r.get(5)?,
                    last_error_at: r.get(6)?,
                })
            },
        )
        .optional()?
        .unwrap_or(ServiceRow {
            name: name.to_string(),
            ..ServiceRow::default()
        }))
}

fn ensure(conn: &Connection, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO daemon_services (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
        [name],
    )?;
    Ok(())
}

pub fn set_paused(conn: &Connection, name: &str, paused: bool) -> rusqlite::Result<()> {
    ensure(conn, name)?;
    conn.execute(
        "UPDATE daemon_services SET paused = ?2 WHERE name = ?1",
        params![name, paused],
    )?;
    Ok(())
}

/// A service started (again, after `restarts` unexpected stops).
pub fn started(conn: &Connection, name: &str, now: Timestamp) -> rusqlite::Result<()> {
    ensure(conn, name)?;
    conn.execute(
        "UPDATE daemon_services SET status = 'running', activity = NULL, started_at = ?2,
           updated_at = ?2
         WHERE name = ?1",
        params![name, now.to_string()],
    )?;
    Ok(())
}

/// What a service is doing now (`status`: running, idle, stopped,
/// restarting).
pub fn set_status(
    conn: &Connection,
    name: &str,
    status: &str,
    activity: Option<&str>,
    now: Timestamp,
) -> rusqlite::Result<()> {
    ensure(conn, name)?;
    conn.execute(
        "UPDATE daemon_services SET status = ?2, activity = ?3, updated_at = ?4 WHERE name = ?1",
        params![name, status, activity, now.to_string()],
    )?;
    Ok(())
}

/// A service stopped unexpectedly.
pub fn failed(conn: &Connection, name: &str, error: &str, now: Timestamp) -> rusqlite::Result<()> {
    ensure(conn, name)?;
    conn.execute(
        "UPDATE daemon_services SET status = 'restarting', activity = NULL,
           restarts = restarts + 1, last_error = ?2, last_error_at = ?3, updated_at = ?3
         WHERE name = ?1",
        params![name, error, now.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    #[test]
    fn keeps_the_schedule_and_each_service() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let at: Timestamp = "2026-11-01T16:00:00Z".parse().unwrap();
        assert_eq!(get_time(conn, NEXT_SYNC_AT).unwrap(), None);
        set_time(conn, NEXT_SYNC_AT, at).unwrap();
        assert_eq!(get_time(conn, NEXT_SYNC_AT).unwrap(), Some(at));

        assert!(!service(conn, "syncer").unwrap().paused);
        set_paused(conn, "syncer", true).unwrap();
        started(conn, "syncer", at).unwrap();
        failed(conn, "syncer", "boom", at).unwrap();
        let row = service(conn, "syncer").unwrap();
        assert!(row.paused, "a pause outlives restarts");
        assert_eq!(row.status.as_deref(), Some("restarting"));
        assert_eq!((row.restarts, row.last_error.as_deref()), (1, Some("boom")));
    }
}
