//! `tidepool run --daemon`: a supervisor on the main thread that starts the
//! services (`syncer`, `processor`), watches for new config revisions,
//! restarts a service that stops unexpectedly, and stops everything on
//! SIGINT/SIGTERM.

pub mod schedule;
pub mod service;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::bail;
use tokio::sync::{Notify, watch};

use crate::app::{App, exit};
use crate::config::store::ConfigStore;
use crate::lock::{self, LockError};
use crate::problems::Kind;
use crate::state::State;
use crate::state::daemon as store;
use crate::state::items::{self, status};
use service::{Handle, Service, Signals};

/// How often the supervisor looks for config changes and requests.
const POLL: Duration = Duration::from_secs(2);

/// One service and how it's been doing.
struct Slot {
    service: Service,
    handle: Option<Handle>,
    /// Unexpected stops in a row.
    failures: u32,
    /// When to start it again after one.
    restart_at: Option<Instant>,
}

pub(crate) async fn run(app: &App<'_>) -> anyhow::Result<i32> {
    if let crate::cli::Command::Run(args) = &app.cli.command
        && args.on_config_mismatch.is_some()
    {
        bail!(
            "--on-config-mismatch isn't implemented yet (it comes with the daemon's config file check)"
        );
    }
    let _daemon = match lock::acquire(&app.paths.scheduler_lock) {
        Ok(held) => held,
        Err(LockError::Held { pid }) => bail!(
            "another tidepool daemon is running{}",
            pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
        ),
        Err(e) => return Err(e.into()),
    };
    let state = app.open_state()?;
    let config = app.current_config(&state)?.config;
    let Some(interval) = config.schedule.interval.0 else {
        bail!(
            "schedule.interval is empty, so the daemon has nothing to do; set it \
             (e.g. interval = \"6h\" under [schedule]) and import the config"
        );
    };

    // Whatever a crash left behind, while nothing else works on the library.
    let library = lock::Shared::new(&app.paths.lock);
    {
        let Some(_hold) = wait_for_library(&library, app).await? else {
            return Ok(exit::OK);
        };
        let reset = app.recover(&state)?;
        if reset > 0 {
            tracing::info!("put back {reset} videos that were interrupted last time");
        }
    }

    let mut revision = ConfigStore::new(&state.conn)
        .current()?
        .map(|r| r.rev)
        .unwrap_or(0);
    let (config_tx, config_rx) = watch::channel(revision);
    let signals = Signals {
        library,
        fetched: Arc::new(Notify::new()),
        sync_now: Arc::new(Notify::new()),
        config: config_rx,
    };
    let cli = Arc::new(app.cli.clone());
    let mut slots: Vec<Slot> = Service::ALL
        .into_iter()
        .map(|service| Slot {
            service,
            handle: None,
            failures: 0,
            restart_at: Some(Instant::now()),
        })
        .collect();
    tracing::info!(
        "tidepool daemon started (pid {}): syncing every {}",
        std::process::id(),
        crate::config::types::format_duration(interval)
    );

    let mut poll = tokio::time::interval(POLL);
    let mut recovered = false;
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            _ = app.shutdown.stopping() => break,
        }
        let now = jiff::Timestamp::now();
        if let Some(rev) = ConfigStore::new(&state.conn).current()?.map(|r| r.rev)
            && rev != revision
        {
            revision = rev;
            tracing::info!("applying config revision {rev}");
            config_tx.send_replace(rev);
        }
        for slot in &mut slots {
            let name = slot.service.name();
            if slot.handle.as_ref().is_some_and(Handle::is_finished) {
                let handle = slot.handle.take().expect("checked");
                let ran = handle.started.elapsed();
                let why = match handle.join() {
                    Err(e) => e,
                    Ok(()) => "it stopped by itself".into(),
                };
                if app.shutdown.is_stopping() {
                    continue;
                }
                slot.failures = if ran >= schedule::HEALTHY_AFTER {
                    1
                } else {
                    slot.failures + 1
                };
                let delay = schedule::restart_delay(slot.failures);
                tracing::error!(
                    "{name} stopped unexpectedly ({why}); restarting it in {}",
                    crate::config::types::format_duration(delay)
                );
                failed(&state, slot.service, &why, now)?;
                slot.restart_at = Some(Instant::now() + delay);
            }
            if slot.handle.is_none() && slot.restart_at.is_some_and(|at| at <= Instant::now()) {
                slot.restart_at = None;
                if store::service(&state.conn, name)?.paused {
                    store::set_status(&state.conn, name, "paused", None, now)?;
                    tracing::info!("{name} is paused");
                    continue;
                }
                let handle = service::spawn(
                    slot.service,
                    cli.clone(),
                    app.dirs.clone(),
                    app.shutdown.child(),
                    signals.clone(),
                )?;
                store::started(&state.conn, name, now)?;
                slot.handle = Some(handle);
            }
            // Running well again for a while.
            if slot.failures > 0
                && slot
                    .handle
                    .as_ref()
                    .is_some_and(|h| h.started.elapsed() >= schedule::HEALTHY_AFTER)
            {
                slot.failures = 0;
                recovered = true;
            }
        }
        // Every service is well again: the problem is over.
        if recovered && slots.iter().all(|s| s.failures == 0) {
            recovered = false;
            crate::problems::resolve(&state.conn, Kind::ServiceFailed, now)?;
        }
    }

    // Stopping: each service winds down what it's doing (downloads and hooks
    // get `shutdown_timeout`, then they're killed).
    let grace = config.general.shutdown_timeout.0 + Duration::from_secs(15);
    for slot in &mut slots {
        if let Some(handle) = slot.handle.take() {
            let name = slot.service.name();
            match tokio::time::timeout(grace, tokio::task::spawn_blocking(|| handle.join())).await {
                Ok(Ok(Err(e))) => tracing::warn!("{name}: {e}"),
                Ok(_) => {}
                Err(_) => tracing::warn!("{name} didn't stop in time"),
            }
        }
        store::set_status(
            &state.conn,
            slot.service.name(),
            "stopped",
            None,
            jiff::Timestamp::now(),
        )?;
    }
    tracing::info!("tidepool daemon stopped");
    Ok(exit::OK)
}

