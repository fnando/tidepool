//! The daemon's services. Each runs on its own thread, with its own tokio
//! runtime and its own database connection, so one can stop (or crash) and
//! start again without the others noticing.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use tracing::Instrument;

use super::schedule;
use crate::app::{App, Role};
use crate::cli::Cli;
use crate::paths::Dirs;
use crate::shutdown::Shutdown;
use crate::state::daemon as store;

/// The parts of the daemon that can be paused and restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// Indexes, downloads and cleans up on the schedule.
    Syncer,
    /// Finishes downloads (hooks, embedding, saving) and runs queued hooks.
    Processor,
}

impl Service {
    pub const ALL: [Service; 2] = [Service::Syncer, Service::Processor];

    pub fn name(self) -> &'static str {
        match self {
            Service::Syncer => "syncer",
            Service::Processor => "processor",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    fn role(self) -> Role {
        match self {
            Service::Syncer => Role::Syncer,
            Service::Processor => Role::Processor,
        }
    }
}

/// What the supervisor shares with every service.
#[derive(Clone)]
pub struct Signals {
    pub library: Arc<crate::lock::Shared>,
    /// A download is waiting to be processed.
    pub fetched: Arc<Notify>,
    /// Sync now instead of waiting for the schedule.
    pub sync_now: Arc<Notify>,
    /// The current config revision; changes when a new one is stored.
    pub config: watch::Receiver<i64>,
}

/// A running service.
pub struct Handle {
    pub service: Service,
    pub shutdown: Shutdown,
    pub started: Instant,
    thread: std::thread::JoinHandle<Result<(), String>>,
}

impl Handle {
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Wait for it to end: `Err` says why it ended on its own (an error, or
    /// a panic).
    pub fn join(self) -> Result<(), String> {
        match self.thread.join() {
            Ok(result) => result,
            Err(panic) => Err(match panic.downcast::<String>() {
                Ok(message) => format!("crashed: {message}"),
                Err(panic) => match panic.downcast::<&str>() {
                    Ok(message) => format!("crashed: {message}"),
                    Err(_) => "crashed".into(),
                },
            }),
        }
    }
}

/// Start `service` on its own thread.
pub fn spawn(
    service: Service,
    cli: Arc<Cli>,
    dirs: Dirs,
    shutdown: Shutdown,
    signals: Signals,
) -> std::io::Result<Handle> {
    let stop = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name(format!("tidepool-{}", service.name()))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("cannot start: {e}"))?;
            let span = match service {
                Service::Syncer => tracing::info_span!("syncer"),
                Service::Processor => tracing::info_span!("processor"),
            };
            runtime
                .block_on(
                    async {
                        let app = App::for_service(
                            &cli,
                            dirs,
                            stop,
                            service.role(),
                            signals.library.clone(),
                            signals.fetched.clone(),
                        );
                        test_panic(service);
                        match service {
                            Service::Syncer => syncer(&app, signals).await,
                            Service::Processor => processor(&app, signals).await,
                        }
                    }
                    .instrument(span),
                )
                .map_err(|e| format!("{e:#}"))
        })?;
    Ok(Handle {
        service,
        shutdown,
        started: Instant::now(),
        thread,
    })
}

