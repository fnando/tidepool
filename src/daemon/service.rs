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
    /// Indexes and cleans up on the schedule.
    Syncer,
    /// Downloads what indexing queued (the processor finishes it).
    Downloader,
    /// Finishes downloads (hooks, embedding, saving) and runs queued hooks.
    Processor,
    /// The web UI (when `server.enabled`).
    Server,
}

impl Service {
    pub const ALL: [Service; 4] = [
        Service::Syncer,
        Service::Downloader,
        Service::Processor,
        Service::Server,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Service::Syncer => "syncer",
            Service::Downloader => "downloader",
            Service::Processor => "processor",
            Service::Server => "server",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    fn role(self) -> Role {
        match self {
            Service::Syncer => Role::Syncer,
            Service::Downloader => Role::Downloader,
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
    /// Sync now (everything, or some sources) instead of waiting for the
    /// schedule.
    pub sync_now: Arc<SyncNow>,
    /// Download now (the syncer tells the downloader after a sync).
    pub download_now: Arc<DownloadNow>,
    /// Clean up now (the syncer does it).
    pub cleanup_now: Arc<Notify>,
    /// The current config revision; changes when a new one is stored.
    pub config: watch::Receiver<i64>,
}

/// What a sync does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncMode {
    /// Look for new videos only.
    Index,
    /// Look for new videos, download them (where `download` is on), clean up.
    Sync,
    /// Like `Sync`, downloading even where `download` is off: a source's own
    /// Sync now button.
    Download,
}

/// What's been asked of the syncer since it last looked.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Asked {
    /// Sync every source (as the schedule does).
    pub everything: bool,
    /// Sources asked for by name; downloading wins over syncing, syncing
    /// over indexing.
    pub sources: std::collections::BTreeMap<String, SyncMode>,
}

impl Asked {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.everything && self.sources.is_empty()
    }

    /// The sources asked for with `mode`.
    #[must_use]
    pub fn keys(&self, mode: SyncMode) -> Vec<String> {
        self.sources
            .iter()
            .filter(|(_, m)| **m == mode)
            .map(|(key, _)| key.clone())
            .collect()
    }
}

/// Where a source is in the syncer's work, for the web UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// Asked for; the syncer gets to it next.
    Waiting,
    /// Being synced or indexed.
    Running,
}

/// Requests to sync before the schedule says so (everything, or just some
/// sources), and which sources the syncer is working on.
#[derive(Default)]
pub struct SyncNow {
    notify: Notify,
    inner: std::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    asked: Asked,
    /// Sources taken from `asked` or being synced.
    active: std::collections::BTreeMap<String, Busy>,
}

impl SyncNow {
    /// Sync every source now.
    pub fn all(&self) {
        self.lock().asked.everything = true;
        self.notify.notify_one();
    }

    /// Sync, download or index this source now.
    pub fn source(&self, key: &str, mode: SyncMode) {
        let mut inner = self.lock();
        let entry = inner.asked.sources.entry(key.to_string()).or_insert(mode);
        *entry = (*entry).max(mode);
        drop(inner);
        self.notify.notify_one();
    }

    /// Wait until something is asked.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// What's been asked, clearing it; the sources named wait their turn.
    pub fn take(&self) -> Asked {
        let mut inner = self.lock();
        let asked = std::mem::take(&mut inner.asked);
        // Nothing runs between syncs: anything left over is stale.
        inner.active.clear();
        for key in asked.sources.keys() {
            inner.active.insert(key.clone(), Busy::Waiting);
        }
        asked
    }

    /// Whether the syncer is on `key` or about to be.
    pub fn busy(&self, key: &str) -> Option<Busy> {
        let inner = self.lock();
        inner.active.get(key).copied().or_else(|| {
            (inner.asked.everything || inner.asked.sources.contains_key(key))
                .then_some(Busy::Waiting)
        })
    }

