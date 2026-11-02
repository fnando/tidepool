//! Command dispatch: turns a parsed [`Cli`] into work and an exit code.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::Value;

use crate::cli::{
    Cli, Command, ConfigArgs, ConfigCmd, DownloadArgs, FilterCmd, HooksCmd, InitArgs, ResetArgs,
    RunArgs, ScopeArgs, SourceRef, ToolsCmd,
};
use crate::config::check::{EffectiveConfig, Finding, check_environment, media_dir, tmp_dir};
use crate::config::diff::{key_changes, mask_json, mask_toml, text_diff};
use crate::config::merge::{EffectiveSource, effective_sources};
use crate::config::schema::schema_json;
use crate::config::store::{ConfigStore, Imported, Revision};
use crate::config::{Loaded, load_file, load_str};
use crate::filter::{Fields, Match, RuleSet};
use crate::init;
use crate::lock::{self, LockError};
use crate::paths::{Dirs, StatePaths};
use crate::state::State;

/// Process exit codes.
pub mod exit {
    pub const OK: i32 = 0;
    pub const FATAL: i32 = 1;
    pub const USAGE: i32 = 2;
    /// The run finished, but at least one source, video or hook failed.
    pub const PARTIAL: i32 = 3;
    /// The YouTube API couldn't be used and `fallback_to_ytdlp = false`.
    pub const NO_FALLBACK: i32 = 4;
}

/// A command-line usage problem found after parsing (exit 2).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

pub(crate) struct App<'a> {
    pub(crate) cli: &'a Cli,
    pub(crate) dirs: Dirs,
    pub(crate) paths: StatePaths,
    /// Ctrl-C and `kill`, for commands that run yt-dlp, ffmpeg or hooks.
    pub(crate) shutdown: crate::shutdown::Shutdown,
    /// A source couldn't use the API and `fallback_to_ytdlp = false` (exit 4).
    no_fallback: std::cell::Cell<bool>,
    /// What this does in `tidepool run --daemon`, if anything.
    role: Role,
    /// The daemon's library lock, shared by its services.
    library: Option<std::sync::Arc<lock::Shared>>,
    /// Told when a download is waiting to be processed (the daemon's
    /// processor waits on it).
    fetched: Option<std::sync::Arc<tokio::sync::Notify>>,
}

/// Which part of the work an [`App`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// A command: download and process, then run the queued hooks.
    OneShot,
    /// The daemon's syncer: index, download, clean up.
    Syncer,
    /// The daemon's processor: process downloads, run queued hooks.
    Processor,
    /// The daemon's web UI.
    Server,
}

impl<'a> App<'a> {
    /// An [`App`] for one of the daemon's services.
    pub(crate) fn for_service(
        cli: &'a Cli,
        dirs: Dirs,
        shutdown: crate::shutdown::Shutdown,
        role: Role,
        library: std::sync::Arc<lock::Shared>,
        fetched: std::sync::Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            cli,
            paths: dirs.state(),
            dirs,
            shutdown,
            no_fallback: std::cell::Cell::new(false),
            role,
            library: Some(library),
            fetched: Some(fetched),
        }
    }
}

/// Run a parsed command; returns the process exit code.
///
/// # Errors
///
/// Fails if the home directory can't be found, or the command can't do its
/// work (an invalid config, an unusable state database, a failed write, …);
/// a [`UsageError`] for a bad command line. Failures a command reports itself
/// show in the exit code instead.
pub async fn run(cli: &Cli, out: &mut dyn Write) -> anyhow::Result<i32> {
    let dirs = Dirs::from_env().map_err(anyhow::Error::msg)?;
    let paths = dirs.state();
    // Commands that start other programs stop them on Ctrl-C (first: give
    // them `shutdown_timeout` to finish; second: right away). Others keep the
    // default: Ctrl-C ends them at once.
    let shutdown = match &cli.command {
        Command::Run(_)
        | Command::Index(_)
        | Command::Download(_)
        | Command::Hooks { .. }
        | Command::Embed(_)
        | Command::CleanTitles(_)
        | Command::Cleanup(_) => crate::shutdown::Shutdown::listen(),
        _ => crate::shutdown::Shutdown::default(),
    };
    let app = App {
        cli,
        dirs,
        paths,
        shutdown,
        no_fallback: std::cell::Cell::new(false),
        role: Role::OneShot,
        library: None,
        fetched: None,
    };
    if cli.config.is_some() && !cli.dry_run && uses_stored_config(&cli.command) {
        app.import_config_flag()?;
    }
    if !matches!(cli.command, Command::Problems { .. } | Command::Init(_)) {
        app.show_problem_notices()?;
    }
    if cli.config.is_none() && !cli.config_flags.skip_check && checks_config_drift(&cli.command) {
        app.check_config_drift()?;
    }
    match &cli.command {
        Command::Init(args) => app.init(args, out),
        Command::Config(args) => app.config(args, out),
        Command::Filter {
            cmd:
                FilterCmd::Test {
                    video: title,
                    source,
                },
        } => app.filter_test(title, source.as_ref(), out),
        Command::Tools { cmd } => app.tools(cmd, out).await,
        Command::Index(scope) => app.index(scope, out).await,
        Command::Download(args) => app.download(args, out).await,
        Command::Run(args) if !args.daemon => app.run_once(args, out).await,
        Command::Run(_) => crate::daemon::run(&app).await,
        Command::Reset(args) => app.reset(args, out),
        Command::Problems { cmd } => app.problems(cmd.as_ref(), out),
        Command::Hooks { cmd } => app.hooks(cmd, out).await,
        Command::Embed(args) => app.embed(args, out).await,
        Command::CleanTitles(select) => app.clean_titles(select, out).await,
        Command::Cleanup(scope) => app.cleanup(scope, out).await,
        Command::Trash { cmd } => app.trash(cmd, out),
        Command::Daemon { cmd } => crate::daemon::control::command(&app, cmd, out).await,
    }
}

/// What differs from the stored config (`stored`, JSON) to a file's
/// (`file`), one indented line per key, secrets masked.
pub(crate) fn config_changes(stored: &str, file: Value) -> anyhow::Result<Vec<String>> {
    let mut stored: Value = serde_json::from_str(stored)?;
    let mut file = file;
    mask_json(&mut stored);
    mask_json(&mut file);
    Ok(key_changes(&stored, &file)
        .iter()
        .map(|c| format!("  {c}"))
        .collect())
}

/// Commands that halt when the config file has changes that aren't imported.
/// (`config …` commands never do: they're how drift gets resolved.)
fn checks_config_drift(command: &Command) -> bool {
    match command {
        Command::Run(args) => !args.daemon,
        Command::Index(_)
        | Command::Download(_)
        | Command::Reset(_)
        | Command::Filter { .. }
        | Command::Hooks { .. }
        | Command::Embed(_)
        | Command::CleanTitles(_)
        | Command::Cleanup(_) => true,
        _ => false,
    }
}

/// Commands that read the stored config, so `--config` is imported
/// before they run.
fn uses_stored_config(command: &Command) -> bool {
    match command {
        Command::Config(args) => matches!(
            args.cmd,
            None | Some(
                ConfigCmd::Export { .. }
                    | ConfigCmd::History
                    | ConfigCmd::Rollback { .. }
                    | ConfigCmd::Diff { .. }
                    | ConfigCmd::Check { path: None, .. }
            )
        ),
        Command::Filter { .. }
        | Command::Tools { .. }
        | Command::Index(_)
        | Command::Download(_)
        | Command::Reset(_)
        | Command::Hooks { .. }
        | Command::Embed(_)
        | Command::CleanTitles(_)
        | Command::Cleanup(_) => true,
        // The daemon compares the file instead (`daemon::startup_check`).
        Command::Run(args) => !args.daemon,
        _ => false,
    }
}