/// Take the library lock, waiting while a `tidepool run` from cron (or a
/// terminal) finishes. `None` when stopped while waiting.
async fn wait_for_library(
    library: &Arc<lock::Shared>,
    app: &App<'_>,
) -> anyhow::Result<Option<lock::SharedGuard>> {
    let mut told = false;
    loop {
        match library.acquire() {
            Ok(hold) => return Ok(Some(hold)),
            Err(LockError::Held { pid }) => {
                if !told {
                    tracing::info!(
                        "waiting for another tidepool{} to finish",
                        pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
                    );
                    told = true;
                }
            }
            Err(e) => return Err(e.into()),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = app.shutdown.stopping() => return Ok(None),
        }
    }
}

/// A service stopped unexpectedly: record it, and put back what it was in
/// the middle of.
fn failed(state: &State, service: Service, why: &str, now: jiff::Timestamp) -> anyhow::Result<()> {
    store::failed(&state.conn, service.name(), why, now)?;
    let (from, to) = match service {
        Service::Syncer => (status::DOWNLOADING, status::PENDING),
        Service::Processor => (status::PROCESSING, status::FETCHED),
    };
    items::reset_status(&state.conn, from, to)?;
    crate::problems::record(
        &state.conn,
        Kind::ServiceFailed,
        1,
        &format!("{}: {why}", service.name()),
        None,
        &std::collections::BTreeSet::new(),
        &Kind::ServiceFailed.hint(None, false),
        now,
    )?;
    Ok(())
}
