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
    /// The web UI (when `server.enabled`).
    Server,
}

impl Service {
    pub const ALL: [Service; 3] = [Service::Syncer, Service::Processor, Service::Server];

    pub fn name(self) -> &'static str {
        match self {
            Service::Syncer => "syncer",
            Service::Processor => "processor",
            Service::Server => "server",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    fn role(self) -> Role {
        match self {
            Service::Syncer => Role::Syncer,
            Service::Processor => Role::Processor,
            Service::Server => Role::Server,
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
    /// Clean up now (the syncer does it).
    pub cleanup_now: Arc<Notify>,
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
                Service::Server => tracing::info_span!("server"),
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
                            Service::Server => server(&app, signals).await,
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
            _ = signals.cleanup_now.notified() => {
                let now = jiff::Timestamp::now();
                let state = app.open_state()?;
                store::set_status(&state.conn, name, "running", Some("cleaning up"), now)?;
                let scope = crate::cli::ScopeArgs { sources: Vec::new() };
                if let Err(e) = app.cleanup(&scope, &mut out).await {
                    tracing::error!("cleanup failed: {e:#}");
                }
                crate::events::emit(crate::events::Event::Changed);
                out.flush()?;
                continue;
            }
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
        crate::events::emit(crate::events::Event::SyncStarted);
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
        let summary = match result {
            Ok(Some(summary)) => format!("{summary}{when}"),
            Ok(None) => format!("sync skipped{when}"),
            // One sync failing doesn't stop the schedule.
            Err(e) => format!("sync failed: {e:#}{when}"),
        };
        if summary.starts_with("sync failed") {
            tracing::error!("{summary}");
        } else {
            tracing::info!("{summary}");
        }
        crate::events::emit(crate::events::Event::SyncFinished { summary });
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
        run_jobs(app, &state).await?;
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

/// Do the work the web UI asked for (run hooks, embed, download again,
/// restore), one video at a time, keeping what each printed as its result.
async fn run_jobs(app: &App<'_>, state: &crate::state::State) -> anyhow::Result<()> {
    use crate::cli::{EmbedArgs, HooksCmd, ResetArgs, SourceRef, TrashCmd, VideoSelection};

    for job in store::waiting_jobs(&state.conn)? {
        if app.shutdown.is_stopping() {
            break;
        }
        store::start_job(&state.conn, job.id, jiff::Timestamp::now())?;
        tracing::info!("{}: {} for {}", job.source_key, job.action, job.video_id);
        let select = || VideoSelection {
            video_ids: vec![job.video_id.clone()],
            all: false,
            sources: vec![SourceRef::Key(job.source_key.clone())],
            categories: Vec::new(),
        };
        let mut out: Vec<u8> = Vec::new();
        let result = match job.action.as_str() {
            "hooks" => {
                let cmd = HooksCmd::Run {
                    select: select(),
                    hooks: Vec::new(),
                    failed: false,
                };
                app.hooks(&cmd, &mut out).await
            }
            "embed" => {
                let args = EmbedArgs {
                    select: select(),
                    remove_disabled: false,
                };
                app.embed(&args, &mut out).await
            }
            "reset" | "reset_delete" => {
                let args = ResetArgs {
                    video_id: job.video_id.clone(),
                    delete_files: job.action == "reset_delete",
                };
                app.reset(&args, &mut out)
            }
            "restore" => {
                let cmd = TrashCmd::Restore {
                    video_ids: vec![job.video_id.clone()],
                    sources: vec![SourceRef::Key(job.source_key.clone())],
                };
                app.trash(&cmd, &mut out)
            }
            other => Err(anyhow::anyhow!("unknown job {other:?}")),
        };
        let printed = String::from_utf8_lossy(&out).trim().to_string();
        let (ok, text) = match result {
            Ok(0) => (true, printed),
            Ok(_) => (false, printed),
            Err(e) if printed.is_empty() => (false, format!("{e:#}")),
            Err(e) => (false, format!("{printed}\n{e:#}")),
        };
        if ok {
            tracing::info!(
                "{}: {} for {}: {text}",
                job.source_key,
                job.action,
                job.video_id
            );
        } else {
            tracing::warn!(
                "{}: {} for {} failed: {text}",
                job.source_key,
                job.action,
                job.video_id
            );
        }
        store::finish_job(&state.conn, job.id, ok, &text, jiff::Timestamp::now())?;
        crate::events::emit(crate::events::Event::Changed);
    }
    Ok(())
}

/// Serve the web UI until stopped (a new `[server]` config restarts it).
async fn server(app: &App<'_>, signals: Signals) -> anyhow::Result<()> {
    let state = app.open_state()?;
    let config = app.current_config(&state)?.config;
    let listener = tokio::net::TcpListener::bind(&config.server.listen)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {}: {e}", config.server.listen))?;
    let addr = listener.local_addr()?;
    let web = Arc::new(crate::web::Web::new(
        app.paths.state_db.clone(),
        app.dirs.data_dir.clone(),
        app.paths.hook_logs.clone(),
        &config.server,
        addr,
        crate::web::Wake {
            sync_now: signals.sync_now.clone(),
            cleanup_now: signals.cleanup_now.clone(),
            processor: signals.fetched.clone(),
            shutdown: app.shutdown.clone(),
        },
    ));
    let url = format!("http://{addr}");
    tracing::info!("web UI at {url}");
    store::set_status(
        &state.conn,
        Service::Server.name(),
        "running",
        Some(&format!("listening on {url}")),
        jiff::Timestamp::now(),
    )?;
    drop(state);
    crate::web::serve(web, listener, app.shutdown.clone()).await?;
    Ok(())
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