impl App<'_> {
    /// The stored config, or built-in defaults when there's none yet
    /// (so commands like `ytdlp` work before `init`).
    fn config_or_defaults(&self) -> anyhow::Result<crate::config::Config> {
        if let (Some(path), true) = (&self.cli.config, self.cli.dry_run) {
            return Ok(load_file(path)?.config);
        }
        if !self.paths.state_db.exists() {
            return Ok(crate::config::Config::default());
        }
        let state = self.open_state()?;
        match ConfigStore::new(&state.conn).load_current() {
            Ok((_, loaded)) => Ok(loaded.config),
            Err(crate::config::store::StoreError::Empty) => Ok(crate::config::Config::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// The sources a command works on: the ones named with `--source`, or
    /// every enabled one.
    fn scoped_sources(
        config: &crate::config::Config,
        refs: &[SourceRef],
    ) -> anyhow::Result<Vec<EffectiveSource>> {
        let all = effective_sources(config);
        if refs.is_empty() {
            return Ok(all.into_iter().filter(|s| s.enabled).collect());
        }
        let mut picked: Vec<EffectiveSource> = Vec::new();
        for r in refs {
            let source = resolve_source(r, &all)?;
            if !picked.iter().any(|p| p.key == source.key) {
                picked.push(source.clone());
            }
        }
        Ok(picked)
    }

    /// `-v`: what a run is working with.
    fn log_context(
        &self,
        state: &State,
        config: &crate::config::Config,
        sources: &[EffectiveSource],
    ) {
        use crate::config::store::Source;
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let current = ConfigStore::new(&state.conn).current().ok().flatten();
        match (&self.cli.config, self.cli.dry_run, current) {
            (Some(path), true, _) => {
                tracing::debug!("using {} without storing it (dry run)", path.display());
            }
            (_, _, Some(rev)) => {
                let when = rev
                    .created_at
                    .parse::<jiff::Timestamp>()
                    .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_default();
                let origin = match (rev.source, &rev.source_path) {
                    (Source::Import, Some(path)) => format!("imported from {path}"),
                    (Source::Rollback, Some(from)) => {
                        format!("a rollback to {}", from.replace("rev:", "revision "))
                    }
                    (source, _) => format!("from {source}"),
                };
                tracing::debug!("config revision {}, {origin} on {when}", rev.rev);
            }
            _ => {}
        }
        let mut folders: Vec<(PathBuf, PathBuf)> = Vec::new();
        for source in sources {
            if let (Ok(root), Ok(tmp)) =
                (media_dir(source, &self.dirs), tmp_dir(source, &self.dirs))
                && !folders.contains(&(root.clone(), tmp.clone()))
            {
                folders.push((root, tmp));
            }
        }
        for (root, tmp) in folders {
            tracing::debug!("media in {}, work area {}", root.display(), tmp.display());
        }
        tracing::debug!("state in {}", self.paths.state_db.display());
        if tracing::enabled!(tracing::Level::TRACE) {
            // Everything the run uses, as `config check` prints it.
            let effective = EffectiveConfig::new(config, &self.dirs, false).to_toml();
            tracing::trace!(
                "effective config (secrets redacted):\n{}",
                effective.trim_end()
            );
        }
        for source in sources {
            tracing::debug!(
                "{}: {} (category {}), format {}, output_dir {}",
                source.key,
                source.urls.join(", "),
                source.category,
                source.format,
                source.output_dir
            );
            let rules = |rules: &[crate::config::Rule]| {
                rules
                    .iter()
                    .map(|r| match r {
                        crate::config::Rule::Pattern(p) => format!("{p:?}"),
                        crate::config::Rule::Field(f) => {
                            format!("{:?} on {:?}", f.pattern, f.field).to_lowercase()
                        }
                        crate::config::Rule::Value(v) => {
                            format!("{} = {:?}", crate::filter::field_name(v.field), v.value)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut filters = Vec::new();
            if !source.include.is_empty() {
                filters.push(format!("include [{}]", rules(&source.include)));
            }
            if !source.exclude.is_empty() {
                filters.push(format!("exclude [{}]", rules(&source.exclude)));
            }
            if let Some(d) = source.min_duration.0 {
                filters.push(format!(
                    "min_duration {}",
                    crate::config::types::format_duration(d)
                ));
            }
            if let Some(d) = source.max_duration.0 {
                filters.push(format!(
                    "max_duration {}",
                    crate::config::types::format_duration(d)
                ));
            }
            if source.cutoff.text != "none" {
                filters.push(format!("cutoff {}", source.cutoff.text));
            }
            if !filters.is_empty() {
                tracing::debug!("{}: filters: {}", source.key, filters.join(", "));
            }
        }
    }

    /// The yt-dlp to run, with the Deno it should use: the managed ones
    /// (downloaded or updated if needed) or your own. Failed installs and
    /// update checks become problems.
    async fn ytdlp_binary(
        &self,
        state: &State,
        config: &crate::config::Config,
    ) -> anyhow::Result<crate::tools::Ytdlp> {
        use crate::problems::Kind;
        use crate::tools::manager::Tool;

        let ytdlp = &config.tools.ytdlp;
        let path = if ytdlp.managed {
            let ensured = self
                .ensure_tool(
                    state,
                    Tool::Ytdlp(ytdlp.channel),
                    ytdlp.update_check.0,
                    Kind::UpdateFailed,
                )
                .await?;
            log_ytdlp(
                &ensured,
                &format!("managed, {} channel", ytdlp.channel.as_str()),
            );
            ensured
        } else {
            let path = PathBuf::from(&ytdlp.path);
            log_ytdlp(&path, "your own; tidepool doesn't update it");
            path
        };

        let deno = &config.tools.deno;
        let deno = if deno.managed {
            match self
                .ensure_tool(state, Tool::Deno, deno.update_check.0, Kind::DenoFailed)
                .await
            {
                Ok(path) => {
                    tracing::debug!("Deno at {} (managed)", path.display());
                    Some(path)
                }
                // yt-dlp still runs; YouTube may not work without it.
                Err(err) => {
                    tracing::warn!("{err}; yt-dlp runs without a JavaScript runtime");
                    None
                }
            }
        } else if deno.path == "deno" {
            // yt-dlp finds a `deno` on PATH by itself.
            None
        } else {
            Some(PathBuf::from(&deno.path))
        };
        Ok(crate::tools::Ytdlp { path, deno })
    }

    /// A managed tool, installed or updated when due. Failures are recorded
    /// as `problem`; a successful check resolves it.
    async fn ensure_tool(
        &self,
        state: &State,
        tool: crate::tools::manager::Tool,
        update_check: std::time::Duration,
        problem: crate::problems::Kind,
    ) -> anyhow::Result<PathBuf> {
        use crate::problems::{record, resolve};
        use crate::tools::manager::{Manager, UpdateOutcome};

        let now = jiff::Timestamp::now();
        let manager = Manager::new(&self.dirs);
        let version = manager.state(tool).map(|s| s.version);
        let fail = |message: &str| {
            record(
                &state.conn,
                problem,
                1,
                message,
                version.as_deref(),
                &std::collections::BTreeSet::new(),
                &problem.hint(version.as_deref(), true),
                now,
            )
        };
        let ensured = match manager.ensure(tool, update_check, now).await {
            Ok(ensured) => ensured,
            Err(err) => {
                fail(&err.to_string())?;
                return Err(err.into());
            }
        };
        if let Some(warning) = &ensured.warning {
            tracing::warn!("{warning}");
            fail(warning)?;
        }
        if let Some(outcome) = ensured.outcome {
            resolve(&state.conn, problem, now)?;
            match outcome {
                UpdateOutcome::Installed { .. } | UpdateOutcome::Updated { .. } => {
                    tracing::info!("{outcome}");
                }
                _ => tracing::debug!("{outcome}"),
            }
        }
        Ok(ensured.path)
    }

    /// End of a run: if it hit something a newer yt-dlp usually fixes, check
    /// for one right away (at most hourly); then record and resolve problems.
    async fn finish_problems(
        &self,
        state: &State,
        config: &crate::config::Config,
        tracker: crate::problems::Tracker,
        ytdlp: &crate::tools::Ytdlp,
    ) -> anyhow::Result<()> {
        let is_managed = config.tools.ytdlp.managed;
        if is_managed && tracker.wants_update() {
            let manager = crate::tools::manager::Manager::new(&self.dirs);
            let hour = std::time::Duration::from_secs(3600);
            match manager
                .ensure(
                    crate::tools::manager::Tool::Ytdlp(config.tools.ytdlp.channel),
                    hour,
                    jiff::Timestamp::now(),
                )
                .await
            {
                Ok(ensured) => {
                    if let Some(outcome @ crate::tools::manager::UpdateOutcome::Updated { .. }) =
                        ensured.outcome
                    {
                        tracing::info!("{outcome}; failed videos are retried on the next run");
                    }
                }
                Err(err) => tracing::warn!("could not check for a yt-dlp update: {err}"),
            }
        }
        let version = crate::tools::manager::tool_version(ytdlp.as_path()).ok();
        tracker.finish(
            &state.conn,
            version.as_deref(),
            is_managed,
            jiff::Timestamp::now(),
        )?;
        Ok(())
    }

    async fn index(&self, scope: &ScopeArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let mut state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = Self::scoped_sources(&loaded.config, &scope.sources)?;
        self.log_context(&state, &loaded.config, &sources);
        if sources.is_empty() {
            writeln!(
                out,
                "no enabled sources; add one under [sources.<name>] in your config"
            )?;
            return Ok(exit::OK);
        }
        let _lock = self.lock()?;
        let ytdlp = self.ytdlp_binary(&state, &loaded.config).await?;
        let mut tracker = crate::problems::Tracker::default();
        let failed = self
            .index_sources(
                &mut state,
                &loaded.config,
                &sources,
                &ytdlp,
                &mut tracker,
                out,
            )
            .await?;
        if let Some(code) = self.shutdown.code() {
            writeln!(out, "stopped; the rest will be checked on the next run")?;
            return Ok(code);
        }
        self.finish_problems(&state, &loaded.config, tracker, &ytdlp)
            .await?;
        Ok(if self.no_fallback.get() {
            exit::NO_FALLBACK
        } else if failed > 0 {
            exit::PARTIAL
        } else {
            exit::OK
        })
    }

    /// Index `sources`, printing a line per source; returns how many failed.
    async fn index_sources(
        &self,
        state: &mut State,
        config: &crate::config::Config,
        sources: &[EffectiveSource],
        ytdlp: &crate::tools::Ytdlp,
        tracker: &mut crate::problems::Tracker,
        out: &mut dyn Write,
    ) -> anyhow::Result<usize> {
        let now = jiff::Timestamp::now();
        let mut failed = 0;
        let mut apis = crate::pipeline::ApiPool::default();
        for source in sources {
            if self.shutdown.is_stopping() {
                break;
            }
            crate::events::emit(crate::events::Event::Checking {
                source: source.key.clone(),
            });
            match crate::pipeline::index_source(
                state,
                config,
                source,
                &self.dirs,
                ytdlp,
                &mut apis,
                tracker,
                &self.shutdown,
                now,
            )
            .await
            {
                Err(crate::pipeline::IndexFailure::Stopped) => break,
                Ok(summary) => {
                    // Some lists couldn't be read: the rest was recorded,
                    // but the source still failed.
                    for (error, no_fallback) in &summary.errors {
                        tracing::error!("{}: {error}", source.key);
                        if *no_fallback {
                            self.no_fallback.set(true);
                        }
                    }
                    if summary.errors.is_empty() {
                        tracker.index_succeeded(&source.key);
                    } else {
                        failed += 1;
                        let errors: Vec<&str> =
                            summary.errors.iter().map(|(e, _)| e.as_str()).collect();
                        tracker.index_failed(&source.key, &errors.join("; "));
                    }
                    tracker.regex_limit(
                        &source.key,
                        summary.count(crate::state::items::status::FILTER_ERROR),
                    );
                    writeln!(
                        out,
                        "{}",
                        crate::pipeline::describe_index(&source.key, &summary)
                    )?;
                }
                Err(err) => {
                    failed += 1;
                    if matches!(err, crate::pipeline::IndexFailure::NoFallback(_)) {
                        self.no_fallback.set(true);
                    } else {
                        tracker.index_failed(&source.key, &err.to_string());
                    }
                    tracing::error!("{}: {err}", source.key);
                }
            }
        }
        Ok(failed)
    }

    /// Download what's queued for `sources`, printing a line per video and a summary.
    #[allow(clippy::too_many_arguments)]
    async fn download_sources(
        &self,
        state: &mut State,
        config: &crate::config::Config,
        sources: &[EffectiveSource],
        ytdlp: crate::tools::Ytdlp,
        limit: Option<usize>,
        concurrency: Option<u32>,
        tracker: &mut crate::problems::Tracker,
        out: &mut dyn Write,
    ) -> anyhow::Result<crate::download::Summary> {
        use crate::download::{Context, Options};

        #[allow(unused_mut)]
        let mut thumbnail_base = crate::download::thumbnail::YTIMG.to_string();
        // Debug builds (tests) can point cover downloads at a fake server.
        #[cfg(debug_assertions)]
        if let Ok(base) = std::env::var("TIDEPOOL_TEST_THUMBNAIL_BASE") {
            thumbnail_base = base;
        }
        let ctx = Context {
            hooks: config.hooks.clone(),
            hook_logs: self.paths.hook_logs.clone(),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            dirs: self.dirs.clone(),
            ytdlp_version: crate::tools::manager::tool_version(ytdlp.as_path()).ok(),
            ytdlp,
            duplicates: config.general.duplicates,
            thumbnail_base,
            backtrack_limit: usize::try_from(config.general.regex_backtrack_limit)
                .unwrap_or(usize::MAX),
            http: reqwest::Client::builder()
                .user_agent(concat!("tidepool/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(std::time::Duration::from_secs(15))
                .read_timeout(std::time::Duration::from_secs(30))
                .build()?,
            shutdown_timeout: config.general.shutdown_timeout.0,
            shutdown: self.shutdown.clone(),
            fetched: self.fetched.clone(),
        };
        // In the daemon, the syncer downloads and the processor processes;
        // the daemon cleans up after a crash once, when it starts.
        let options = Options {
            limit,
            concurrency: concurrency.unwrap_or(config.general.concurrency) as usize,
            process_concurrency: config.general.process_concurrency as usize,
            fetch: self.role != Role::Processor,
            process: self.role != Role::Syncer,
            recover: self.role == Role::OneShot,
            now: jiff::Timestamp::now(),
        };
        if self.role == Role::OneShot {
            for source in sources.iter().filter(|s| !s.download) {
                writeln!(
                    out,
                    "{}: downloads are off (download = false); only checked",
                    source.key
                )?;
            }
        }
        let dirs = &self.dirs;
        let paths = |source: &EffectiveSource| {
            Ok(crate::download::SourcePaths {
                media_dir: media_dir(source, dirs)?,
                tmp_root: tmp_dir(source, dirs)?,
                cookies: crate::pipeline::cookies_file(source, dirs)?,
            })
        };
        let mut summary =
            crate::download::download_all(state, sources, paths, ctx, options, tracker, out)
                .await
                .map_err(anyhow::Error::msg)?;
        if summary.interrupted.is_none() && self.role != Role::Syncer {
            summary.hooks_failed += self
                .run_hook_queue(state, config, sources, tracker, out)
                .await?;
            summary.interrupted = self.shutdown.code();
        }
        if self.role != Role::OneShot {
            // The daemon logs its own summary.
            return Ok(summary);
        }
        if summary.interrupted.is_none() && !self.cli.dry_run {
            Self::purge_trash(state, jiff::Timestamp::now(), out)?;
        }

        print_download_summary(&summary, out)?;
        Ok(summary)
    }

    /// Run the `finished` hooks of `sources` that are due: queued for videos
    /// just saved, or failed before and due for a retry (see
    /// `items::hook_retry_wait`). Returns how many hooks failed.
    async fn run_hook_queue(
        &self,
        state: &State,
        config: &crate::config::Config,
        sources: &[EffectiveSource],
        tracker: &mut crate::problems::Tracker,
        out: &mut dyn Write,
    ) -> anyhow::Result<usize> {
        use crate::state::items;
        let due: Vec<(String, String, Vec<String>)> =
            items::due_hooks(&state.conn, jiff::Timestamp::now())?
                .into_iter()
                .filter(|(source, _, _)| sources.iter().any(|s| &s.key == source))
                .collect();
        if due.is_empty() {
            return Ok(0);
        }
        let every: Vec<String> = effective_sources(config)
            .into_iter()
            .map(|s| s.key)
            .collect();
        let everywhere = items::downloaded(&state.conn, &every)?;
        let videos = everywhere
            .iter()
            .filter(|v| {
                due.iter()
                    .any(|(s, id, _)| *s == v.source_key && *id == v.video_id)
            })
            .cloned()
            .collect();
        let selected = Selected {
            sources: sources.to_vec(),
            videos,
            everywhere,
        };
        let mut rerun = Rerun::new(
            self,
            state,
            config,
            &selected,
            crate::download::rerun::Mode::Hooks,
            tracker,
        );
        rerun.retry = true;
        let mut failed = 0;
        for (source_key, video_id, hooks) in &due {
            if self.shutdown.is_stopping() {
                break;
            }
            // Gone since, or no longer downloaded: nothing to run it on.
            let Some(item) = selected
                .videos
                .iter()
                .find(|v| &v.source_key == source_key && &v.video_id == video_id)
            else {
                continue;
            };
            let source = selected.source(source_key);
            // In the source's order, if it still has them.
            let names: Vec<String> = source
                .hooks
                .iter()
                .filter(|h| hooks.contains(h))
                .filter(|h| {
                    config
                        .hooks
                        .get(*h)
                        .is_some_and(|c| c.kind == crate::config::HookType::Finished)
                })
                .cloned()
                .collect();
            if names.is_empty() {
                continue;
            }
            // Every one queued, not run yet: a first run, quiet when it works.
            let first = names.iter().all(|h| {
                items::hook_status(&state.conn, source_key, video_id, h)
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("pending")
            });
            let (title, problems) = rerun.video(item, source, &names).await?;
            let hook_failures = problems.iter().filter(|p| *p != "stopped").count();
            failed += hook_failures;
            if first {
                continue;
            }
            let label = format!("{source_key}: “{title}”");
            if problems.is_empty() {
                writeln!(out, "{label}: hook {} worked this time", names.join(", "))?;
                continue;
            }
            let line = retry_line(state, &label, source_key, video_id, &names, &problems);
            writeln!(out, "{line}")?;
        }
        Ok(failed)
    }

    fn exit_code(summary: &crate::download::Summary, index_failures: usize) -> i32 {
        match summary.interrupted {
            Some(code) => code,
            None if summary.failed > 0
                || summary.hooks_failed > 0
                || summary.missing.failed > 0
                || index_failures > 0 =>
            {
                exit::PARTIAL
            }
            None => exit::OK,
        }
    }

    async fn download(&self, args: &DownloadArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let mut state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = Self::scoped_sources(&loaded.config, &args.scope.sources)?;
        self.log_context(&state, &loaded.config, &sources);
        let _lock = self.lock()?;
        let ytdlp = self.ytdlp_binary(&state, &loaded.config).await?;
        let limit = args.limit.map(|l| l as usize);
        let mut tracker = crate::problems::Tracker::default();
        let summary = self
            .download_sources(
                &mut state,
                &loaded.config,
                &sources,
                ytdlp.clone(),
                limit,
                args.concurrency,
                &mut tracker,
                out,
            )
            .await?;
        if summary.interrupted.is_none() {
            self.finish_problems(&state, &loaded.config, tracker, &ytdlp)
                .await?;
        }
        Ok(Self::exit_code(&summary, 0))
    }

    /// `tidepool run`: index, then download, once.
    /// One sync of the daemon's syncer: index every enabled source, download
    /// what's new (the processor finishes it), clean up. `None` when another
    /// tidepool holds the library.
    pub(crate) async fn sync_once(&self, out: &mut dyn Write) -> anyhow::Result<Option<String>> {
        let mut state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let sources = Self::scoped_sources(&config, &[])?;
        if sources.is_empty() {
            return Ok(Some(
                "no enabled sources; add one under [sources.<name>] in your config".into(),
            ));
        }
        let lock = match self.lock() {
            Ok(lock) => lock,
            Err(LockError::Held { pid }) => {
                tracing::info!(
                    "skipped: another tidepool is running{}",
                    pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
                );
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let ytdlp = self.ytdlp_binary(&state, &config).await?;
        let mut tracker = crate::problems::Tracker::default();
        let failed = self
            .index_sources(&mut state, &config, &sources, &ytdlp, &mut tracker, out)
            .await?;
        if self.shutdown.is_stopping() {
            return Ok(Some("stopped".into()));
        }
        let summary = self
            .download_sources(
                &mut state,
                &config,
                &sources,
                ytdlp.clone(),
                None,
                None,
                &mut tracker,
                out,
            )
            .await?;
        if summary.interrupted.is_some() {
            return Ok(Some("stopped".into()));
        }
        self.finish_problems(&state, &config, tracker, &ytdlp)
            .await?;
        drop(lock);
        self.cleanup(
            &ScopeArgs {
                sources: Vec::new(),
            },
            out,
        )
        .await?;
        let mut parts = vec![format!(
            "synced {} source{}",
            sources.len(),
            if sources.len() == 1 { "" } else { "s" }
        )];
        if summary.fetched > 0 {
            parts.push(format!("{} downloaded", summary.fetched));
        }
        if summary.failed > 0 {
            parts.push(format!("{} failed", summary.failed));
        }
        if failed > 0 {
            parts.push(format!(
                "{failed} source{} couldn't be checked",
                if failed == 1 { "" } else { "s" }
            ));
        }
        Ok(Some(parts.join(", ")))
    }

    /// One pass of the daemon's processor: finish what's downloaded, run the
    /// queued hooks. `None` when another tidepool holds the library.
    pub(crate) async fn process_once(
        &self,
        out: &mut dyn Write,
    ) -> anyhow::Result<Option<crate::download::Summary>> {
        let mut state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        // Every source, enabled or not: what's downloaded gets finished.
        let sources = effective_sources(&config);
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(LockError::Held { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let ytdlp = self.ytdlp_binary(&state, &config).await?;
        let mut tracker = crate::problems::Tracker::default();
        let summary = self
            .download_sources(
                &mut state,
                &config,
                &sources,
                ytdlp.clone(),
                None,
                None,
                &mut tracker,
                out,
            )
            .await?;
        if summary.interrupted.is_none() {
            self.finish_problems(&state, &config, tracker, &ytdlp)
                .await?;
        }
        Ok(Some(summary))
    }

    /// Put things back after a crash, for every source: interrupted videos,
    /// work areas (see `download::recover`).
    pub(crate) fn recover(&self, state: &State) -> anyhow::Result<usize> {
        let config = self.current_config(state)?.config;
        let mut by_source = std::collections::BTreeMap::new();
        for source in effective_sources(&config) {
            by_source.insert(
                source.key.clone(),
                crate::download::SourcePaths {
                    media_dir: media_dir(&source, &self.dirs).map_err(anyhow::Error::msg)?,
                    tmp_root: tmp_dir(&source, &self.dirs).map_err(anyhow::Error::msg)?,
                    cookies: None,
                },
            );
        }
        crate::download::recover(state, &by_source).map_err(anyhow::Error::msg)
    }

    async fn run_once(&self, args: &RunArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let mut state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = Self::scoped_sources(&loaded.config, &args.sources)?;
        self.log_context(&state, &loaded.config, &sources);
        if sources.is_empty() {
            writeln!(
                out,
                "no enabled sources; add one under [sources.<name>] in your config"
            )?;
            return Ok(exit::OK);
        }
        let _lock = self.lock()?;
        let ytdlp = self.ytdlp_binary(&state, &loaded.config).await?;
        let mut tracker = crate::problems::Tracker::default();
        let failed = self
            .index_sources(
                &mut state,
                &loaded.config,
                &sources,
                &ytdlp,
                &mut tracker,
                out,
            )
            .await?;
        if let Some(code) = self.shutdown.code() {
            writeln!(out, "stopped; the rest will be checked on the next run")?;
            return Ok(code);
        }
        let summary = self
            .download_sources(
                &mut state,
                &loaded.config,
                &sources,
                ytdlp.clone(),
                None,
                None,
                &mut tracker,
                out,
            )
            .await?;
        if summary.interrupted.is_none() {
            self.finish_problems(&state, &loaded.config, tracker, &ytdlp)
                .await?;
        }
        let code = Self::exit_code(&summary, failed);
        Ok(if self.no_fallback.get() && summary.interrupted.is_none() {
            exit::NO_FALLBACK
        } else {
            code
        })
    }

    /// `tidepool problems [ack <id>]`.
    fn problems(
        &self,
        cmd: Option<&crate::cli::ProblemsCmd>,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        use crate::problems::{acknowledge, describe, open, recently_resolved};

        if !self.paths.state_db.exists() {
            writeln!(out, "no open problems")?;
            return Ok(exit::OK);
        }
        let state = self.open_state()?;
        if let Some(crate::cli::ProblemsCmd::Ack { id }) = cmd {
            if !acknowledge(&state.conn, *id, jiff::Timestamp::now())? {
                bail!("no open problem #{id} (see `tidepool problems`)");
            }
            writeln!(out, "problem #{id} is hidden until it happens again")?;
            return Ok(exit::OK);
        }
        let problems = open(&state.conn)?;
        if problems.is_empty() {
            writeln!(out, "no open problems")?;
        }
        for (i, problem) in problems.iter().enumerate() {
            if i > 0 {
                writeln!(out)?;
            }
            writeln!(out, "{}", describe(problem))?;
        }
        let resolved = recently_resolved(&state.conn, 5)?;
        if !resolved.is_empty() {
            writeln!(out, "\nrecently resolved:")?;
            for problem in resolved {
                let when = problem
                    .resolved_at
                    .as_deref()
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                    .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_default();
                writeln!(
                    out,
                    "  #{} {} (resolved {when})",
                    problem.id,
                    problem.kind.title()
                )?;
            }
        }
        let ids: Vec<i64> = problems.iter().map(|p| p.id).collect();
        crate::problems::mark_shown(&state.conn, &ids, jiff::Timestamp::now())?;
        Ok(exit::OK)
    }

    /// Before a command: new problems in full, a reminder for known ones.
    /// Halt when the config file (the one last imported, or the conventional
    /// one) resolves to something other than the stored config.
    fn check_config_drift(&self) -> anyhow::Result<()> {
        use crate::config::store::Source;

        if !self.paths.state_db.exists() {
            return Ok(());
        }
        let state = self.open_state()?;
        let store = ConfigStore::new(&state.conn);
        let Some(current) = store.current()? else {
            return Ok(());
        };
        let path = store
            .last_import()?
            .and_then(|r| r.source_path)
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| self.dirs.default_config_file());
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::debug!("no config file at {} to compare with", path.display());
            return Ok(());
        };
        if crate::config::store::hash(&text) == current.source_hash {
            return Ok(());
        }
        let name = Self::display_path(&path);
        let skip = format!(
            "Apply it with `tidepool config import`, or pass --skip-config-check to run with the \
             stored config (revision {}).",
            current.rev
        );
        let loaded = match load_str(&name, &text) {
            Ok(loaded) => loaded,
            Err(err) => bail!(
                "{name} has changes that aren't applied yet, and it has errors:\n{err}\nFix them, or pass --skip-config-check to run with the stored config (revision {}).",
                current.rev
            ),
        };
        if loaded.normalized() == current.json {
            return Ok(()); // only comments or formatting changed
        }
        let changes = config_changes(&current.json, loaded.json)?;
        let mut message = format!(
            "{name} has changes that aren't applied yet:\n{}\n{skip}",
            changes.join("\n")
        );
        if current.source != Source::Import {
            let _ = write!(
                message,
                "\nNote: the stored config came from a {} (revision {}); to keep it instead, write it \
                 back to the file with `tidepool config export --show-secrets > {name}`.",
                current.source, current.rev
            );
        }
        bail!(message)
    }

    fn show_problem_notices(&self) -> anyhow::Result<()> {
        if !self.paths.state_db.exists() {
            return Ok(());
        }
        let state = self.open_state()?;
        let open: Vec<_> = crate::problems::open(&state.conn)?
            .into_iter()
            .filter(|p| p.acknowledged_at.is_none())
            .collect();
        let (new, known): (Vec<_>, Vec<_>) = open.iter().partition(|p| p.shown_cli_at.is_none());
        for problem in &new {
            eprintln!(
                "tidepool problem {}\n   (`tidepool problems` lists them; `tidepool problems ack {}` hides this one)",
                crate::problems::describe(problem),
                problem.id
            );
        }
        if !known.is_empty() {
            let n = known.len();
            eprintln!(
                "{n} open {} (see `tidepool problems`)",
                if n == 1 { "problem" } else { "problems" }
            );
        }
        let ids: Vec<i64> = new.iter().map(|p| p.id).collect();
        crate::problems::mark_shown(&state.conn, &ids, jiff::Timestamp::now())?;
        Ok(())
    }

    /// `tidepool reset <video>`: download it again on the next run.
    pub(crate) fn reset(&self, args: &ResetArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let state = self.open_state()?;
        self.current_config(&state)?;
        let _lock = self.lock()?;
        let reset = crate::state::items::reset_video(&state.conn, &args.video_id)?;
        if reset.is_empty() {
            bail!("no video {} in any source", args.video_id);
        }
        if args.delete_files {
            // Only the files tidepool placed, then their folder if it's empty.
            for files in reset.iter().filter_map(|(_, files)| files.as_ref()) {
                for path in files.paths().into_iter().map(Path::new) {
                    if path.is_absolute() && (path.is_file() || path.is_symlink()) {
                        std::fs::remove_file(path)
                            .with_context(|| format!("cannot delete {}", path.display()))?;
                        writeln!(out, "deleted {}", path.display())?;
                    }
                }
                let dir = Path::new(&files.dir);
                if dir.is_absolute() && std::fs::remove_dir(dir).is_ok() {
                    writeln!(out, "deleted {}", dir.display())?;
                }
            }
        }
        let sources: Vec<&str> = reset.iter().map(|(key, _)| key.as_str()).collect();
        writeln!(
            out,
            "reset {} in {}; it will be downloaded on the next run",
            args.video_id,
            sources.join(", ")
        )?;
        Ok(exit::OK)
    }

    /// `tidepool clean-titles`: clean up the titles of downloaded videos
    /// (`clean_title`) from YouTube's original, renaming their files to match
    /// and embedding the new title.
    async fn clean_titles(
        &self,
        select: &crate::cli::VideoSelection,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        use crate::download::layout::Values;

        let dry_run = self.cli.dry_run;
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let selected = Self::select_downloaded(&state, &config, select, out)?;
        let library_lock = if dry_run { None } else { Some(self.lock()?) };
        let (mut renamed, mut unchanged, mut failed) = (Vec::new(), 0usize, 0usize);
        let mut skipped_sources = std::collections::BTreeSet::new();
        for item in &selected.videos {
            if self.shutdown.is_stopping() {
                break;
            }
            let source = selected.source(&item.source_key);
            if source.clean_title != crate::config::CleanTitle::MusicVideo {
                skipped_sources.insert(source.key.clone());
                continue;
            }
            let (meta, info) =
                match crate::download::rerun::load_metadata(&item.files, &item.video_id) {
                    Ok(found) => found,
                    Err(e) => {
                        writeln!(out, "{}: {}: skipped: {e}", item.source_key, item.video_id)?;
                        failed += 1;
                        continue;
                    }
                };
            let label = format!("{}: “{}”", item.source_key, info.title);
            let youtube = meta["original"]["title"]
                .as_str()
                .unwrap_or(&info.title)
                .to_string();
            let source_title = meta["source"]["title"].as_str().map(str::to_string);
            let position = source_position(&meta);
            let values = Values::new(&info, source, source_title.as_deref(), position);
            let name = match self.music_video_artist(source, &values).await {
                Ok(Some(name)) => name,
                Ok(None) => {
                    writeln!(out, "{label}: skipped: no MUSICBRAINZ_ARTISTID")?;
                    continue;
                }
                Err(e) => {
                    writeln!(out, "{label}: failed: {e}")?;
                    failed += 1;
                    continue;
                }
            };
            let cleaned = crate::titles::music_video(&youtube, &name);
            if cleaned == info.title {
                unchanged += 1;
                continue;
            }
            if dry_run {
                writeln!(out, "{label} → “{cleaned}” (dry run)")?;
                renamed.push(item.video_id.clone());
                continue;
            }

            // Where its files go with the new title.
            let mut new_info = info.clone();
            new_info.title.clone_from(&cleaned);
            let new_values = Values::new(&new_info, source, source_title.as_deref(), position);
            let (new, metadata_dest) =
                Self::retitled_files(source, &new_values, &item.files, &self.dirs)?;
            let meta = retitled_metadata(&meta, &cleaned, &youtube, &new);
            if let Err(problem) = move_retitled(&item.files, &new, &metadata_dest, &meta) {
                writeln!(out, "{label}: {problem}")?;
                failed += 1;
                continue;
            }
            let bytes = std::fs::metadata(&new.media).map_or(0, |m| m.len());
            crate::state::items::update_files(
                &state.conn,
                &item.source_key,
                &item.video_id,
                &new,
                bytes,
            )?;
            writeln!(out, "{label} → “{cleaned}”")?;
            renamed.push(item.video_id.clone());
        }
        for key in &skipped_sources {
            writeln!(out, "{key}: skipped: clean_title isn't set for it")?;
        }

        // The new titles go into the files too.
        let mut code = exit::OK;
        if !dry_run && !renamed.is_empty() {
            drop(library_lock);
            code = self.embed_retitled(&selected, &renamed, out).await?;
        }
        writeln!(
            out,
            "{}",
            clean_titles_summary(dry_run, renamed.len(), unchanged, failed)
        )?;
        Ok(if failed > 0 { exit::PARTIAL } else { code })
    }

    /// The name of the artist a music video's `MUSICBRAINZ_ARTISTID` tag
    /// names, from MusicBrainz; `None` when it has no artist ID.
    async fn music_video_artist(
        &self,
        source: &EffectiveSource,
        values: &crate::download::layout::Values,
    ) -> Result<Option<String>, String> {
        let Some(artist) = crate::download::stages::artist_id(source, values) else {
            return Ok(None);
        };
        crate::musicbrainz::artist_name(
            &source.embed.musicbrainz_server,
            &self.dirs.data_dir,
            &artist,
        )
        .await
        .map(Some)
    }

    /// Where a video's files go when its title changes, and its metadata
    /// file's new place.
    fn retitled_files(
        source: &EffectiveSource,
        values: &crate::download::layout::Values,
        files: &crate::state::items::VideoFiles,
        dirs: &Dirs,
    ) -> anyhow::Result<(crate::state::items::VideoFiles, PathBuf)> {
        let media_root = media_dir(source, dirs).map_err(anyhow::Error::msg)?;
        let layout = crate::download::layout::Layout::new(source, values, &media_root, dirs)
            .map_err(anyhow::Error::msg)?;
        let ext = |path: &str| {
            Path::new(path)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let mut new = files.clone();
        new.dir = layout.output_dir().display().to_string();
        new.media = layout
            .output_file(&ext(&files.media))
            .map_err(anyhow::Error::msg)?
            .display()
            .to_string();
        if files.cover.is_some() {
            new.cover = Some(
                layout
                    .cover_file()
                    .map_err(anyhow::Error::msg)?
                    .0
                    .display()
                    .to_string(),
            );
        }
        for sub in &mut new.subtitles {
            sub.path = layout
                .subtitle_file(&sub.lang, &ext(&sub.path))
                .map_err(anyhow::Error::msg)?
                .display()
                .to_string();
        }
        let metadata_dest = layout.metadata_file().map_err(anyhow::Error::msg)?;
        new.metadata = metadata_dest.display().to_string();
        if files.raw_info.is_some() {
            let mut name = metadata_dest.file_stem().unwrap_or_default().to_owned();
            name.push(".ytdlp.json");
            new.raw_info = Some(metadata_dest.with_file_name(name).display().to_string());
        }
        Ok((new, metadata_dest))
    }

    /// Embed the new titles of `renamed`, in the sources that embed them.
    async fn embed_retitled(
        &self,
        selected: &Selected,
        renamed: &[String],
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let embed_sources: Vec<SourceRef> = selected
            .sources
            .iter()
            .filter(|s| s.embed.metadata && s.clean_title != crate::config::CleanTitle::None)
            .map(|s| SourceRef::Key(s.key.clone()))
            .collect();
        if embed_sources.is_empty() {
            return Ok(exit::OK);
        }
        let args = crate::cli::EmbedArgs {
            select: crate::cli::VideoSelection {
                video_ids: renamed.to_vec(),
                all: false,
                sources: embed_sources,
                categories: Vec::new(),
            },
            remove_disabled: false,
        };
        self.embed(&args, out).await
    }

    /// `tidepool hooks run`: run hooks again on videos already downloaded.
    pub(crate) async fn hooks(&self, cmd: &HooksCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
        let HooksCmd::Run {
            select,
            hooks: only_hooks,
            failed: only_failed,
        } = cmd;
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let reruns = |name: &String| reruns_hook(&config, name);
        check_rerun_hooks(&config, only_hooks)?;
        let selected = Self::select_downloaded(&state, &config, select, out)?;
        let _lock = self.lock()?;
        let mut tracker = crate::problems::Tracker::default();
        let mut rerun = Rerun::new(
            self,
            &state,
            &config,
            &selected,
            crate::download::rerun::Mode::Hooks,
            &mut tracker,
        );
        let (mut ran, mut failed) = (0, 0);
        for item in &selected.videos {
            if self.shutdown.is_stopping() {
                break;
            }
            let source = selected.source(&item.source_key);
            let last_failed = if *only_failed {
                crate::state::items::failed_hooks(&state.conn, &item.source_key, &item.video_id)?
            } else {
                std::collections::HashSet::new()
            };
            let hook_names: Vec<String> = source
                .hooks
                .iter()
                .filter(|h| reruns(h))
                .filter(|h| only_hooks.is_empty() || only_hooks.contains(h))
                .filter(|h| !*only_failed || last_failed.contains(*h))
                .cloned()
                .collect();
            if hook_names.is_empty() {
                continue;
            }
            let (title, problems) = rerun.video(item, source, &hook_names).await?;
            ran += 1;
            let label = format!("{}: “{title}”", item.source_key);
            if problems.is_empty() {
                writeln!(out, "{label}: ran {}", hook_names.join(", "))?;
            } else {
                failed += 1;
                writeln!(out, "{label}: {}", problems.join("; "))?;
            }
        }
        drop(rerun);
        tracker.finish(&state.conn, None, false, jiff::Timestamp::now())?;
        if let Some(code) = self.shutdown.code() {
            writeln!(out, "stopped after {ran} videos; run it again to finish")?;
            return Ok(code);
        }
        if ran == 0 {
            writeln!(out, "no hooks to run for the chosen videos")?;
            return Ok(exit::OK);
        }
        let videos = if ran == 1 { "video" } else { "videos" };
        if failed > 0 {
            writeln!(out, "ran hooks for {ran} {videos}; {failed} with failures")?;
            Ok(exit::PARTIAL)
        } else {
            writeln!(out, "ran hooks for {ran} {videos}")?;
            Ok(exit::OK)
        }
    }

    /// `tidepool embed`: embed again (after the `embed` hooks) with the
    /// current settings.
    pub(crate) async fn embed(
        &self,
        args: &crate::cli::EmbedArgs,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let selected = Self::select_downloaded(&state, &config, &args.select, out)?;
        let _lock = self.lock()?;
        let mode = crate::download::rerun::Mode::Embed {
            remove_disabled: args.remove_disabled,
        };
        let mut tracker = crate::problems::Tracker::default();
        let mut rerun = Rerun::new(self, &state, &config, &selected, mode, &mut tracker);
        let (mut done, mut failed) = (0, 0);
        let mut nothing_on: Vec<&str> = Vec::new();
        for item in &selected.videos {
            if self.shutdown.is_stopping() {
                break;
            }
            let source = selected.source(&item.source_key);
            if !crate::download::stages::embedding_on(source) && !args.remove_disabled {
                if !nothing_on.contains(&source.key.as_str()) {
                    nothing_on.push(&source.key);
                }
                continue;
            }
            let hook_names: Vec<String> = source
                .hooks
                .iter()
                .filter(|h| {
                    config
                        .hooks
                        .get(*h)
                        .is_some_and(|hook| hook.kind == crate::config::HookType::Embed)
                })
                .cloned()
                .collect();
            let (title, mut problems) = rerun.video(item, source, &hook_names).await?;
            done += 1;
            let label = format!("{}: “{title}”", item.source_key);
            let embedding = rerun.last_embedding.take().unwrap_or_default();
            if let Some(e) = &embedding.error {
                problems.push(format!("embedding failed: {e}"));
            }
            if problems.is_empty() {
                writeln!(out, "{label}: {}", describe_embedding(&embedding))?;
            } else {
                failed += 1;
                writeln!(out, "{label}: {}", problems.join("; "))?;
            }
        }
        drop(rerun);
        tracker.finish(&state.conn, None, false, jiff::Timestamp::now())?;
        for key in nothing_on {
            writeln!(
                out,
                "{key}: nothing to embed (every embed option is off; --remove-disabled takes out what's in the files)"
            )?;
        }
        if let Some(code) = self.shutdown.code() {
            writeln!(out, "stopped after {done} videos; run it again to finish")?;
            return Ok(code);
        }
        let videos = if done == 1 { "video" } else { "videos" };
        if failed > 0 {
            writeln!(out, "embedded {done} {videos}; {failed} with failures")?;
            Ok(exit::PARTIAL)
        } else {
            if done > 0 {
                writeln!(out, "embedded {done} {videos}")?;
            }
            Ok(exit::OK)
        }
    }

    /// `tidepool cleanup`: remove what the sources' `cleanup` limits no
    /// longer allow (to the trash, or deleted); `--dry-run` only says what.
    pub(crate) async fn cleanup(
        &self,
        scope: &ScopeArgs,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        use crate::cleanup::plan;
        use crate::download::human_bytes;
        use crate::state::items;

        let dry_run = self.cli.dry_run;
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let sources = Self::scoped_sources(&config, &scope.sources)?;
        let _lock = if dry_run { None } else { Some(self.lock()?) };
        let now = jiff::Timestamp::now();
        if !dry_run {
            Self::purge_trash(&state, now, out)?;
        }
        let limit = usize::try_from(config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
        let mut run = CleanupRun {
            app: self,
            state: &state,
            config: &config,
            now,
            tracker: crate::problems::Tracker::default(),
            removed: 0,
            freed: 0,
            failed: 0,
            trashed: false,
        };
        for source in &sources {
            if self.shutdown.is_stopping() {
                break;
            }
            let kept = items::kept(&state.conn, &source.key)?;
            if kept.is_empty() {
                continue;
            }
            let rules = RuleSet::compile(source, limit, now).map_err(anyhow::Error::msg)?;
            let ids: Vec<String> = kept.iter().map(|k| k.video_id.clone()).collect();
            let details = items::videos(&state.conn, &ids)?;
            let candidates = cleanup_candidates(source, &kept, &details, &rules);
            let victims = plan(&source.cleanup, &candidates, now);
            // With --dry-run, a line per source: how many go and how many stay.
            let tally = cleanup_tally(&source.key, &kept, &candidates, &victims);
            let trash_root =
                crate::config::check::trash_dir(source, &self.dirs).map_err(anyhow::Error::msg)?;
            for (i, reason) in victims {
                if self.shutdown.is_stopping() {
                    break;
                }
                let k = &kept[i];
                let title = details
                    .get(&k.video_id)
                    .map(|v| v.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| k.video_id.clone());
                let label = format!("{}: “{title}”", source.key);
                if dry_run {
                    writeln!(
                        out,
                        "{label}: would remove ({reason}), {}",
                        human_bytes(k.media_bytes)
                    )?;
                    run.removed += 1;
                    run.freed += k.media_bytes;
                    continue;
                }
                run.clean_up_video(source, k, &reason, &label, &trash_root, out)
                    .await?;
            }
            if dry_run {
                writeln!(out, "{tally}")?;
            }
        }
        let hooks_failed = run.tracker.has_failures();
        let (summary, failed) = (run.summary(dry_run), run.failed);
        if !dry_run {
            run.tracker.finish(&state.conn, None, false, now)?;
        }
        writeln!(out, "{summary}")?;
        if let Some(code) = self.shutdown.code() {
            return Ok(code);
        }
        Ok(if failed > 0 || hooks_failed {
            exit::PARTIAL
        } else {
            exit::OK
        })
    }

    /// Run a video's `before_cleanup` hooks; `Some(why)` when it stays.
    #[allow(clippy::too_many_arguments)]
    async fn ask_before_cleanup(
        &self,
        state: &State,
        config: &crate::config::Config,
        source: &EffectiveSource,
        kept: &crate::state::items::Kept,
        label: &str,
        reason: &crate::cleanup::Reason,
        tracker: &mut crate::problems::Tracker,
    ) -> anyhow::Result<Option<String>> {
        let asks = source.hooks.iter().any(|h| {
            config
                .hooks
                .get(h)
                .is_some_and(|c| c.kind == crate::config::HookType::BeforeCleanup)
        });
        if !asks {
            return Ok(None);
        }
        let (meta, info) = match crate::download::rerun::load_metadata(&kept.files, &kept.video_id)
        {
            Ok(found) => found,
            Err(e) => {
                return Ok(Some(format!(
                    "its before_cleanup hooks can't be asked: {e}"
                )));
            }
        };
        let source_title = meta["source"]["title"].as_str();
        let position = source_position(&meta);
        let values = crate::download::layout::Values::new(&info, source, source_title, position);
        let hook_stage = crate::download::stages::Stage {
            source,
            hook_names: &source.hooks,
            hooks: &config.hooks,
            video_id: &kept.video_id,
            source_title,
            position,
            label,
            home: &self.dirs.home,
            hook_logs: &self.paths.hook_logs,
            shutdown: &self.shutdown,
            grace: config.general.shutdown_timeout.0,
            ffmpeg: Path::new("ffmpeg"),
            ffprobe: Path::new("ffprobe"),
            data_dir: &self.dirs.data_dir,
        };
        let context = hook_stage.context(&info);
        let mut reports = Vec::new();
        let keep = hook_stage
            .before_cleanup_hooks(
                &values,
                &kept.files,
                &context,
                &reason.to_string(),
                &mut reports,
            )
            .await;
        for report in &reports {
            crate::state::items::record_hook_run(
                &state.conn,
                &crate::download::hook_run(&source.key, &kept.video_id, report, false),
            )?;
            match &report.result {
                Ok(_) => tracker.hook_succeeded(),
                Err(e) if e == "stopped" => {}
                Err(e) => tracker.hook_failed(&source.key, &report.name, e),
            }
        }
        Ok(keep)
    }

    /// Delete for good what's been in the trash for its `trash_for`.
    fn purge_trash(state: &State, now: jiff::Timestamp, out: &mut dyn Write) -> anyhow::Result<()> {
        let mut count = 0;
        let mut bytes = 0;
        for t in crate::state::items::trash(&state.conn)? {
            let due = t
                .delete_after
                .parse::<jiff::Timestamp>()
                .is_ok_and(|at| at <= now);
            if due && delete_trashed(&t) {
                crate::state::items::remove_trash(&state.conn, t.id)?;
                count += 1;
                bytes += t.bytes;
            }
        }
        if count > 0 {
            writeln!(
                out,
                "emptied {count} {} from the trash ({})",
                if count == 1 { "video" } else { "videos" },
                crate::download::human_bytes(bytes)
            )?;
        }
        Ok(())
    }

    /// `tidepool trash list | restore | empty`.
    pub(crate) fn trash(
        &self,
        cmd: &crate::cli::TrashCmd,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        use crate::cli::TrashCmd;
        use crate::download::human_bytes;
        use crate::state::items;

        let state = self.open_state()?;
        let all = items::trash(&state.conn)?;
        let ids: Vec<String> = all.iter().map(|t| t.video_id.clone()).collect();
        let details = items::videos(&state.conn, &ids)?;
        let label = |t: &items::Trashed| {
            let title = details
                .get(&t.video_id)
                .map(|v| v.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| t.video_id.clone());
            format!("{}: “{title}” [{}]", t.source_key, t.video_id)
        };
        let day = |at: &str| {
            at.parse::<jiff::Timestamp>()
                .map(|t| t.strftime("%Y-%m-%d").to_string())
                .unwrap_or_default()
        };
        match cmd {
            TrashCmd::List => {
                if all.is_empty() {
                    writeln!(out, "the trash is empty")?;
                }
                for t in &all {
                    writeln!(
                        out,
                        "{}: {}, removed {} ({}), deleted for good on {}",
                        label(t),
                        human_bytes(t.bytes),
                        day(&t.trashed_at),
                        t.reason,
                        day(&t.delete_after)
                    )?;
                }
                Ok(exit::OK)
            }
            TrashCmd::Restore { video_ids, sources } => {
                let _lock = self.lock()?;
                let keys: Vec<String> = if sources.is_empty() {
                    Vec::new()
                } else {
                    let config = self.current_config(&state)?.config;
                    Self::scoped_sources(&config, sources)?
                        .into_iter()
                        .map(|s| s.key)
                        .collect()
                };
                let mut failed = 0;
                for id in video_ids {
                    let found: Vec<&items::Trashed> = all
                        .iter()
                        .filter(|t| &t.video_id == id)
                        .filter(|t| keys.is_empty() || keys.contains(&t.source_key))
                        .collect();
                    if found.is_empty() {
                        failed += 1;
                        writeln!(out, "{id}: not in the trash")?;
                    }
                    for t in found {
                        match crate::cleanup::restore(&t.files) {
                            Ok(()) => {
                                items::mark_restored(&state.conn, &t.source_key, &t.video_id)?;
                                items::remove_trash(&state.conn, t.id)?;
                                let _ = std::fs::remove_dir(&t.dir);
                                writeln!(out, "{}: restored", label(t))?;
                            }
                            Err(e) => {
                                failed += 1;
                                writeln!(out, "{}: {e}", label(t))?;
                            }
                        }
                    }
                }
                Ok(if failed > 0 { exit::PARTIAL } else { exit::OK })
            }
            TrashCmd::Empty => {
                let _lock = self.lock()?;
                let mut bytes = 0;
                let mut count = 0;
                for t in &all {
                    if delete_trashed(t) {
                        items::remove_trash(&state.conn, t.id)?;
                        bytes += t.bytes;
                        count += 1;
                    }
                }
                writeln!(
                    out,
                    "deleted {count} {} from the trash ({})",
                    if count == 1 { "video" } else { "videos" },
                    human_bytes(bytes)
                )?;
                Ok(exit::OK)
            }
        }
    }

    /// The downloaded videos a selection (ids, `--all`, `-s`, `--category`)
    /// picks. Ids that aren't downloaded are reported and skipped.
    fn select_downloaded(
        state: &State,
        config: &crate::config::Config,
        select: &crate::cli::VideoSelection,
        out: &mut dyn Write,
    ) -> anyhow::Result<Selected> {
        let crate::cli::VideoSelection {
            video_ids,
            all,
            sources: source_refs,
            categories,
        } = select;
        if video_ids.is_empty() && !*all && source_refs.is_empty() && categories.is_empty() {
            bail!("pick the videos: video ids, --source <name>, --category <name> or --all");
        }
        let every = effective_sources(config);
        let mut picked: Vec<EffectiveSource> = Vec::new();
        for r in source_refs {
            let source = resolve_source(r, &every)?;
            if !picked.iter().any(|p| p.key == source.key) {
                picked.push(source.clone());
            }
        }
        for category in categories {
            if category != crate::config::merge::DEFAULT_CATEGORY
                && !config.category.contains_key(category)
            {
                bail!("there's no category {category:?}");
            }
            for source in every.iter().filter(|s| &s.category == category) {
                if !picked.iter().any(|p| p.key == source.key) {
                    picked.push(source.clone());
                }
            }
        }
        if source_refs.is_empty() && categories.is_empty() {
            picked.clone_from(&every);
        }
        let keys: Vec<String> = picked.iter().map(|s| s.key.clone()).collect();
        let mut videos = crate::state::items::downloaded(&state.conn, &keys)?;
        if !video_ids.is_empty() {
            videos.retain(|v| video_ids.contains(&v.video_id));
            for id in video_ids {
                if !videos.iter().any(|v| &v.video_id == id) {
                    writeln!(out, "{id}: not downloaded in the chosen sources; skipped")?;
                }
            }
        }
        let everywhere = crate::state::items::downloaded(
            &state.conn,
            &every.iter().map(|s| s.key.clone()).collect::<Vec<_>>(),
        )?;
        Ok(Selected {
            sources: picked,
            videos,
            everywhere,
        })
    }

    async fn tools(&self, cmd: &ToolsCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
        use crate::cli::ToolName;
        use crate::tools::manager::Manager;

        let config = self.config_or_defaults()?;
        let tools = &config.tools;
        let manager = Manager::new(&self.dirs);
        let own = |name: &str, table: &str| {
            format!(
                "managed = false in [tools.{table}]: tidepool doesn't update your {name}; update it \
                 yourself, or set managed = true"
            )
        };
        match cmd {
            ToolsCmd::Status => {
                for name in [ToolName::Ytdlp, ToolName::Deno] {
                    print_tool_status(&manager, &ToolInfo::new(tools, name), out)?;
                }
            }
            ToolsCmd::Update { tool: requested } => {
                let names = match requested {
                    Some(name) => vec![*name],
                    None => vec![ToolName::Ytdlp, ToolName::Deno],
                };
                let mut failed = false;
                for name in names {
                    let ToolInfo {
                        tool,
                        is_managed,
                        table,
                        ..
                    } = ToolInfo::new(tools, name);
                    if !is_managed {
                        if requested.is_some() {
                            bail!("{}", own(tool.name(), table));
                        }
                        writeln!(
                            out,
                            "{} is your own (managed = false in [tools.{table}]); skipped",
                            tool.name()
                        )?;
                        continue;
                    }
                    if self.cli.dry_run {
                        writeln!(out, "dry run: would check for a newer {}", tool.name())?;
                        continue;
                    }
                    match manager.update(tool, jiff::Timestamp::now()).await {
                        Ok(outcome) => writeln!(out, "{outcome}")?,
                        Err(e) => {
                            failed = true;
                            writeln!(out, "{}: {e}", tool.name())?;
                        }
                    }
                }
                if failed {
                    return Ok(exit::PARTIAL);
                }
            }
            ToolsCmd::Rollback { tool: name } => {
                let ToolInfo {
                    tool,
                    is_managed,
                    table,
                    ..
                } = ToolInfo::new(tools, *name);
                if !is_managed {
                    bail!("{}", own(tool.name(), table));
                }
                let (from, to) = manager.rollback(tool)?;
                writeln!(
                    out,
                    "rolled back {} {from} → {to}; {from} won't be installed again until a newer release",
                    tool.name()
                )?;
            }
        }
        Ok(exit::OK)
    }

    /// `--config <path>`: import the file if it changed since the current revision.
    fn import_config_flag(&self) -> anyhow::Result<()> {
        let Some(path) = &self.cli.config else {
            return Ok(());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let name = Self::display_path(path);
        let state = self.open_state()?;
        let _lock = self.lock()?;
        match ConfigStore::new(&state.conn).import_if_changed(
            &name,
            &text,
            self.cli.config_flags.force,
        )? {
            Imported::Created(rev) => tracing::info!("imported {name} as config revision {rev}"),
            Imported::Unchanged(rev) => tracing::debug!("{name} matches config revision {rev}"),
        }
        Ok(())
    }

    /// Log environment findings, what passed at `ok_level`; returns the errors.
    fn report(findings: Vec<Finding>, ok_level: tracing::Level) -> usize {
        let mut errors = 0;
        for finding in findings {
            match finding {
                Finding::Ok(message) if ok_level == tracing::Level::INFO => {
                    tracing::info!("{message}");
                }
                Finding::Ok(message) => tracing::debug!("{message}"),
                Finding::Warning(message) => tracing::warn!("{message}"),
                Finding::Error(message) => {
                    errors += 1;
                    tracing::error!("{message}");
                }
            }
        }
        errors
    }

    fn init(&self, args: &InitArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let path = self
            .cli
            .config
            .clone()
            .unwrap_or_else(|| self.dirs.default_config_file());
        let name = Self::display_path(&path);
        let text = init::render();
        let loaded = load_str(&name, &text)?;

        if args.print || self.cli.dry_run {
            if self.cli.dry_run {
                tracing::info!("dry run: would write {name} and create the state database");
            }
            write!(out, "{text}")?;
            return Ok(exit::OK);
        }

        let stored = self.stored_config()?;
        if path.exists() && !args.force {
            match &stored {
                Some(current) => bail!(
                    "tidepool already has a config (revision {}) and {name} exists; \
                     to replace the file with a fresh one (keeping a backup), pass --force",
                    current.rev
                ),
                None => bail!(
                    "{name} already exists; import it with `tidepool config import {name}`, \
                     or pass --force to replace it with a fresh one (keeping a backup)"
                ),
            }
        }

        if !path.exists()
            && let Some(current) = &stored
        {
            write_private_file(&path, &current.toml)?;
            writeln!(
                out,
                "Wrote the stored config (revision {rev}) to {name}\n\
                 It's the config tidepool already uses; edit it and apply your changes with \
                 `tidepool config import{import_arg}`",
                rev = current.rev,
                import_arg = self.import_arg(&path),
            )?;
            return Ok(exit::OK);
        }

        let replaced = if path.exists() {
            let old =
                std::fs::read_to_string(&path).with_context(|| format!("cannot read {name}"))?;
            let backup = write_backup(&path, &old, jiff::Timestamp::now())?;
            replace_private_file(&path, &text)?;
            let backup_name = Self::display_path(&backup);
            writeln!(out, "Backed up {name} to {backup_name}")?;
            writeln!(out, "Wrote a fresh config to {name}\n")?;
            let diff = text_diff(&mask_toml(&old), &mask_toml(&text), &backup_name, &name);
            write!(out, "{}", colorize_diff(&diff))?;
            true
        } else {
            write_private_file(&path, &text)?;
            false
        };

        if let Some(current) = &stored {
            writeln!(
                out,
                "\nThe stored config (revision {rev}) is unchanged; apply the new file with \
                 `tidepool config import{import_arg}` (until then, runs stop at the config check)",
                rev = current.rev,
                import_arg = self.import_arg(&path),
            )?;
            return Ok(exit::OK);
        }

        let state = self.open_state()?;
        let _lock = self.lock()?;
        let rev = ConfigStore::new(&state.conn)
            .import(Some(&name), &text, &loaded)?
            .rev();
        let errors = Self::report(
            check_environment(&loaded.config, &self.dirs, Some(&path)),
            tracing::Level::INFO,
        );

        if replaced {
            writeln!(out)?;
        } else {
            writeln!(out, "Created {name}")?;
        }
        self.print_next_steps(&name, rev, &loaded, errors, out)?;
        Ok(exit::OK)
    }

    /// The config revision in use, if any.
    fn stored_config(&self) -> anyhow::Result<Option<Revision>> {
        if !self.paths.state_db.exists() {
            return Ok(None);
        }
        let state = self.open_state()?;
        Ok(ConfigStore::new(&state.conn).current()?)
    }

    /// The path argument `config import` needs for `path` (none for the default file).
    fn import_arg(&self, path: &Path) -> String {
        if path == self.dirs.default_config_file() {
            String::new()
        } else {
            format!(" {}", Self::display_path(path))
        }
    }

    /// The file `config import` reads: the given path, else `--config`, else
    /// the conventional config file. It must exist.
    fn import_path(&self, path: Option<&Path>) -> anyhow::Result<PathBuf> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| self.cli.config.clone())
            .unwrap_or_else(|| self.dirs.default_config_file());
        if !path.exists() {
            bail!(
                "no config file at {}; create one with `tidepool init`, or pass the path to import",
                Self::display_path(&path)
            );
        }
        Ok(path)
    }

    fn print_next_steps(
        &self,
        name: &str,
        rev: i64,
        loaded: &Loaded,
        errors: usize,
        out: &mut dyn Write,
    ) -> anyhow::Result<()> {
        let config = &loaded.config;
        writeln!(out, "Stored it as config revision {rev}")?;
        let default =
            crate::config::merge::category_settings(config, crate::config::merge::DEFAULT_CATEGORY);
        if let Ok(root) = media_dir(&default, &self.dirs) {
            writeln!(out, "Videos go to {}", root.display())?;
        }
        if errors > 0 {
            writeln!(out, "\nFix the problems reported above before syncing.")?;
        }
        writeln!(
            out,
            "\nThe config includes a sample source with one short video. Next steps:\n\
             \x20 1. Edit {name}: replace the sample with your sources, set where videos go.\n\
             \x20 2. Apply your changes: tidepool config import{import_path}\n\
             \x20 3. Sync once: tidepool run\n\
             \x20    Or keep syncing every {interval}: tidepool run --daemon",
            import_path = self.import_arg(Path::new(name)),
            interval = crate::config::types::format_duration(
                config.schedule.interval.0.unwrap_or_default()
            ),
        )?;
        let exe =
            std::env::current_exe().map_or_else(|_| "tidepool".into(), |p| p.display().to_string());
        writeln!(
            out,
            "\nTo run it as a service, save this as ~/.config/systemd/user/tidepool.service\n\
             and run `systemctl --user enable --now tidepool`:\n\n\
             [Unit]\nDescription=tidepool\n\n\
             [Service]\nExecStart={exe} run --daemon --config {name} --on-config-mismatch import\n\
             Restart=on-failure\n\n\
             [Install]\nWantedBy=default.target"
        )?;
        Ok(())
    }

    pub(crate) fn open_state(&self) -> anyhow::Result<State> {
        Ok(State::open(&self.paths.state_db)?)
    }

    /// The library lock: this process's own, or a share of the daemon's.
    pub(crate) fn lock(&self) -> Result<lock::Hold, LockError> {
        match &self.library {
            Some(shared) => shared.acquire().map(lock::Hold::Shared),
            None => lock::acquire(&self.paths.lock).map(lock::Hold::Own),
        }
    }

    pub(crate) fn display_path(path: &Path) -> String {
        std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string()
    }

    fn warn(warnings: &[String]) {
        for warning in warnings {
            tracing::warn!("{warning}");
        }
    }

    /// The config to run with: the stored revision (`--config` was imported
    /// up front), or with `--dry-run`, the `--config` file without storing it.
    pub(crate) fn current_config(&self, state: &State) -> anyhow::Result<Loaded> {
        if let (Some(path), true) = (&self.cli.config, self.cli.dry_run) {
            let loaded = load_file(path)?;
            Self::warn(&loaded.warnings);
            return Ok(loaded);
        }
        let store = ConfigStore::new(&state.conn);
        let (_, loaded) = store.load_current().map_err(|err| match (&err, store.current()) {
            (crate::config::store::StoreError::Config(_), Ok(Some(current))) => anyhow::anyhow!(
                "the stored config (revision {}) no longer loads with this tidepool:\n{err}\n\
                 fix it in your config file and run `tidepool config import`",
                current.rev
            ),
            _ => err.into(),
        })?;
        Self::warn(&loaded.warnings);
        Ok(loaded)
    }

    fn config(&self, args: &ConfigArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let show = |text: &str| {
            if args.show_secrets {
                text.to_string()
            } else {
                mask_toml(text)
            }
        };
        match &args.cmd {
            None => {
                let state = self.open_state()?;
                let loaded = self.current_config(&state)?;
                let current = ConfigStore::new(&state.conn)
                    .current()?
                    .expect("loaded above");
                let text = if args.include_defaults {
                    crate::config::defaults::with_defaults(
                        &current.toml,
                        &loaded.config,
                        &self.dirs.default_media_dir().display().to_string(),
                    )
                    .map_err(anyhow::Error::msg)?
                } else {
                    current.toml.clone()
                };
                write!(out, "{}", show(&text))?;
            }
            Some(ConfigCmd::Schema) => write!(out, "{}", schema_json())?,
            Some(ConfigCmd::Check { path }) => {
                return self.config_check(path.as_deref());
            }
            Some(ConfigCmd::Import { path }) => {
                let path = &self.import_path(path.as_deref())?;
                let loaded = load_file(path)?;
                Self::warn(&loaded.warnings);
                let name = Self::display_path(path);
                if self.cli.dry_run {
                    writeln!(out, "{name} is valid (dry run: not imported)")?;
                    return Ok(exit::OK);
                }
                let state = self.open_state()?;
                let _lock = self.lock()?;
                let text = std::fs::read_to_string(path)?;
                match ConfigStore::new(&state.conn).import(Some(&name), &text, &loaded)? {
                    Imported::Created(rev) => writeln!(out, "imported {name} as revision {rev}")?,
                    Imported::Unchanged(rev) => {
                        writeln!(out, "unchanged: {name} matches revision {rev}")?;
                    }
                }
            }
            Some(ConfigCmd::Export { rev }) => {
                let state = self.open_state()?;
                let store = ConfigStore::new(&state.conn);
                let revision = if let Some(rev) = rev {
                    store.get(*rev)?
                } else {
                    self.current_config(&state)?;
                    store.current()?.expect("loaded above")
                };
                write!(out, "{}", show(&revision.toml))?;
            }
            Some(ConfigCmd::History) => {
                let state = self.open_state()?;
                let history = ConfigStore::new(&state.conn).history()?;
                if history.is_empty() {
                    writeln!(
                        out,
                        "no config revisions yet; run `tidepool config import <path>`"
                    )?;
                }
                Self::print_history(&history, out)?;
            }
            Some(ConfigCmd::Rollback { rev }) => {
                let state = self.open_state()?;
                if self.cli.dry_run {
                    let old = ConfigStore::new(&state.conn).get(*rev)?;
                    load_str(&format!("config revision {rev}"), &old.toml)?;
                    writeln!(out, "would roll back to revision {rev} (dry run)")?;
                    return Ok(exit::OK);
                }
                let _lock = self.lock()?;
                let new = ConfigStore::new(&state.conn).rollback(*rev)?;
                writeln!(out, "rolled back to revision {rev} as revision {new}")?;
            }
            Some(ConfigCmd::Diff { path }) => {
                return self.config_diff(path.as_deref(), args.show_secrets, out);
            }
        }
        Ok(exit::OK)
    }

    fn print_history(history: &[Revision], out: &mut dyn Write) -> anyhow::Result<()> {
        let current = history.last().map(|r| r.rev);
        for r in history {
            let marker = if Some(r.rev) == current { "*" } else { " " };
            let created = r.created_at.parse::<jiff::Timestamp>().map_or_else(
                |_| r.created_at.clone(),
                |t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string(),
            );
            writeln!(
                out,
                "{marker} {:>4}  {created:<23}  {:<8}  {}  {}",
                r.rev,
                r.source,
                &r.source_hash[..r.source_hash.len().min(12)],
                r.source_path.as_deref().unwrap_or("-"),
            )?;
        }
        Ok(())
    }

    /// `tidepool config check [path]`: the config file (the path, `--config`,
    /// the file last imported, or the default) and the stored config, each
    /// validated, and whether they differ. Environment checks and the
    /// effective config use the file when it's valid, else the stored one.
    fn config_check(&self, path: Option<&Path>) -> anyhow::Result<i32> {
        let state = if self.paths.state_db.exists() {
            Some(self.open_state()?)
        } else {
            None
        };
        let store = state.as_ref().map(|s| ConfigStore::new(&s.conn));
        let current = match &store {
            Some(store) => store.current()?,
            None => None,
        };
        let file_path = if let Some(path) = path
            .map(Path::to_path_buf)
            .or_else(|| self.cli.config.clone())
        {
            path
        } else {
            let last = match &store {
                Some(store) => store.last_import()?,
                None => None,
            };
            last.and_then(|r| r.source_path)
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .unwrap_or_else(|| self.dirs.default_config_file())
        };
        let name = Self::display_path(&file_path);
        let mut invalid = Vec::new();

        // The file.
        let text = std::fs::read_to_string(&file_path).ok();
        let file = match &text {
            None => {
                if path.is_some() || self.cli.config.is_some() {
                    bail!("cannot read {name}");
                }
                tracing::warn!("no config file at {name}");
                None
            }
            Some(text) => match load_str(&name, text) {
                Ok(loaded) => {
                    Self::warn(&loaded.warnings);
                    tracing::debug!("{name} is valid ({} sources)", loaded.config.sources.len());
                    Some(loaded)
                }
                Err(err) => {
                    tracing::error!("{err}");
                    invalid.push(name.clone());
                    None
                }
            },
        };

        // The stored config, and whether the file matches it.
        let stored = match &current {
            None => {
                tracing::warn!(
                    "nothing is stored yet; apply your config with `tidepool config import`"
                );
                None
            }
            Some(current) => {
                check_stored_config(current, &name, text.as_deref(), file.as_ref(), &mut invalid)
            }
        };

        let Some(loaded) = file.or(stored) else {
            bail!(
                "{} invalid; see above",
                invalid.join(" and ") + if invalid.len() > 1 { " are" } else { " is" }
            );
        };
        let env_failed = Self::report(
            check_environment(
                &loaded.config,
                &self.dirs,
                text.is_some().then_some(file_path.as_path()),
            ),
            tracing::Level::DEBUG,
        ) > 0;
        if !invalid.is_empty() {
            bail!(
                "{} invalid; see above",
                invalid.join(" and ") + if invalid.len() > 1 { " are" } else { " is" }
            );
        }
        if env_failed {
            bail!("the config is valid, but the environment checks above failed");
        }
        Ok(exit::OK)
    }

    fn config_diff(
        &self,
        path: Option<&Path>,
        show_secrets: bool,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let state = self.open_state()?;
        let store = ConfigStore::new(&state.conn);
        let current = store
            .current()?
            .ok_or(crate::config::store::StoreError::Empty)?;
        let file = match path {
            Some(path) => path.to_path_buf(),
            None => store
                .last_import()?
                .and_then(|r| r.source_path)
                .map(PathBuf::from)
                .context("no file to compare with: pass a path")?,
        };
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let name = Self::display_path(&file);

        let mask = |t: &str| {
            if show_secrets {
                t.to_string()
            } else {
                mask_toml(t)
            }
        };
        let mut stored: Value = serde_json::from_str(&current.json)?;
        match load_str(&name, &text) {
            Ok(loaded) => {
                let mut theirs = loaded.json;
                if !show_secrets {
                    mask_json(&mut stored);
                    mask_json(&mut theirs);
                }
                let changes = key_changes(&stored, &theirs);
                if changes.is_empty() && current.toml == text {
                    writeln!(
                        out,
                        "no differences between revision {} and {name}",
                        current.rev
                    )?;
                    return Ok(exit::OK);
                }
                if changes.is_empty() {
                    writeln!(out, "same settings; only comments or formatting differ")?;
                }
                for change in &changes {
                    writeln!(out, "{change}")?;
                }
            }
            Err(err) => writeln!(out, "{name} is not a valid config:\n{err}")?,
        }
        writeln!(out)?;
        let label = format!("revision {}", current.rev);
        write!(
            out,
            "{}",
            text_diff(&mask(&current.toml), &mask(&text), &label, &name)
        )?;
        Ok(exit::OK)
    }

    fn filter_test(
        &self,
        video: &str,
        source: Option<&SourceRef>,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = effective_sources(&loaded.config);
        let limit =
            usize::try_from(loaded.config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
        let now = jiff::Timestamp::now();

        // A video tidepool knows: everything stored about it, in each source.
        let only = match source {
            Some(r) => Some(resolve_source(r, &sources)?.key.clone()),
            None => None,
        };
        if explain_video(&state.conn, &loaded.config, video, only.as_deref(), out)? {
            return Ok(exit::OK);
        }

        // A title: only the include/exclude rules (no duration or date).
        let Some(source) = source else {
            bail!(
                "{video:?} isn't a video tidepool knows; to test it as a title, pass --source <name>"
            );
        };
        let source = resolve_source(source, &sources)?;
        let rules = RuleSet::compile(source, limit, now).map_err(anyhow::Error::msg)?;
        let rules = RuleSet {
            min_duration: None,
            max_duration: None,
            cutoff: None,
            ..rules
        };
        writeln!(out, "source {} ({})", source.key, source.ids().join(", "))?;
        write_rules(
            out,
            &rules,
            &Fields {
                title: Some(video),
                ..Fields::default()
            },
        )?;
        Ok(exit::OK)
    }
}

/// The last line of a one-shot download: what got downloaded, skipped or
/// failed.
fn print_download_summary(
    summary: &crate::download::Summary,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    use crate::download::human_bytes;

    if summary.interrupted.is_some() {
        writeln!(
            out,
            "stopped; unfinished downloads will resume on the next run"
        )?;
    } else if summary.downloaded + summary.skipped + summary.failed == 0 {
        if summary.hooks_failed > 0 {
            writeln!(
                out,
                "nothing to download; {} hook{} failed",
                summary.hooks_failed,
                if summary.hooks_failed == 1 { "" } else { "s" }
            )?;
        } else {
            writeln!(out, "nothing to download")?;
        }
    } else {
        let videos = if summary.downloaded == 1 {
            "video"
        } else {
            "videos"
        };
        let mut parts = vec![format!(
            "downloaded {} {videos} ({})",
            summary.downloaded,
            human_bytes(summary.bytes)
        )];
        if summary.skipped > 0 {
            parts.push(format!("{} skipped", summary.skipped));
        }
        if summary.failed > 0 {
            parts.push(format!("{} failed", summary.failed));
        }
        if summary.hooks_failed > 0 {
            parts.push(format!(
                "{} hook{} failed",
                summary.hooks_failed,
                if summary.hooks_failed == 1 { "" } else { "s" }
            ));
        }
        writeln!(out, "{}", parts.join("; "))?;
    }
    Ok(())
}

/// Whether `tidepool hooks run` runs hook `name`: `prepare` hooks only make
/// sense before a video's files exist, and `before_cleanup` hooks are asked
/// by cleanup.
fn reruns_hook(config: &crate::config::Config, name: &str) -> bool {
    config.hooks.get(name).is_some_and(|h| {
        !matches!(
            h.kind,
            crate::config::HookType::Prepare | crate::config::HookType::BeforeCleanup
        )
    })
}

/// Fail if a hook `tidepool hooks run --hook` names doesn't exist or isn't
/// one it runs.
fn check_rerun_hooks(config: &crate::config::Config, names: &[String]) -> anyhow::Result<()> {
    for name in names {
        if let Some(hook) = config.hooks.get(name)
            && !reruns_hook(config, name)
        {
            bail!(
                "hook {name:?} is a {} hook, which `hooks run` doesn't run: {}",
                hook.kind.as_str(),
                if hook.kind == crate::config::HookType::Prepare {
                    "it only runs for new downloads"
                } else {
                    "`tidepool cleanup` asks it"
                }
            );
        }
        if !config.hooks.contains_key(name) {
            let known: Vec<&str> = config.hooks.keys().map(String::as_str).collect();
            bail!(
                "there's no hook {name:?} (hooks: {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            );
        }
    }
    Ok(())
}

/// What retrying `names` on a video did, and which of them gave up.
fn retry_line(
    state: &State,
    label: &str,
    source_key: &str,
    video_id: &str,
    names: &[String],
    problems: &[String],
) -> String {
    let gave_up: Vec<&str> = names
        .iter()
        .filter(|h| {
            crate::state::items::hook_status(&state.conn, source_key, video_id, h)
                .ok()
                .flatten()
                .as_deref()
                == Some("failed")
        })
        .map(String::as_str)
        .collect();
    let mut line = format!("{label}: tried again: {}", problems.join("; "));
    if !gave_up.is_empty() {
        let _ = write!(
            line,
            " (no more retries for {}; `tidepool hooks run {video_id} --failed` tries again)",
            gave_up.join(", ")
        );
    }
    line
}

/// What one `tidepool cleanup` works with, and how it's going.
struct CleanupRun<'a> {
    app: &'a App<'a>,
    state: &'a State,
    config: &'a crate::config::Config,
    now: jiff::Timestamp,
    tracker: crate::problems::Tracker,
    removed: usize,
    freed: u64,
    failed: usize,
    /// Something went to the trash.
    trashed: bool,
}

impl CleanupRun<'_> {
    /// Remove a video cleanup picked (to `trash_root`, or deleted), unless a
    /// hook still has to run on it or a `before_cleanup` hook keeps it.
    /// Prints what happened.
    async fn clean_up_video(
        &mut self,
        source: &EffectiveSource,
        k: &crate::state::items::Kept,
        reason: &crate::cleanup::Reason,
        label: &str,
        trash_root: &Path,
        out: &mut dyn Write,
    ) -> anyhow::Result<()> {
        use crate::cleanup::{files_to_remove, remove};
        use crate::state::items;

        let (state, now) = (self.state, self.now);
        // A `finished` hook may still be working with the files.
        let waiting = items::waiting_hooks(&state.conn, &source.key, &k.video_id)?;
        if !waiting.is_empty() {
            writeln!(
                out,
                "{label}: kept for now: hook {} hasn't run yet",
                waiting.join(", ")
            )?;
            return Ok(());
        }
        if let Some(why) = self
            .app
            .ask_before_cleanup(
                state,
                self.config,
                source,
                k,
                label,
                reason,
                &mut self.tracker,
            )
            .await?
        {
            writeln!(out, "{label}: kept: {why}")?;
            return Ok(());
        }
        let paths = files_to_remove(&k.files, source.cleanup.keep);
        let trash = (source.cleanup.delete == crate::config::CleanupDelete::Trash
            && !paths.is_empty())
        .then(|| {
            trash_root.join(&source.key).join(format!(
                "{}-{}",
                k.video_id,
                now.strftime("%Y%m%dT%H%M%S")
            ))
        });
        match remove(&paths, Path::new(&k.files.dir), trash.as_deref()) {
            Ok((moved, bytes)) => {
                if let Some(dir) = &trash {
                    let until = now
                        .checked_add(
                            jiff::SignedDuration::try_from(source.cleanup.trash_for.0)
                                .unwrap_or(jiff::SignedDuration::ZERO),
                        )
                        .unwrap_or(now);
                    items::add_trash(
                        &state.conn,
                        &items::Trashed {
                            id: 0,
                            source_key: source.key.clone(),
                            video_id: k.video_id.clone(),
                            dir: dir.display().to_string(),
                            files: moved,
                            bytes,
                            reason: reason.to_string(),
                            trashed_at: now.to_string(),
                            delete_after: until.to_string(),
                        },
                    )?;
                    self.trashed = true;
                }
                items::mark_cleaned_up(
                    &state.conn,
                    &source.key,
                    &k.video_id,
                    &reason.to_string(),
                    now,
                )?;
                let how = if trash.is_some() {
                    "to the trash"
                } else {
                    "deleted"
                };
                writeln!(
                    out,
                    "{label}: {how} ({reason}), {}",
                    crate::download::human_bytes(bytes)
                )?;
                self.removed += 1;
                self.freed += bytes;
            }
            Err(e) => {
                self.failed += 1;
                writeln!(out, "{label}: {e}")?;
            }
        }
        Ok(())
    }

    /// The last line: what was (or would be) removed.
    fn summary(&self, dry_run: bool) -> String {
        use crate::download::human_bytes;

        let (removed, failed) = (self.removed, self.failed);
        let videos = if removed == 1 { "video" } else { "videos" };
        if removed == 0 && failed == 0 {
            "nothing to clean up".to_string()
        } else if dry_run {
            format!(
                "would remove {removed} {videos} ({})",
                human_bytes(self.freed)
            )
        } else {
            let mut line = format!("removed {removed} {videos} ({})", human_bytes(self.freed));
            if failed > 0 {
                let _ = write!(line, "; {failed} couldn't be removed");
            }
            if self.trashed {
                line.push_str("; `tidepool trash restore <video_id>` brings one back");
            }
            line
        }
    }
}

/// A source's downloaded videos as cleanup sees them.
fn cleanup_candidates(
    source: &EffectiveSource,
    kept: &[crate::state::items::Kept],
    details: &std::collections::HashMap<String, crate::index::RemoteItem>,
    rules: &RuleSet,
) -> Vec<crate::cleanup::Candidate> {
    let marker = &source.cleanup.marker_file;
    kept.iter()
        .map(|k| {
            let video = details.get(&k.video_id);
            let unmatched = video.and_then(|v| match rules.decide(&Fields::from_item(v)) {
                crate::filter::Decision::Keep { .. }
                | crate::filter::Decision::Deferred { .. }
                | crate::filter::Decision::FilterError { .. } => None,
                other => Some(other.to_string()),
            });
            crate::cleanup::Candidate {
                video_id: k.video_id.clone(),
                position: k.position,
                bytes: k.media_bytes,
                downloaded_at: k.downloaded_at,
                published_at: video.and_then(|v| v.published_at),
                present: !k.files.is_removed("media") && Path::new(&k.files.media).exists(),
                unmatched,
                marked: !marker.is_empty() && Path::new(&k.files.dir).join(marker).exists(),
            }
        })
        .collect()
}

/// `tidepool cleanup --dry-run`'s line for a source: how many videos would
/// go and how many stay.
fn cleanup_tally(
    key: &str,
    kept: &[crate::state::items::Kept],
    candidates: &[crate::cleanup::Candidate],
    victims: &[(usize, crate::cleanup::Reason)],
) -> String {
    use crate::download::human_bytes;

    format!(
        "{key}: {} {}: {}, {} stay ({})",
        kept.len(),
        if kept.len() == 1 { "video" } else { "videos" },
        if victims.is_empty() {
            "none would be removed".to_string()
        } else {
            format!(
                "{} would be removed ({})",
                victims.len(),
                human_bytes(victims.iter().map(|(i, _)| kept[*i].media_bytes).sum())
            )
        },
        kept.len() - victims.len(),
        human_bytes(
            (0..kept.len())
                .filter(|i| candidates[*i].present && !victims.iter().any(|(v, _)| v == i))
                .map(|i| kept[i].media_bytes)
                .sum()
        )
    )
}

/// A tool as `tidepool tools` describes it.
struct ToolInfo<'a> {
    tool: crate::tools::manager::Tool,
    /// tidepool installs and updates it.
    is_managed: bool,
    /// Your own path to it.
    path: &'a str,
    update_check: std::time::Duration,
    /// What it's for.
    what: String,
    /// Its `[tools.*]` table.
    table: &'static str,
}

impl<'a> ToolInfo<'a> {
    fn new(tools: &'a crate::config::types::ToolsConfig, name: crate::cli::ToolName) -> Self {
        use crate::cli::ToolName;
        use crate::tools::manager::Tool;

        match name {
            ToolName::Ytdlp => Self {
                tool: Tool::Ytdlp(tools.ytdlp.channel),
                is_managed: tools.ytdlp.managed,
                path: tools.ytdlp.path.as_str(),
                update_check: tools.ytdlp.update_check.0,
                what: format!("{} channel", tools.ytdlp.channel.as_str()),
                table: "yt-dlp",
            },
            ToolName::Deno => Self {
                tool: Tool::Deno,
                is_managed: tools.deno.managed,
                path: tools.deno.path.as_str(),
                update_check: tools.deno.update_check.0,
                what: "2.x, the JavaScript runtime yt-dlp needs for YouTube".to_string(),
                table: "deno",
            },
        }
    }
}

/// `tidepool tools status` for one tool.
fn print_tool_status(
    manager: &crate::tools::manager::Manager,
    info: &ToolInfo<'_>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    use crate::tools::manager::Tool;

    let when = |t: jiff::Timestamp| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string();
    let ToolInfo {
        tool,
        path,
        table,
        what,
        ..
    } = info;
    let label = tool.name();
    if !info.is_managed {
        match crate::tools::manager::tool_version(Path::new(path)) {
            Ok(line) => {
                // yt-dlp prints "2026.10.01"; Deno "deno 2.9.8 (…)".
                let version = match tool {
                    Tool::Deno => line.split_whitespace().nth(1).unwrap_or(&line),
                    Tool::Ytdlp(_) => &line,
                };
                writeln!(
                    out,
                    "{label} {version} at {path} (your own; tidepool doesn't update it)"
                )?;
            }
            Err(e) => writeln!(
                out,
                "{label}: cannot run {path:?} (tools.{table}.path): {e}"
            )?,
        }
        return Ok(());
    }
    let binary = manager.binary(*tool);
    let Some(state) = manager.state(*tool).filter(|_| binary.exists()) else {
        return writeln!(
            out,
            "{label} isn't downloaded yet (managed, {what}); it's downloaded on \
             first use, or run `tidepool tools update {table}`"
        );
    };
    writeln!(out, "{label} {} (managed, {what})", state.version)?;
    writeln!(out, "  path: {}", binary.display())?;
    writeln!(out, "  installed: {}", when(state.installed_at))?;
    writeln!(
        out,
        "  last update check: {} (checks every {})",
        when(state.last_check_at),
        crate::config::types::format_duration(info.update_check)
    )?;
    if let Some(previous) = &state.previous_version {
        writeln!(
            out,
            "  previous version: {previous} (`tidepool tools rollback {table}` restores it)"
        )?;
    }
    if let Some(skipped) = &state.skip_version {
        writeln!(
            out,
            "  {skipped} was rolled back; staying on {} until a newer release",
            state.version
        )?;
    }
    Ok(())
}

/// `tidepool config check`: validate the stored config `current`, and say
/// whether the file `name` (`text`, `file` when it loads) matches it.
/// Returns it loaded; an invalid one is added to `invalid`.
fn check_stored_config(
    current: &Revision,
    name: &str,
    text: Option<&str>,
    file: Option<&Loaded>,
    invalid: &mut Vec<String>,
) -> Option<Loaded> {
    let label = format!("config revision {}", current.rev);
    let loaded = match load_str(&label, &current.toml) {
        Ok(loaded) => Some(loaded),
        Err(err) => {
            tracing::error!(
                "the stored config (revision {}) no longer loads with this tidepool:\n{err}\n\
                 fix it in your config file and run `tidepool config import`",
                current.rev
            );
            invalid.push(format!("the stored config (revision {})", current.rev));
            None
        }
    };
    let same = text.is_some_and(|text| {
        crate::config::store::hash(text) == current.source_hash
            || file.is_some_and(|f| loaded.is_some() && f.normalized() == current.json)
    });
    if same {
        if loaded.is_some() {
            tracing::debug!(
                "the stored config (revision {}) is valid and matches {name}",
                current.rev
            );
        }
    } else if text.is_some() {
        tracing::warn!(
            "{name} and the stored config (revision {}) differ: run `tidepool config import` \
             to apply it (`tidepool config diff` shows what changed)",
            current.rev
        );
    }
    loaded
}

/// `tidepool clean-titles`' last line: how many were renamed, already
/// clean, or failed.
fn clean_titles_summary(dry_run: bool, renamed: usize, unchanged: usize, failed: usize) -> String {
    let videos = if renamed == 1 { "video" } else { "videos" };
    let verb = if dry_run { "would rename" } else { "renamed" };
    format!(
        "{verb} {renamed} {videos}; {unchanged} already clean{}",
        if failed > 0 {
            format!("; {failed} failed")
        } else {
            String::new()
        }
    )
}

/// The position in its source a video's metadata file records.
fn source_position(meta: &Value) -> u32 {
    meta["source"]["position"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .unwrap_or(0)
}

/// A video's metadata with its new title (keeping YouTube's as the
/// original) and its files' new places.
fn retitled_metadata(
    meta: &Value,
    cleaned: &str,
    youtube: &str,
    new: &crate::state::items::VideoFiles,
) -> Value {
    let mut meta = meta.clone();
    meta["title"] = cleaned.into();
    if meta["original"]["title"].is_null() {
        if !meta["original"].is_object() {
            meta["original"] = serde_json::json!({});
        }
        meta["original"]["title"] = youtube.into();
    }
    meta["files"] = crate::download::metadata::files_json(new);
    meta
}

/// Move a retitled video's media, cover, subtitles and raw info to their new
/// places, then write its metadata file at its new place. All or nothing:
/// on failure, what moved is put back, and the error says why (`skipped:` or
/// `failed:`).
fn move_retitled(
    files: &crate::state::items::VideoFiles,
    new: &crate::state::items::VideoFiles,
    metadata_dest: &Path,
    meta: &Value,
) -> Result<(), String> {
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut pairs: Vec<(Option<&String>, Option<&String>)> = vec![
        (Some(&files.media), Some(&new.media)),
        (files.cover.as_ref(), new.cover.as_ref()),
        (files.raw_info.as_ref(), new.raw_info.as_ref()),
    ];
    for (old, now) in files.subtitles.iter().zip(&new.subtitles) {
        pairs.push((Some(&old.path), Some(&now.path)));
    }
    for (old, now) in pairs {
        if let (Some(old), Some(now)) = (old, now)
            && old != now
            && Path::new(old).exists()
        {
            moves.push((PathBuf::from(old), PathBuf::from(now)));
        }
    }
    if let Some((_, taken)) = moves.iter().find(|(_, to)| to.exists()) {
        return Err(format!("skipped: {} already exists", taken.display()));
    }
    if files.metadata != new.metadata && metadata_dest.exists() {
        return Err(format!(
            "skipped: {} already exists",
            metadata_dest.display()
        ));
    }
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut problem = None;
    for (from, to) in &moves {
        match crate::download::place(from, to) {
            Ok(()) => done.push((from.clone(), to.clone())),
            Err(e) => {
                problem = Some(format!("cannot move {}: {e}", from.display()));
                break;
            }
        }
    }
    if problem.is_none() {
        let text = serde_json::to_string_pretty(meta).expect("JSON") + "\n";
        if let Err(e) = std::fs::create_dir_all(metadata_dest.parent().unwrap_or(Path::new("/")))
            .and_then(|()| std::fs::write(metadata_dest, text))
        {
            problem = Some(format!("cannot write {}: {e}", metadata_dest.display()));
        }
    }
    if let Some(problem) = problem {
        // Put back what moved.
        for (from, to) in done.iter().rev() {
            let _ = crate::download::place(to, from);
        }
        return Err(format!("failed: {problem}"));
    }
    if files.metadata != new.metadata {
        let _ = std::fs::remove_file(&files.metadata);
    }
    if files.dir != new.dir {
        let _ = std::fs::remove_dir(&files.dir);
    }
    Ok(())
}

/// Everything stored about a known video, and each rule's verdict in each
/// source it's in (or only in `only`). `false` when tidepool doesn't know it.
pub(crate) fn explain_video(
    conn: &rusqlite::Connection,
    config: &crate::config::Config,
    video: &str,
    only: Option<&str>,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    use crate::state::items;

    let known = items::videos(conn, &[video.to_string()])?;
    let Some(item) = known.get(video) else {
        return Ok(false);
    };
    let sources = effective_sources(config);
    let limit = usize::try_from(config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
    let now = jiff::Timestamp::now();
    let statuses = items::item_statuses(conn, video)?;
    let picked: Vec<&EffectiveSource> = sources
        .iter()
        .filter(|s| match only {
            Some(key) => s.key == key,
            None => statuses.iter().any(|(key, _)| *key == s.key),
        })
        .collect();
    describe_video(out, item)?;
    if picked.is_empty() {
        writeln!(out, "in no configured source; pass --source to test one")?;
    }
    for source in picked {
        let rules = RuleSet::compile(source, limit, now).map_err(anyhow::Error::msg)?;
        let status = statuses
            .iter()
            .find(|(key, _)| *key == source.key)
            .map_or_else(
                || "not in this source".into(),
                |(_, status)| status.replace('_', " "),
            );
        writeln!(out, "\nsource {} ({status})", source.key)?;
        write_rules(out, &rules, &Fields::from_item(item))?;
    }
    Ok(true)
}

/// What's stored about a video, the fields rules look at.
fn describe_video(out: &mut dyn Write, item: &crate::index::RemoteItem) -> std::io::Result<()> {
    writeln!(out, "{} “{}”", item.video_id, item.title)?;
    let unknown = || "not known yet".to_string();
    let kind = item.kind.map_or_else(
        || "not known yet (checked before download)".into(),
        |k| k.as_str().to_string(),
    );
    // 4m26s, 1h2m3s.
    let duration = item.duration.map_or_else(unknown, |d| {
        let (h, m, s) = (d / 3600, d % 3600 / 60, d % 60);
        match (h, m) {
            (0, 0) => format!("{s}s"),
            (0, _) => format!("{m}m{s}s"),
            _ => format!("{h}h{m}m{s}s"),
        }
    });
    let published = item
        .published_at
        .map_or_else(unknown, |t| t.strftime("%Y-%m-%d").to_string());
    writeln!(
        out,
        "  channel: {}",
        item.channel.as_deref().unwrap_or("not known yet")
    )?;
    writeln!(out, "  kind: {kind}")?;
    writeln!(out, "  duration: {duration}")?;
    writeln!(out, "  published: {published}")?;
    writeln!(out, "  availability: {}", item.availability.as_str())?;
    match &item.tags {
        Some(tags) if !tags.is_empty() => writeln!(out, "  tags: {}", tags.join(", "))?,
        Some(_) => writeln!(out, "  tags: none")?,
        None => writeln!(out, "  tags: not known yet")?,
    }
    let description = match item.description.as_deref() {
        None => "not known yet".to_string(),
        Some("") => "none".to_string(),
        Some(d) => {
            let first = d.lines().next().unwrap_or_default();
            if d.lines().count() > 1 || first.chars().count() > 80 {
                format!("{}…", first.chars().take(80).collect::<String>())
            } else {
                first.to_string()
            }
        }
    };
    writeln!(out, "  description: {description}")
}

/// Each rule's result for `item`, then the decision.
fn write_rules(out: &mut dyn Write, rules: &RuleSet, item: &Fields<'_>) -> std::io::Result<()> {
    if rules.include.is_empty() && rules.exclude.is_empty() {
        writeln!(out, "  no include/exclude rules")?;
    }
    for rule in rules.include.iter().chain(&rules.exclude) {
        let fields: Vec<String> = rule
            .fields
            .iter()
            .map(|f| format!("{f:?}").to_lowercase())
            .collect();
        let result = match rule.test(item) {
            Match::Yes => "match".to_string(),
            Match::No => "no match".to_string(),
            Match::Unknown(field) => format!(
                "not evaluated (needs {})",
                format!("{field:?}").to_lowercase()
            ),
            Match::Error(e) => format!("error: {e}"),
        };
        writeln!(
            out,
            "  {}[{}] {:?} on {}: {result}",
            rule.list,
            rule.index,
            rule.source,
            fields.join(", ")
        )?;
    }
    writeln!(out, "decision: {}", rules.decide(item))
}

/// Find the configured source a `--source` value refers to.
///
/// # Errors
///
/// A [`UsageError`] if no configured source has that key or ID.
pub fn resolve_source<'a>(
    r: &SourceRef,
    sources: &'a [EffectiveSource],
) -> anyhow::Result<&'a EffectiveSource> {
    let found = match r {
        SourceRef::Key(key) => sources.iter().find(|p| &p.key == key),
        SourceRef::Id(id) | SourceRef::Url { id, .. } => {
            sources.iter().find(|p| p.ids().contains(id))
        }
    };
    found.ok_or_else(|| {
        let known: Vec<&str> = sources.iter().map(|p| p.key.as_str()).collect();
        let known = if known.is_empty() { "none".to_string() } else { known.join(", ") };
        UsageError(format!(
            "--source {:?} matches no configured source (configured: {known}); add it under [sources.<name>] in your config",
            r.raw()
        ))
        .into()
    })
}

/// Write `text` to a new file at `path` with owner-only permissions.
fn write_private_file(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Replace the file at `path` with `text` (owner-only), through a rename.
fn replace_private_file(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    write_private_file(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Save `text` next to `path` as `<stem>.<UTC timestamp>.bak`, numbering it
/// (`-2`, `-3`, …) rather than overwriting an earlier backup.
fn write_backup(path: &Path, text: &str, now: jiff::Timestamp) -> anyhow::Result<PathBuf> {
    let stem = path
        .file_stem()
        .map_or_else(|| "config".into(), |s| s.to_string_lossy().into_owned());
    let stamp = now.strftime("%Y%m%dT%H%M%SZ").to_string();
    for n in 1.. {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let backup = path.with_file_name(format!("{stem}.{stamp}{suffix}.bak"));
        match write_private_file(&backup, text) {
            Ok(()) => return Ok(backup),
            Err(e)
                if e.root_cause()
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Colour a unified diff when stdout is a terminal (and `NO_COLOR` is unset).
fn colorize_diff(diff: &str) -> String {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
        return diff.to_string();
    }
    diff.split_inclusive('\n')
        .fold(String::with_capacity(diff.len()), |mut out, line| {
            let color = if line.starts_with("+++") || line.starts_with("---") {
                "1"
            } else if line.starts_with('+') {
                "32"
            } else if line.starts_with('-') {
                "31"
            } else if line.starts_with("@@") {
                "36"
            } else {
                out.push_str(line);
                return out;
            };
            let body = line.trim_end_matches('\n');
            let end = &line[body.len()..];
            let _ = write!(out, "\x1b[{color}m{body}\x1b[0m{end}");
            out
        })
}

/// `-v`: which yt-dlp runs.
fn log_ytdlp(path: &Path, how: &str) {
    if tracing::enabled!(tracing::Level::DEBUG) {
        let version = crate::tools::manager::tool_version(path)
            .unwrap_or_else(|e| format!("(not runnable: {e})"));
        tracing::debug!("yt-dlp {version} at {} ({how})", path.display());
    }
}

/// The downloaded videos a `hooks run` or `embed` works on.
struct Selected {
    /// The chosen sources.
    sources: Vec<EffectiveSource>,
    videos: Vec<crate::state::items::Downloaded>,
    /// Every downloaded video of every source, to keep linked copies in step.
    everywhere: Vec<crate::state::items::Downloaded>,
}

impl Selected {
    fn source(&self, key: &str) -> &EffectiveSource {
        self.sources
            .iter()
            .find(|s| s.key == key)
            .expect("a chosen source")
    }
}

/// Running hooks or embedding again over chosen videos, and recording what
/// happened.
struct Rerun<'a> {
    state: &'a State,
    selected: &'a Selected,
    shared: crate::download::rerun::Shared<'a>,
    tracker: &'a mut crate::problems::Tracker,
    /// These are automatic retries of failed `finished` hooks.
    retry: bool,
    /// What embedding did for the last video.
    last_embedding: Option<crate::download::stages::Embedding>,
}

impl<'a> Rerun<'a> {
    fn new(
        app: &'a App<'_>,
        state: &'a State,
        config: &'a crate::config::Config,
        selected: &'a Selected,
        mode: crate::download::rerun::Mode,
        tracker: &'a mut crate::problems::Tracker,
    ) -> Self {
        Self {
            state,
            selected,
            shared: crate::download::rerun::Shared {
                mode,
                hooks: &config.hooks,
                hook_logs: &app.paths.hook_logs,
                dirs: &app.dirs,
                ffmpeg: Path::new("ffmpeg"),
                ffprobe: Path::new("ffprobe"),
                duplicates: config.general.duplicates,
                shutdown: &app.shutdown,
                grace: config.general.shutdown_timeout.0,
            },
            tracker,
            retry: false,
            last_embedding: None,
        }
    }

    /// Run `hook_names` (or embedding) for one video; returns its title and
    /// what went wrong.
    async fn video(
        &mut self,
        item: &crate::state::items::Downloaded,
        source: &EffectiveSource,
        hook_names: &[String],
    ) -> anyhow::Result<(String, Vec<String>)> {
        let dirs = self.shared.dirs;
        let media_dir = media_dir(source, dirs).map_err(anyhow::Error::msg)?;
        let tmp_root = tmp_dir(source, dirs).map_err(anyhow::Error::msg)?;
        let others: Vec<crate::state::items::VideoFiles> = self
            .selected
            .everywhere
            .iter()
            .filter(|o| o.video_id == item.video_id && o.source_key != item.source_key)
            .map(|o| o.files.clone())
            .collect();
        let result = crate::download::rerun::rerun(
            &self.shared,
            &crate::download::rerun::Video {
                item,
                source,
                hook_names,
                media_dir: &media_dir,
                tmp_root: &tmp_root,
                others: &others,
            },
        )
        .await;
        let conn = &self.state.conn;
        if let Some((files, bytes)) = &result.files {
            crate::state::items::update_files(
                conn,
                &item.source_key,
                &item.video_id,
                files,
                *bytes,
            )?;
        }
        let mut problems = Vec::new();
        for report in &result.hook_runs {
            crate::state::items::record_hook_run(
                conn,
                &crate::download::hook_run(&item.source_key, &item.video_id, report, self.retry),
            )?;
            match &report.result {
                Ok(_) => self.tracker.hook_succeeded(),
                Err(e) if e == "stopped" => problems.push("stopped".into()),
                Err(e) => {
                    self.tracker.hook_failed(&item.source_key, &report.name, e);
                    problems.push(format!("hook {} failed: {e}", report.name));
                }
            }
        }
        if let Some(e) = &result.error
            && !problems.iter().any(|p| p == e)
        {
            problems.push(e.clone());
        }
        self.last_embedding = result.embedding;
        Ok((result.title, problems))
    }
}

/// `embedded cover, chapters; removed subtitles`, or `nothing changed`.
fn describe_embedding(embedding: &crate::download::stages::Embedding) -> String {
    let names = |e: &crate::download::embed::Embedded| {
        [
            (e.metadata, "tags"),
            (e.cover, "cover"),
            (e.chapters, "chapters"),
            (e.subtitles, "subtitles"),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| name)
        .collect::<Vec<_>>()
        .join(", ")
    };
    let mut parts = Vec::new();
    if embedding.embedded.any() {
        parts.push(format!("embedded {}", names(&embedding.embedded)));
    }
    if embedding.removed.any() {
        parts.push(format!("removed {}", names(&embedding.removed)));
    }
    if parts.is_empty() {
        "nothing changed".into()
    } else {
        parts.join("; ")
    }
}

/// Delete a video's folder in the trash; `false` (and a warning) if it
/// can't be.
fn delete_trashed(t: &crate::state::items::Trashed) -> bool {
    match std::fs::remove_dir_all(&t.dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!("cannot delete {}: {e}", t.dir);
            return false;
        }
    }
    // The source's folder in the trash, once it's empty.
    if let Some(parent) = Path::new(&t.dir).parent() {
        let _ = std::fs::remove_dir(parent);
    }
    true
}
