//! `tidepool run --daemon`: a supervisor on the main thread that starts the
//! services (`syncer`, `processor`), watches for new config revisions,
//! restarts a service that stops unexpectedly, and stops everything on
//! SIGINT/SIGTERM.

pub mod control;
pub mod schedule;
pub mod service;
mod startup;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::bail;
use tokio::sync::{Notify, watch};

use crate::app::{App, exit};
use crate::cli::Cli;
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
    /// When to start it (again).
    restart_at: Option<Instant>,
    /// Asked to stop, and why.
    stopping: Option<Stopping>,
    /// Whether the config wants it (`server.enabled` for the server).
    enabled: bool,
}

/// Why the supervisor stopped a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopping {
    Pause,
    Restart,
}

impl Slot {
    /// Every service, due to start now (the server only if `server_enabled`).
    fn all(server_enabled: bool) -> Vec<Self> {
        Service::ALL
            .into_iter()
            .map(|service| Slot {
                service,
                handle: None,
                failures: 0,
                restart_at: Some(Instant::now()),
                stopping: None,
                enabled: service != Service::Server || server_enabled,
            })
            .collect()
    }

    /// The web UI's settings changed: switch it, and restart it if it's on.
    fn server_changed(slots: &mut [Self], enabled: bool) {
        for slot in slots.iter_mut().filter(|s| s.service == Service::Server) {
            slot.enabled = enabled;
            if enabled {
                slot.stop(Stopping::Restart);
            }
        }
    }

    /// Ask it to stop what it's doing and end (`why` decides what's next).
    fn stop(&mut self, why: Stopping) {
        if let Some(handle) = &self.handle
            && self.stopping.is_none()
        {
            handle.shutdown.stop(143);
            self.stopping = Some(why);
        }
    }
}

pub(crate) async fn run(app: &App<'_>) -> anyhow::Result<i32> {
    let _daemon = match lock::acquire(&app.paths.scheduler_lock) {
        Ok(held) => held,
        Err(LockError::Held { pid }) => bail!(
            "another tidepool daemon is running{}",
            pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
        ),
        Err(e) => return Err(e.into()),
    };
    crate::events::enable();
    let state = app.open_state()?;
    let mismatch = match &app.cli.command {
        crate::cli::Command::Run(args) => args.on_config_mismatch,
        _ => None,
    };
    startup::check(app, &state, mismatch)?;
    let config = app.current_config(&state)?.config;
    let interval = config.schedule.interval.0;
    if interval.is_none() && !config.server.enabled {
        bail!(
            "schedule.interval is empty and the web UI is off, so the daemon has nothing to \
             do; set the interval (e.g. interval = \"6h\" under [schedule]) and import the config"
        );
    }
    let mut server = config.server.clone();

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
        .map_or(0, |r| r.rev);
    let (config_tx, config_rx) = watch::channel(revision);
    let signals = Signals {
        library,
        fetched: Arc::new(Notify::new()),
        sync_now: Arc::default(),
        cleanup_now: Arc::new(Notify::new()),
        config: config_rx,
    };
    let cli = Arc::new(app.cli.clone());
    let mut slots = Slot::all(server.enabled);
    ignore_old_requests(&state)?;
    tracing::info!(
        "tidepool daemon started (pid {}): {}",
        std::process::id(),
        describe_schedule(interval)
    );

    let mut poll = tokio::time::interval(POLL);
    let mut recovered = false;
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            () = app.shutdown.stopping() => break,
        }
        let now = jiff::Timestamp::now();
        if let Some(rev) = ConfigStore::new(&state.conn).current()?.map(|r| r.rev)
            && rev != revision
        {
            revision = rev;
            tracing::info!("applying config revision {rev}");
            crate::events::emit(crate::events::Event::Changed);
            config_tx.send_replace(rev);
            // A new address, token or switch: the web UI starts over.
            let now_server = app.current_config(&state)?.config.server;
            if now_server != server {
                server = now_server;
                Slot::server_changed(&mut slots, server.enabled);
            }
        }
        for request in store::waiting_requests(&state.conn)? {
            let answer = handle_request(&request, &mut slots, &signals, &state)?;
            tracing::info!("{answer}");
            store::answer(&state.conn, request.id, &answer, now)?;
        }
        for slot in &mut slots {
            recovered |= tend(app, &state, slot, &cli, &signals, now)?;
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
    stop_all(&mut slots, grace, &state).await?;
    tracing::info!("tidepool daemon stopped");
    Ok(exit::OK)
}

