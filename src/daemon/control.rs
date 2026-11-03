//! `tidepool daemon status|pause|resume|restart|sync`: see and steer the
//! running daemon through the state DB (the supervisor reads it every 2 s).

use std::fmt::Write as _;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::bail;

use super::service::{Service, local};
use crate::app::{App, exit};
use crate::cli::{DaemonCmd, DaemonService};
use crate::state::State;
use crate::state::daemon as store;

/// How long to wait for the daemon to answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

pub(crate) async fn command(
    app: &App<'_>,
    cmd: &DaemonCmd,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    let running = crate::lock::holder(&app.paths.scheduler_lock);
    if !app.paths.state_db.exists() {
        writeln!(out, "daemon: not running")?;
        return Ok(exit::OK);
    }
    let state = app.open_state()?;
    match cmd {
        DaemonCmd::Status => status(&state, running, out)?,
        DaemonCmd::Pause { service } => pause(&state, running, *service, true, out).await?,
        DaemonCmd::Resume { service } => pause(&state, running, *service, false, out).await?,
        DaemonCmd::Restart { service } => {
            let answer = ask(&state, running, service.map(name), "restart").await?;
            writeln!(out, "{answer}")?;
        }
        DaemonCmd::Sync => {
            if store::service(&state.conn, Service::Syncer.name())?.paused {
                bail!("the syncer is paused; `tidepool daemon resume syncer` first");
            }
            let answer = ask(&state, running, None, "sync").await?;
            writeln!(out, "{answer}")?;
        }
    }
    Ok(exit::OK)
}

fn name(service: DaemonService) -> &'static str {
    match service {
        DaemonService::Syncer => Service::Syncer.name(),
        DaemonService::Downloader => Service::Downloader.name(),
        DaemonService::Processor => Service::Processor.name(),
        DaemonService::Server => Service::Server.name(),
    }
}

fn targets(service: Option<DaemonService>) -> Vec<&'static str> {
    match service {
        Some(service) => vec![name(service)],
        None => Service::ALL.iter().map(|s| s.name()).collect(),
    }
}

fn status(state: &State, running: Option<u32>, out: &mut dyn Write) -> anyhow::Result<()> {
    match running {
        Some(pid) => writeln!(out, "daemon: running (pid {pid})")?,
        None => writeln!(out, "daemon: not running")?,
    }
    for service in Service::ALL {
        let row = store::service(&state.conn, service.name())?;
        let mut line = match (row.paused, running) {
            (true, _) => "paused".to_string(),
            (false, None) => "not running".to_string(),
            (false, Some(_)) => {
                let status = row.status.unwrap_or_else(|| "starting".into());
                match row.activity {
                    Some(activity) => format!("{status}, {activity}"),
                    None => status,
                }
            }
        };
        if row.restarts > 0 {
            let _ = write!(
                line,
                " (restarted {} time{}",
                row.restarts,
                if row.restarts == 1 { "" } else { "s" }
            );
            if let (Some(error), Some(at)) = (&row.last_error, &row.last_error_at) {
                let at = at.parse().map_or_else(|_| at.clone(), local);
                let _ = write!(line, "; last at {at}: {error}");
            }
            line.push(')');
        }
        writeln!(out, "{}: {line}", service.name())?;
    }
    if let Some(started) = store::get_time(&state.conn, store::LAST_SYNC_STARTED_AT)? {
        let finished =
            store::get_time(&state.conn, store::LAST_SYNC_FINISHED_AT)?.filter(|f| *f >= started);
        match finished {
            Some(finished) => writeln!(
                out,
                "last sync: {}, took {}",
                local(started),
                crate::config::types::format_duration(
                    Duration::try_from(finished.duration_since(started)).unwrap_or_default()
                )
            )?,
            None => writeln!(out, "last sync: {} (didn't finish)", local(started))?,
        }
    }
    let count =
        |sql: &str| -> rusqlite::Result<i64> { state.conn.query_row(sql, [], |r| r.get(0)) };
    let downloads = count("SELECT COUNT(*) FROM items WHERE status IN ('fetched', 'processing')")?;
    let hooks = count(
        "SELECT COUNT(*) FROM hook_runs WHERE type = 'finished' AND status IN ('pending', 'retrying')",
    )?;
    if downloads > 0 || hooks > 0 {
        let mut parts = Vec::new();
        if downloads > 0 {
            parts.push(format!(
                "{downloads} download{} to finish",
                if downloads == 1 { "" } else { "s" }
            ));
        }
        if hooks > 0 {
            parts.push(format!(
                "{hooks} hook{} to run",
                if hooks == 1 { "" } else { "s" }
            ));
        }
        writeln!(out, "waiting: {}", parts.join(", "))?;
    }
    Ok(())
}

/// `pause` (or `resume`, with `pause = false`): saved right away, so it
/// works whether the daemon runs or not; a running one follows within
/// seconds.
async fn pause(
    state: &State,
    running: Option<u32>,
    service: Option<DaemonService>,
    pause: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    for name in targets(service) {
        let was = store::service(&state.conn, name)?.paused;
        if was == pause {
            let how = if pause { "paused" } else { "not paused" };
            writeln!(out, "{name} is already {how}")?;
            continue;
        }
        store::set_paused(&state.conn, name, pause)?;
        let Some(_) = running else {
            let line = if pause {
                format!("paused {name}; it stays paused when the daemon starts")
            } else {
                format!("resumed {name}; it runs when the daemon starts")
            };
            writeln!(out, "{line}")?;
            continue;
        };
        // Wait for the daemon to follow.
        let start = Instant::now();
        let done = loop {
            let status = store::service(&state.conn, name)?.status;
            let followed = (status.as_deref() == Some("paused")) == pause;
            if followed || start.elapsed() >= ANSWER_WITHIN {
                break followed;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let line = match (pause, done) {
            (true, true) => format!("paused {name}"),
            (true, false) => {
                format!("pausing {name}; it stops once what it's doing winds down")
            }
            (false, true) => format!("resumed {name}"),
            (false, false) => format!("resuming {name}"),
        };
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Ask the running daemon to do something; returns its answer.
async fn ask(
    state: &State,
    running: Option<u32>,
    service: Option<&str>,
    action: &str,
) -> anyhow::Result<String> {
    if running.is_none() {
        bail!(
            "the daemon isn't running; start it with `tidepool run --daemon` \
             (or sync once with `tidepool run`)"
        );
    }
    let id = store::request(&state.conn, service, action, jiff::Timestamp::now())?;
    let start = Instant::now();
    while start.elapsed() < ANSWER_WITHIN {
        if let Some(answer) = store::answer_of(&state.conn, id)? {
            return Ok(answer);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("the daemon didn't answer within 10 seconds")
}