    /// The syncer works on `keys` until the guard is dropped.
    fn working(&self, keys: &[String]) -> Working<'_> {
        let mut inner = self.lock();
        for key in keys {
            inner.active.insert(key.clone(), Busy::Running);
        }
        Working {
            sync_now: self,
            keys: keys.to_vec(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Sources the syncer is on; done with them when dropped, however the sync
/// ended.
struct Working<'a> {
    sync_now: &'a SyncNow,
    keys: Vec<String>,
}

impl Drop for Working<'_> {
    fn drop(&mut self) {
        let mut inner = self.sync_now.lock();
        for key in &self.keys {
            inner.active.remove(key);
        }
    }
}

/// Wakes the downloader, with the sources it should download even where
/// `download` is off (a source's own Sync now button).
#[derive(Default)]
pub struct DownloadNow {
    notify: Notify,
    forced: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl DownloadNow {
    /// Download what's queued now.
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Download `keys` now, even where `download` is off.
    pub fn force(&self, keys: &[String]) {
        self.lock().extend(keys.iter().cloned());
        self.notify.notify_one();
    }

    /// Wait until woken.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// `keys` (taken) weren't downloaded: they wait for the next try.
    fn put_back(&self, keys: &[String]) {
        self.lock().extend(keys.iter().cloned());
    }

    /// The sources to download even where `download` is off, clearing them.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock()).into_iter().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<String>> {
        self.forced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A running service.
pub struct Handle {
    pub service: Service,
    pub shutdown: Shutdown,
    pub started: Instant,
    thread: std::thread::JoinHandle<Result<(), String>>,
}

impl Handle {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Wait for it to end: `Err` says why it ended on its own (an error, or
    /// a panic).
    ///
    /// # Errors
    ///
    /// Returns the service's own error, or a "crashed" message if its thread
    /// panicked.
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
///
/// # Errors
///
/// Fails if the OS can't spawn the thread.
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
                Service::Downloader => tracing::info_span!("downloader"),
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
                            Service::Downloader => downloader(&app, signals).await,
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
#[must_use]
pub fn local(t: jiff::Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// Sync each source when it's due (or when asked), until stopped. Every
/// minute at least, it looks for sources due: new ones, and those whose next
/// sync has passed.
async fn syncer(app: &App<'_>, mut signals: Signals) -> anyhow::Result<()> {
    let name = Service::Syncer.name();
    let mut out = LogWriter::default();
    loop {
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        let state = app.open_state()?;
        let config = app.current_config(&state)?.config;
        let now = jiff::Timestamp::now();
        let every = config.schedule.interval.0;
        // A new interval (even one set while the daemon was stopped) counts
        // from each source's last sync.
        let interval =
            every.map_or_else(|| "none".to_string(), crate::config::types::format_duration);
        let before = store::get(&state.conn, store::SCHEDULE_INTERVAL)?;
        if before.as_deref() != Some(&interval) {
            if before.is_some() {
                store::reschedule(&state.conn, |last| {
                    every.map(|every| schedule::next_sync(last, every, schedule::jitter()))
                })?;
            }
            store::set(&state.conn, store::SCHEDULE_INTERVAL, Some(&interval))?;
        }
        let enabled: Vec<String> = crate::config::merge::effective_sources(&config)
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.key)
            .collect();
        let nexts = store::next_syncs(&state.conn)?;
        let due = if every.is_some() {
            schedule::due(&enabled, &nexts, now)
        } else {
            Vec::new()
        };
        let mut wait = schedule::CHECK_EVERY;
        if due.is_empty() {
            let activity = match (every, schedule::soonest(&enabled, &nexts, now)) {
                (None, _) => "no schedule".to_string(),
                (Some(_), None) => "no enabled sources".to_string(),
                (Some(_), Some(next)) => {
                    wait = wait.min(schedule::wait_until(next, now));
                    format!("next sync at {}", local(next))
                }
            };
            store::set_status(&state.conn, name, "idle", Some(&activity), now)?;
        }
        drop(state);
        let asked = if due.is_empty() {
            tokio::select! {
                // Look again: something may be due now.
                () = tokio::time::sleep(wait) => continue,
                () = signals.sync_now.notified() => signals.sync_now.take(),
                () = signals.cleanup_now.notified() => {
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
                // New sources, or a new interval: look again.
                changed = signals.config.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    continue;
                }
                () = app.shutdown.stopping() => return Ok(()),
            }
        } else {
            Asked {
                sources: due.into_iter().map(|key| (key, SyncMode::Sync)).collect(),
                ..Asked::default()
            }
        };
        let batch = Batch {
            every,
            out: &mut out,
            sync_now: &signals.sync_now,
            download_now: &signals.download_now,
        };
        if asked.everything {
            // Sources asked for by name are synced along with the rest.
            batch.run(app, &enabled, None, SyncMode::Sync).await?;
            continue;
        }
        let mut batch = batch;
        for mode in [SyncMode::Download, SyncMode::Sync] {
            let synced = asked.keys(mode);
            batch = batch.run(app, &synced, Some(&synced), mode).await?;
        }
        let indexed = asked.keys(SyncMode::Index);
        batch
            .run(app, &indexed, Some(&indexed), SyncMode::Index)
            .await?;
    }
}

/// Syncs the syncer runs, and what they need.
struct Batch<'o> {
    /// `schedule.interval`.
    every: Option<Duration>,
    out: &'o mut LogWriter,
    /// Told which sources are being synced.
    sync_now: &'o SyncNow,
    /// Told when a sync may have queued downloads.
    download_now: &'o DownloadNow,
}

impl Batch<'_> {
    /// Sync or index `keys`: `only` those (else every enabled source, which
    /// `keys` lists). A sync schedules each source's next one and has the
    /// downloader download what's new; a sync that couldn't run (another
    /// tidepool has the library) is tried again in a minute. Indexing keeps
    /// the schedule.
    async fn run(
        self,
        app: &App<'_>,
        keys: &[String],
        only: Option<&[String]>,
        mode: SyncMode,
    ) -> anyhow::Result<Self> {
        if keys.is_empty() || app.shutdown.is_stopping() {
            return Ok(self);
        }
        let sync_now = self.sync_now;
        let _working = sync_now.working(keys);
        let state = app.open_state()?;
        let started = jiff::Timestamp::now();
        let doing = match mode {
            SyncMode::Sync | SyncMode::Download => "syncing",
            SyncMode::Index => "indexing",
        };
        let what = only.map_or_default(|keys| format!("{} ", keys.join(", ")));
        let activity = format!("{doing} {what}since {}", local(started));
        store::set_status(
            &state.conn,
            Service::Syncer.name(),
            "running",
            Some(&activity),
            started,
        )?;
        let syncs = mode != SyncMode::Index;
        if syncs {
            store::set_time(&state.conn, store::LAST_SYNC_STARTED_AT, started)?;
        }
        crate::events::emit(crate::events::Event::SyncStarted);
        let result = app
            .sync_once(only.unwrap_or_default(), mode, self.out)
            .await;
        self.out.flush()?;
        if app.shutdown.is_stopping() {
            return Ok(self);
        }
        if matches!(result, Ok(Some(_))) {
            match mode {
                SyncMode::Download => self.download_now.force(keys),
                SyncMode::Sync => self.download_now.wake(),
                SyncMode::Index => {}
            }
        }
        let finished = jiff::Timestamp::now();
        let mut suffix = String::new();
        if syncs {
            if matches!(result, Ok(None)) {
                let retry = finished
                    .checked_add(schedule::CHECK_EVERY)
                    .unwrap_or(finished);
                for key in keys {
                    store::set_next_sync(&state.conn, key, retry)?;
                }
                suffix = format!("; trying again at {}", local(retry));
            } else {
                // One sync failing doesn't stop the schedule.
                store::set_time(&state.conn, store::LAST_SYNC_FINISHED_AT, finished)?;
                let mut soonest: Option<jiff::Timestamp> = None;
                for key in keys {
                    let next = self
                        .every
                        .map(|every| schedule::next_sync(finished, every, schedule::jitter()));
                    store::set_synced(&state.conn, key, finished, next)?;
                    soonest = match (soonest, next) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                }
                if let Some(next) = soonest {
                    suffix = format!("; next sync at {}", local(next));
                }
            }
        }
        let prefix = only.map(|keys| format!("{}: ", keys.join(", ")));
        report(result, prefix.as_deref().unwrap_or_default(), &suffix);
        Ok(self)
    }
}

/// Log how a sync went (`prefix`: the sources asked for, if any) and tell
/// the live view.
fn report(result: anyhow::Result<Option<String>>, prefix: &str, suffix: &str) {
    let summary = match result {
        Ok(Some(summary)) => format!("{summary}{suffix}"),
        Ok(None) => format!("sync skipped{suffix}"),
        Err(e) => format!("sync failed: {e:#}{suffix}"),
    };
    if summary.starts_with("sync failed") {
        tracing::error!("{prefix}{summary}");
    } else {
        tracing::info!("{prefix}{summary}");
    }
    crate::events::emit(crate::events::Event::SyncFinished { summary });
}

/// How long the downloader waits when nothing wakes it: failed downloads
/// whose retry is due get another try then.
const DOWNLOAD_EVERY: Duration = Duration::from_secs(3600);

/// Download what indexing queued, when woken after a sync (and hourly),
/// until stopped. The processor finishes each download.
async fn downloader(app: &App<'_>, mut signals: Signals) -> anyhow::Result<()> {
    let name = Service::Downloader.name();
    let mut out = LogWriter::default();
    loop {
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        let forced = signals.download_now.take();
        let state = app.open_state()?;
        let started = jiff::Timestamp::now();
        let activity = format!("downloading since {}", local(started));
        store::set_status(&state.conn, name, "running", Some(&activity), started)?;
        let wait = match app.download_once(&forced, &mut out).await {
            Ok(Some(summary)) => {
                if summary.fetched > 0 || summary.failed > 0 {
                    let mut parts = vec![format!("{} downloaded", summary.fetched)];
                    if summary.failed > 0 {
                        parts.push(format!("{} failed", summary.failed));
                    }
                    tracing::info!("{}", parts.join(", "));
                }
                DOWNLOAD_EVERY
            }
            // Another tidepool has the library: look again in a minute.
            Ok(None) => {
                signals.download_now.put_back(&forced);
                schedule::CHECK_EVERY
            }
            Err(e) => {
                tracing::error!("downloading failed: {e:#}");
                schedule::CHECK_EVERY
            }
        };
        out.flush()?;
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        store::set_status(&state.conn, name, "idle", None, jiff::Timestamp::now())?;
        drop(state);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = signals.download_now.notified() => {}
            changed = signals.config.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            () = app.shutdown.stopping() => return Ok(()),
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
            () = tokio::time::sleep(wait.min(Duration::from_secs(3600))) => {}
            () = signals.fetched.notified() => {}
            changed = signals.config.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            () = app.shutdown.stopping() => return Ok(()),
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
        app.dirs.clone(),
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

#[cfg(test)]
mod tests {
    use super::SyncNow;

    #[test]
    fn says_which_sources_are_being_synced() {
        use super::{Busy, SyncMode};

        let sync_now = SyncNow::default();
        assert_eq!(sync_now.busy("lofi"), None);
        sync_now.source("lofi", SyncMode::Download);
        assert_eq!(sync_now.busy("lofi"), Some(Busy::Waiting));
        assert_eq!(sync_now.busy("jazz"), None);
        let asked = sync_now.take();
        assert_eq!(sync_now.busy("lofi"), Some(Busy::Waiting), "its turn comes");
        {
            let _working = sync_now.working(&asked.keys(SyncMode::Download));
            assert_eq!(sync_now.busy("lofi"), Some(Busy::Running));
        }
        assert_eq!(sync_now.busy("lofi"), None, "done");

        sync_now.all();
        assert_eq!(sync_now.busy("jazz"), Some(Busy::Waiting));
    }

    #[tokio::test]
    async fn forced_downloads_add_up_until_taken() {
        let now = super::DownloadNow::default();
        assert!(now.take().is_empty());
        now.force(&["lofi".into()]);
        now.force(&["jazz".into(), "lofi".into()]);
        tokio::time::timeout(std::time::Duration::from_secs(1), now.notified())
            .await
            .expect("the downloader was woken");
        assert_eq!(now.take(), ["jazz", "lofi"]);
        assert!(now.take().is_empty(), "taking clears it");

        // Put back (another tidepool had the library): kept, without a wake.
        now.put_back(&["lofi".into()]);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), now.notified())
                .await
                .is_err()
        );
        assert_eq!(now.take(), ["lofi"]);
    }

    #[test]
    fn sync_requests_add_up() {
        use super::SyncMode::{Download, Index, Sync};

        let asked = SyncNow::default();
        assert!(asked.take().is_empty());
        asked.source("lofi", Index);
        asked.source("jazz", Sync);
        asked.source("lofi", Sync);
        asked.source("jazz", Index);
        asked.source("rock", Index);
        let taken = asked.take();
        assert!(!taken.everything);
        assert_eq!(taken.keys(Sync), ["jazz", "lofi"], "syncing wins");
        assert_eq!(taken.keys(Index), ["rock"]);
        assert!(asked.take().is_empty(), "taking clears it");

        // The source page's button downloads, whatever else was asked.
        asked.source("lofi", Download);
        asked.source("lofi", Sync);
        asked.source("lofi", Index);
        assert_eq!(asked.take().keys(Download), ["lofi"]);

        asked.source("lofi", Index);
        asked.all();
        assert!(asked.take().everything);
    }
}