/// One supervisor round for a service: pause or resume it, deal with it
/// having stopped, and start it when it's due. Returns whether it just
/// counted as healthy again after failing.
fn tend(
    app: &App<'_>,
    state: &State,
    slot: &mut Slot,
    cli: &Arc<Cli>,
    signals: &Signals,
    now: jiff::Timestamp,
) -> anyhow::Result<bool> {
    let name = slot.service.name();
    // `tidepool daemon pause|resume`, or switched off in the config.
    let paused = store::service(&state.conn, name)?.paused || !slot.enabled;
    if paused {
        slot.restart_at = None;
        if slot.handle.is_some() && slot.stopping.is_none() {
            tracing::info!("pausing {name}");
            slot.stop(Stopping::Pause);
        }
    } else if slot.handle.is_none() && slot.restart_at.is_none() {
        tracing::info!("resuming {name}");
        slot.restart_at = Some(Instant::now());
    }
    if slot.handle.as_ref().is_some_and(Handle::is_finished) {
        let handle = slot.handle.take().expect("checked");
        let ran = handle.started.elapsed();
        let result = handle.join();
        match slot.stopping.take() {
            Some(Stopping::Pause) => {
                let status = if slot.enabled { "paused" } else { "off" };
                store::set_status(&state.conn, name, status, None, now)?;
                tracing::info!("{name} is {status}");
                crate::events::emit(crate::events::Event::Changed);
                return Ok(false);
            }
            Some(Stopping::Restart) => {
                slot.restart_at = Some(Instant::now());
            }
            None if app.shutdown.is_stopping() => return Ok(false),
            None => {
                let why = match result {
                    Err(e) => e,
                    Ok(()) => "it stopped by itself".into(),
                };
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
                failed(state, slot.service, &why, now)?;
                crate::events::emit(crate::events::Event::Changed);
                slot.restart_at = Some(Instant::now() + delay);
            }
        }
    }
    if paused && slot.handle.is_none() && slot.stopping.is_none() {
        let status = if slot.enabled { "paused" } else { "off" };
        store::set_status(&state.conn, name, status, None, now)?;
    }
    if slot.handle.is_none() && slot.restart_at.is_some_and(|at| at <= Instant::now()) {
        slot.restart_at = None;
        // Before the thread starts: what it says it's doing wins.
        store::started(&state.conn, name, now)?;
        let handle = service::spawn(
            slot.service,
            cli.clone(),
            app.dirs.clone(),
            app.shutdown.child(),
            signals.clone(),
        )?;
        slot.handle = Some(handle);
        crate::events::emit(crate::events::Event::Changed);
    }
    // Running well again for a while.
    if slot.failures > 0
        && slot
            .handle
            .as_ref()
            .is_some_and(|h| h.started.elapsed() >= schedule::HEALTHY_AFTER)
    {
        slot.failures = 0;
        return Ok(true);
    }
    Ok(false)
}

/// Stop every service, giving each `grace` to wind down.
async fn stop_all(slots: &mut [Slot], grace: Duration, state: &State) -> anyhow::Result<()> {
    for slot in slots {
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
    Ok(())
}

/// How often it syncs, for the startup message.
fn describe_schedule(interval: Option<Duration>) -> String {
    match interval {
        Some(every) => format!(
            "syncing every {}",
            crate::config::types::format_duration(every)
        ),
        None => "no schedule; syncing only when asked".into(),
    }
}

/// Answer the requests made while no daemon was running.
fn ignore_old_requests(state: &State) -> anyhow::Result<()> {
    for request in store::waiting_requests(&state.conn)? {
        store::answer(
            &state.conn,
            request.id,
            "ignored: the daemon wasn't running",
            jiff::Timestamp::now(),
        )?;
    }
    Ok(())
}

/// Do what `tidepool daemon restart|sync` asked; returns the answer.
fn handle_request(
    request: &store::Request,
    slots: &mut [Slot],
    signals: &Signals,
    state: &State,
) -> anyhow::Result<String> {
    let targets: Vec<Service> = match request.service.as_deref() {
        None => Service::ALL.to_vec(),
        Some(name) => match Service::parse(name) {
            Some(service) => vec![service],
            None => return Ok(format!("there's no service {name:?}")),
        },
    };
    match request.action.as_str() {
        "sync" => {
            if store::service(&state.conn, Service::Syncer.name())?.paused {
                return Ok("the syncer is paused; resume it first".into());
            }
            signals.sync_now.all();
            Ok("syncing now".into())
        }
        "restart" => {
            let mut restarted = Vec::new();
            let mut paused = Vec::new();
            for slot in slots.iter_mut().filter(|s| targets.contains(&s.service)) {
                let name = slot.service.name();
                if store::service(&state.conn, name)?.paused {
                    paused.push(name);
                } else if slot.handle.is_some() {
                    slot.stop(Stopping::Restart);
                    restarted.push(name);
                } else {
                    slot.restart_at = Some(Instant::now());
                    restarted.push(name);
                }
            }
            let mut parts = Vec::new();
            if !restarted.is_empty() {
                parts.push(format!("restarting {}", restarted.join(" and ")));
            }
            if !paused.is_empty() {
                parts.push(format!(
                    "{} {} paused (resume it first)",
                    paused.join(" and "),
                    if paused.len() == 1 { "is" } else { "are" }
                ));
            }
            Ok(parts.join("; "))
        }
        other => Ok(format!("unknown request {other:?}")),
    }
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
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            () = app.shutdown.stopping() => return Ok(None),
        }
    }
}

/// A service stopped unexpectedly: record it, and put back what it was in
/// the middle of.
fn failed(state: &State, service: Service, why: &str, now: jiff::Timestamp) -> anyhow::Result<()> {
    store::failed(&state.conn, service.name(), why, now)?;
    let interrupted = match service {
        Service::Syncer => Some((status::DOWNLOADING, status::PENDING)),
        Service::Processor => Some((status::PROCESSING, status::FETCHED)),
        Service::Server => None,
    };
    if let Some((from, to)) = interrupted {
        items::reset_status(&state.conn, from, to)?;
    }
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