/// Debug builds: `TIDEPOOL_TEST_PANIC=<service>` crashes it once, to test
/// restarts.
fn test_panic(service: Service) {
    #[cfg(debug_assertions)]
    {
        static PANICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var("TIDEPOOL_TEST_PANIC").ok().as_deref() == Some(service.name())
            && !PANICKED.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("TIDEPOOL_TEST_PANIC");
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = service;
}

/// A time as the log shows it: local, to the minute.
pub fn local(t: jiff::Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// Sync on the schedule (or when asked), until stopped.
async fn syncer(app: &App<'_>, mut signals: Signals) -> anyhow::Result<()> {
    let name = Service::Syncer.name();
    let mut out = LogWriter::default();
    let mut interval = None;
    loop {
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        let state = app.open_state()?;
        let config = app.current_config(&state)?.config;
        let now = jiff::Timestamp::now();
        let changed = interval != config.schedule.interval.0;
        interval = config.schedule.interval.0;
        // A new interval counts from the last sync's end.
        if changed
            && let (Some(every), Some(last)) = (
                interval,
                store::get_time(&state.conn, store::LAST_SYNC_FINISHED_AT)?,
            )
        {
            let next = schedule::next_sync(last, every, schedule::jitter());
            store::set_time(&state.conn, store::NEXT_SYNC_AT, next)?;
        }
        let next = store::get_time(&state.conn, store::NEXT_SYNC_AT)?.unwrap_or(now);
        let wait = match interval {
            Some(_) => {
                let activity = format!("next sync at {}", local(next));
                store::set_status(&state.conn, name, "idle", Some(&activity), now)?;
                schedule::wait_until(next, now)
            }
            None => {
                store::set_status(&state.conn, name, "idle", Some("no schedule"), now)?;
                Duration::MAX
            }
        };
        drop(state);
        tokio::select! {
            _ = tokio::time::sleep(wait.min(Duration::from_secs(86400 * 365))) => {}
            _ = signals.sync_now.notified() => {}
            changed = signals.config.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                continue;
            }
            _ = app.shutdown.stopping() => return Ok(()),
        }

        let state = app.open_state()?;
        let started = jiff::Timestamp::now();
        store::set_time(&state.conn, store::LAST_SYNC_STARTED_AT, started)?;
        let activity = format!("syncing since {}", local(started));
        store::set_status(&state.conn, name, "running", Some(&activity), started)?;
        let result = app.sync_once(&mut out).await;
        out.flush()?;
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        let finished = jiff::Timestamp::now();
        store::set_time(&state.conn, store::LAST_SYNC_FINISHED_AT, finished)?;
        let next = interval.map(|every| schedule::next_sync(finished, every, schedule::jitter()));
        store::set(
            &state.conn,
            store::NEXT_SYNC_AT,
            next.map(|t| t.to_string()).as_deref(),
        )?;
        let when = next
            .map(|t| format!("; next sync at {}", local(t)))
            .unwrap_or_default();
        match result {
            Ok(Some(summary)) => tracing::info!("{summary}{when}"),
            Ok(None) => tracing::info!("sync skipped{when}"),
            // One sync failing doesn't stop the schedule.
            Err(e) => tracing::error!("sync failed: {e:#}{when}"),
        }
    }
}

/// Finish downloads as they come in, and run queued hooks when they're
/// due, until stopped.
async fn processor(app: &App<'_>, mut signals: Signals) -> anyhow::Result<()> {
    let name = Service::Processor.name();
    let mut out = LogWriter::default();
    loop {
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        let state = app.open_state()?;
        store::set_status(
            &state.conn,
            name,
            "running",
            Some("processing"),
            jiff::Timestamp::now(),
        )?;
        let wait = match app.process_once(&mut out).await {
            Ok(Some(_)) => {
                let now = jiff::Timestamp::now();
                match crate::state::items::next_hook_due(&state.conn)? {
                    Some(due) => schedule::wait_until(due, now).max(Duration::from_secs(1)),
                    None => Duration::from_secs(3600),
                }
            }
            // Another tidepool has the library: look again in a minute.
            Ok(None) => Duration::from_secs(60),
            Err(e) => {
                tracing::error!("processing failed: {e:#}");
                Duration::from_secs(60)
            }
        };
        out.flush()?;
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        store::set_status(&state.conn, name, "idle", None, jiff::Timestamp::now())?;
        drop(state);
        tokio::select! {
            _ = tokio::time::sleep(wait.min(Duration::from_secs(3600))) => {}
            _ = signals.fetched.notified() => {}
            changed = signals.config.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            _ = app.shutdown.stopping() => return Ok(()),
        }
    }
}

/// What commands print, as log lines (the daemon has no terminal).
#[derive(Default)]
pub struct LogWriter {
    pending: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]);
            if !line.trim().is_empty() {
                tracing::info!("{line}");
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&self.pending).into_owned();
            self.pending.clear();
            if !line.trim().is_empty() {
                tracing::info!("{line}");
            }
        }
        Ok(())
    }
}
