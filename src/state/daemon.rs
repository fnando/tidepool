//! What `tidepool run --daemon` keeps in the state DB: its schedule, how its
//! services are doing, and requests from `tidepool daemon`.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

pub const NEXT_SYNC_AT: &str = "next_sync_at";
pub const LAST_SYNC_STARTED_AT: &str = "last_sync_started_at";
pub const LAST_SYNC_FINISHED_AT: &str = "last_sync_finished_at";

/// A daemon state value, if it's set.
///
/// # Errors
///
/// If the DB read fails.
pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM daemon_state WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()
    .map(Option::flatten)
}

/// A daemon state value as a timestamp, if it's set and parses.
///
/// # Errors
///
/// If the DB read fails.
pub fn get_time(conn: &Connection, key: &str) -> rusqlite::Result<Option<Timestamp>> {
    Ok(get(conn, key)?.and_then(|t| t.parse().ok()))
}

/// Set (or with `None`, clear) a daemon state value.
///
/// # Errors
///
/// If the DB write fails.
pub fn set(conn: &Connection, key: &str, value: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO daemon_state (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Set a daemon state value to a timestamp.
///
/// # Errors
///
/// If the DB write fails.
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

/// A service's row (a blank one if it never ran).
///
/// # Errors
///
/// If the DB read fails.
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

/// Pause or resume a service.
///
/// # Errors
///
/// If the DB write fails.
pub fn set_paused(conn: &Connection, name: &str, paused: bool) -> rusqlite::Result<()> {
    ensure(conn, name)?;
    conn.execute(
        "UPDATE daemon_services SET paused = ?2 WHERE name = ?1",
        params![name, paused],
    )?;
    Ok(())
}

/// A service started (again, after `restarts` unexpected stops).
///
/// # Errors
///
/// If the DB write fails.
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
///
/// # Errors
///
/// If the DB write fails.
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
///
/// # Errors
///
/// If the DB write fails.
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

/// Something `tidepool daemon` asked the running daemon to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: i64,
    /// `None`: every service.
    pub service: Option<String>,
    /// `restart` or `sync`.
    pub action: String,
}

/// Ask the running daemon to do something, returning the request's id.
///
/// # Errors
///
/// If the DB write fails.
pub fn request(
    conn: &Connection,
    service: Option<&str>,
    action: &str,
    now: Timestamp,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO daemon_requests (service, action, created_at) VALUES (?1, ?2, ?3)",
        params![service, action, now.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Requests not answered yet, oldest first.
///
/// # Errors
///
/// If the DB read fails.
pub fn waiting_requests(conn: &Connection) -> rusqlite::Result<Vec<Request>> {
    let mut stmt = conn.prepare(
        "SELECT id, service, action FROM daemon_requests WHERE done_at IS NULL ORDER BY id",
    )?;
    stmt.query_map([], |r| {
        Ok(Request {
            id: r.get(0)?,
            service: r.get(1)?,
            action: r.get(2)?,
        })
    })?
    .collect()
}

/// Answer a request, marking it done.
///
/// # Errors
///
/// If the DB write fails.
pub fn answer(conn: &Connection, id: i64, result: &str, now: Timestamp) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE daemon_requests SET done_at = ?2, result = ?3 WHERE id = ?1",
        params![id, now.to_string(), result],
    )?;
    Ok(())
}

/// The daemon's answer to a request, once it's given.
///
/// # Errors
///
/// If the DB read fails.
pub fn answer_of(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT result FROM daemon_requests WHERE id = ?1 AND done_at IS NOT NULL",
        [id],
        |r| r.get(0),
    )
    .optional()
    .map(Option::flatten)
}

/// Work asked for one video, done by the processor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Job {
    pub id: i64,
    /// `hooks`, `embed`, `reset`, `reset_delete` or `restore`.
    pub action: String,
    pub source_key: String,
    pub video_id: String,
    pub created_at: String,
    pub done_at: Option<String>,
    pub ok: Option<bool>,
    pub result: Option<String>,
}

pub const JOB_ACTIONS: [&str; 5] = ["hooks", "embed", "reset", "reset_delete", "restore"];

/// Queue a job for one video, returning its id.
///
/// # Errors
///
/// If the DB write fails.
pub fn add_job(
    conn: &Connection,
    action: &str,
    source_key: &str,
    video_id: &str,
    now: Timestamp,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO daemon_jobs (action, source_key, video_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![action, source_key, video_id, now.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

const JOB_COLUMNS: &str = "id, action, source_key, video_id, created_at, done_at, ok, result";

fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        action: r.get(1)?,
        source_key: r.get(2)?,
        video_id: r.get(3)?,
        created_at: r.get(4)?,
        done_at: r.get(5)?,
        ok: r.get(6)?,
        result: r.get(7)?,
    })
}

/// Jobs not done yet, oldest first.
///
/// # Errors
///
/// If the DB read fails.
pub fn waiting_jobs(conn: &Connection) -> rusqlite::Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM daemon_jobs WHERE done_at IS NULL ORDER BY id"
    ))?;
    stmt.query_map([], job_row)?.collect()
}

/// A video's latest jobs, newest first.
///
/// # Errors
///
/// If the DB read fails.
pub fn jobs_for(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM daemon_jobs WHERE source_key = ?1 AND video_id = ?2
         ORDER BY id DESC LIMIT ?3"
    ))?;
    stmt.query_map(
        params![
            source_key,
            video_id,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ],
        job_row,
    )?
    .collect()
}

/// Mark a job as started.
///
/// # Errors
///
/// If the DB write fails.
pub fn start_job(conn: &Connection, id: i64, now: Timestamp) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE daemon_jobs SET started_at = ?2 WHERE id = ?1",
        params![id, now.to_string()],
    )?;
    Ok(())
}

/// Mark a job as done, with how it went.
///
/// # Errors
///
/// If the DB write fails.
pub fn finish_job(
    conn: &Connection,
    id: i64,
    ok: bool,
    result: &str,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE daemon_jobs SET done_at = ?2, ok = ?3, result = ?4 WHERE id = ?1",
        params![id, now.to_string(), ok, result],
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

    #[test]
    fn jobs_wait_until_done() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let now = Timestamp::now();
        let id = add_job(conn, "hooks", "lofi", "a", now).unwrap();
        add_job(conn, "embed", "lofi", "b", now).unwrap();
        assert_eq!(waiting_jobs(conn).unwrap().len(), 2);
        start_job(conn, id, now).unwrap();
        finish_job(conn, id, true, "ran notify", now).unwrap();
        let waiting = waiting_jobs(conn).unwrap();
        assert_eq!((waiting.len(), waiting[0].action.as_str()), (1, "embed"));
        let done = &jobs_for(conn, "lofi", "a", 5).unwrap()[0];
        assert_eq!(
            (done.ok, done.result.as_deref()),
            (Some(true), Some("ran notify"))
        );
    }

    #[test]
    fn requests_wait_for_an_answer() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let now = Timestamp::now();
        let id = request(conn, Some("syncer"), "restart", now).unwrap();
        request(conn, None, "sync", now).unwrap();
        let waiting = waiting_requests(conn).unwrap();
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0].service.as_deref(), Some("syncer"));
        assert_eq!(answer_of(conn, id).unwrap(), None);
        answer(conn, id, "restarted syncer", now).unwrap();
        assert_eq!(
            answer_of(conn, id).unwrap().as_deref(),
            Some("restarted syncer")
        );
        assert_eq!(waiting_requests(conn).unwrap().len(), 1);
    }
}
