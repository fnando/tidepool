//! Command dispatch: turns a parsed [`Cli`] into work and an exit code.

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
use crate::lock::{self, LibraryLock};
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

struct App<'a> {
    cli: &'a Cli,
    dirs: Dirs,
    paths: StatePaths,
    /// Ctrl-C and `kill`, for commands that run yt-dlp, ffmpeg or hooks.
    shutdown: crate::shutdown::Shutdown,
    /// A source couldn't use the API and `fallback_to_ytdlp = false` (exit 4).
    no_fallback: std::cell::Cell<bool>,
}

/// Run a parsed command; returns the process exit code.
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
        | Command::Cleanup(_) => crate::shutdown::Shutdown::listen(),
        _ => crate::shutdown::Shutdown::default(),
    };
    let app = App {
        cli,
        dirs,
        paths,
        shutdown,
        no_fallback: Default::default(),
    };
    if cli.config.is_some() && !cli.dry_run && uses_stored_config(&cli.command) {
        app.import_config_flag()?;
    }
    if !matches!(cli.command, Command::Problems { .. } | Command::Init(_)) {
        app.show_problem_notices()?;
    }
    if cli.config.is_none() && !cli.skip_config_check && checks_config_drift(&cli.command) {
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
        } => app.filter_test(title, source, out),
        Command::Tools { cmd } => app.tools(cmd, out).await,
        Command::Index(scope) => app.index(scope, out).await,
        Command::Download(args) => app.download(args, out).await,
        Command::Run(args) if !args.daemon => app.run_once(args, out).await,
        Command::Reset(args) => app.reset(args, out),
        Command::Problems { cmd } => app.problems(cmd, out),
        Command::Hooks { cmd } => app.hooks(cmd, out).await,
        Command::Embed(args) => app.embed(args, out).await,
        Command::Cleanup(scope) => app.cleanup(scope, out).await,
        Command::Trash { cmd } => app.trash(cmd, out),
        other => {
            let milestone = other.milestone().unwrap_or("a later milestone");
            bail!("this command is not implemented yet (planned for {milestone})")
        }
    }
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
            None | Some(ConfigCmd::Export { .. })
                | Some(ConfigCmd::History)
                | Some(ConfigCmd::Rollback { .. })
                | Some(ConfigCmd::Diff { .. })
                | Some(ConfigCmd::Check { path: None, .. })
        ),
        Command::Filter { .. }
        | Command::Tools { .. }
        | Command::Index(_)
        | Command::Download(_)
        | Command::Reset(_)
        | Command::Hooks { .. }
        | Command::Embed(_)
        | Command::Cleanup(_) => true,
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
        &self,
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
                tracing::debug!("using {} without storing it (dry run)", path.display())
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
                &Default::default(),
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
                    tracing::info!("{outcome}")
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
        let managed = config.tools.ytdlp.managed;
        if managed && tracker.wants_update() {
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
            managed,
            jiff::Timestamp::now(),
        )?;
        Ok(())
    }

    async fn index(&self, scope: &ScopeArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let mut state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = self.scoped_sources(&loaded.config, &scope.sources)?;
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
        use crate::download::{Context, Options, human_bytes};

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
        };
        let options = Options {
            limit,
            concurrency: concurrency.unwrap_or(config.general.concurrency) as usize,
            now: jiff::Timestamp::now(),
        };
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
        if summary.interrupted.is_none() {
            summary.hooks_failed += self
                .retry_hooks(state, config, sources, tracker, out)
                .await?;
            summary.interrupted = self.shutdown.code();
        }
        if summary.interrupted.is_none() && !self.cli.dry_run {
            self.purge_trash(state, jiff::Timestamp::now(), out)?;
        }

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
        Ok(summary)
    }

    /// Try again the failed `finished` hooks of `sources` that are due (see
    /// `items::hook_retry_wait`); returns how many failed again.
    async fn retry_hooks(
        &self,
        state: &State,
        config: &crate::config::Config,
        sources: &[EffectiveSource],
        tracker: &mut crate::problems::Tracker,
        out: &mut dyn Write,
    ) -> anyhow::Result<usize> {
        use crate::state::items;
        let due: Vec<(String, String, Vec<String>)> =
            items::due_hook_retries(&state.conn, jiff::Timestamp::now())?
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
            let (title, problems) = rerun.video(item, source, &names).await?;
            let label = format!("{source_key}: “{title}”");
            if problems.is_empty() {
                writeln!(out, "{label}: hook {} worked this time", names.join(", "))?;
                continue;
            }
            failed += 1;
            let gave_up: Vec<&str> = names
                .iter()
                .filter(|h| {
                    items::hook_status(&state.conn, source_key, video_id, h)
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some("failed")
                })
                .map(String::as_str)
                .collect();
            let mut line = format!("{label}: tried again: {}", problems.join("; "));
            if !gave_up.is_empty() {
                line.push_str(&format!(
                    " (no more retries for {}; `tidepool hooks run {video_id} --failed` tries again)",
                    gave_up.join(", ")
                ));
            }
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
        let sources = self.scoped_sources(&loaded.config, &args.scope.sources)?;
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
    async fn run_once(&self, args: &RunArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let mut state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = self.scoped_sources(&loaded.config, &args.sources)?;
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
        cmd: &Option<crate::cli::ProblemsCmd>,
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
        let mut stored: Value = serde_json::from_str(&current.json)?;
        let mut file = loaded.json;
        mask_json(&mut stored);
        mask_json(&mut file);
        let changes: Vec<String> = key_changes(&stored, &file)
            .iter()
            .map(|c| format!("  {c}"))
            .collect();
        let mut message = format!(
            "{name} has changes that aren't applied yet:\n{}\n{skip}",
            changes.join("\n")
        );
        if current.source != Source::Import {
            message.push_str(&format!(
                "\nNote: the stored config came from a {} (revision {}); to keep it instead, write it \
                 back to the file with `tidepool config export --show-secrets > {name}`.",
                current.source, current.rev
            ));
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
    fn reset(&self, args: &ResetArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
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

    /// `tidepool hooks run`: run hooks again on videos already downloaded.
    async fn hooks(&self, cmd: &HooksCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
        let HooksCmd::Run {
            select,
            hooks: only_hooks,
            failed: only_failed,
        } = cmd;
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        for name in only_hooks {
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
        let selected = self.select_downloaded(&state, &config, select, out)?;
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
                Default::default()
            };
            let hook_names: Vec<String> = source
                .hooks
                .iter()
                .filter(|h| config.hooks.contains_key(*h))
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
    async fn embed(
        &self,
        args: &crate::cli::EmbedArgs,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let selected = self.select_downloaded(&state, &config, &args.select, out)?;
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
    async fn cleanup(&self, scope: &ScopeArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        use crate::cleanup::{Candidate, files_to_remove, plan, remove};
        use crate::download::human_bytes;
        use crate::state::items;

        let dry_run = self.cli.dry_run;
        let state = self.open_state()?;
        let config = self.current_config(&state)?.config;
        let sources = self.scoped_sources(&config, &scope.sources)?;
        let _lock = if dry_run { None } else { Some(self.lock()?) };
        let now = jiff::Timestamp::now();
        if !dry_run {
            self.purge_trash(&state, now, out)?;
        }
        let limit = usize::try_from(config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
        let mut tracker = crate::problems::Tracker::default();
        let (mut removed, mut freed, mut failed) = (0usize, 0u64, 0usize);
        let mut trashed = false;
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
            let marker = &source.cleanup.marker_file;
            let candidates: Vec<Candidate> = kept
                .iter()
                .map(|k| {
                    let video = details.get(&k.video_id);
                    let unmatched = video.and_then(|v| match rules.decide(&Fields::from_item(v)) {
                        crate::filter::Decision::Keep { .. }
                        | crate::filter::Decision::Deferred { .. }
                        | crate::filter::Decision::FilterError { .. } => None,
                        other => Some(other.to_string()),
                    });
                    Candidate {
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
                .collect();
            let victims = plan(&source.cleanup, &candidates, now);
            // With --dry-run, a line per source: how many go and how many stay.
            let tally = format!(
                "{}: {} {}: {}, {} stay ({})",
                source.key,
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
            );
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
                    removed += 1;
                    freed += k.media_bytes;
                    continue;
                }
                if let Some(why) = self
                    .ask_before_cleanup(&state, &config, source, k, &label, &reason, &mut tracker)
                    .await?
                {
                    writeln!(out, "{label}: kept: {why}")?;
                    continue;
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
                            trashed = true;
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
                        writeln!(out, "{label}: {how} ({reason}), {}", human_bytes(bytes))?;
                        removed += 1;
                        freed += bytes;
                    }
                    Err(e) => {
                        failed += 1;
                        writeln!(out, "{label}: {e}")?;
                    }
                }
            }
            if dry_run {
                writeln!(out, "{tally}")?;
            }
        }
        let hooks_failed = tracker.has_failures();
        if !dry_run {
            tracker.finish(&state.conn, None, false, now)?;
        }
        let videos = if removed == 1 { "video" } else { "videos" };
        if removed == 0 && failed == 0 {
            writeln!(out, "nothing to clean up")?;
        } else if dry_run {
            writeln!(
                out,
                "would remove {removed} {videos} ({})",
                human_bytes(freed)
            )?;
        } else {
            let mut line = format!("removed {removed} {videos} ({})", human_bytes(freed));
            if failed > 0 {
                line.push_str(&format!("; {failed} couldn't be removed"));
            }
            if trashed {
                line.push_str("; `tidepool trash restore <video_id>` brings one back");
            }
            writeln!(out, "{line}")?;
        }
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
        let position = meta["source"]["position"].as_u64().unwrap_or(0) as u32;
        let values = crate::download::layout::Values::new(&info, source, source_title, position);
        let stage = crate::download::stages::Stage {
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
        };
        let context = stage.context(&info);
        let mut reports = Vec::new();
        let keep = stage
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
    fn purge_trash(
        &self,
        state: &State,
        now: jiff::Timestamp,
        out: &mut dyn Write,
    ) -> anyhow::Result<()> {
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
    fn trash(&self, cmd: &crate::cli::TrashCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
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
                    self.scoped_sources(&config, sources)?
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
        &self,
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
            picked = every.clone();
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
        use crate::tools::manager::{Manager, Tool};

        let config = self.config_or_defaults()?;
        let tools = &config.tools;
        let manager = Manager::new(&self.dirs);
        let when = |t: jiff::Timestamp| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string();
        // (tool, managed, your own path, update interval, what it's for)
        let describe = |name: ToolName| match name {
            ToolName::Ytdlp => (
                Tool::Ytdlp(tools.ytdlp.channel),
                tools.ytdlp.managed,
                tools.ytdlp.path.as_str(),
                tools.ytdlp.update_check.0,
                format!("{} channel", tools.ytdlp.channel.as_str()),
                "yt-dlp",
            ),
            ToolName::Deno => (
                Tool::Deno,
                tools.deno.managed,
                tools.deno.path.as_str(),
                tools.deno.update_check.0,
                "2.x, the JavaScript runtime yt-dlp needs for YouTube".to_string(),
                "deno",
            ),
        };
        let own = |name: &str, table: &str| {
            format!(
                "managed = false in [tools.{table}]: tidepool doesn't update your {name}; update it \
                 yourself, or set managed = true"
            )
        };
        match cmd {
            ToolsCmd::Status => {
                for name in [ToolName::Ytdlp, ToolName::Deno] {
                    let (tool, managed, path, every, what, table) = describe(name);
                    let label = tool.name();
                    if !managed {
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
                                )?
                            }
                            Err(e) => writeln!(
                                out,
                                "{label}: cannot run {path:?} (tools.{table}.path): {e}"
                            )?,
                        }
                        continue;
                    }
                    let binary = manager.binary(tool);
                    let Some(state) = manager.state(tool).filter(|_| binary.exists()) else {
                        writeln!(
                            out,
                            "{label} isn't downloaded yet (managed, {what}); it's downloaded on \
                             first use, or run `tidepool tools update {table}`"
                        )?;
                        continue;
                    };
                    writeln!(out, "{label} {} (managed, {what})", state.version)?;
                    writeln!(out, "  path: {}", binary.display())?;
                    writeln!(out, "  installed: {}", when(state.installed_at))?;
                    writeln!(
                        out,
                        "  last update check: {} (checks every {})",
                        when(state.last_check_at),
                        crate::config::types::format_duration(every)
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
                }
            }
            ToolsCmd::Update { tool: requested } => {
                let names = match requested {
                    Some(name) => vec![*name],
                    None => vec![ToolName::Ytdlp, ToolName::Deno],
                };
                let mut failed = false;
                for name in names {
                    let (tool, managed, _, _, _, table) = describe(name);
                    if !managed {
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
                let (tool, managed, _, _, _, table) = describe(*name);
                if !managed {
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
            self.cli.force_config,
        )? {
            Imported::Created(rev) => tracing::info!("imported {name} as config revision {rev}"),
            Imported::Unchanged(rev) => tracing::debug!("{name} matches config revision {rev}"),
        }
        Ok(())
    }

    /// Log environment findings, what passed at `ok_level`; returns the errors.
    fn report(&self, findings: Vec<Finding>, ok_level: tracing::Level) -> usize {
        let mut errors = 0;
        for finding in findings {
            match finding {
                Finding::Ok(message) if ok_level == tracing::Level::INFO => {
                    tracing::info!("{message}")
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
        let errors = self.report(
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
             \x20    Or keep syncing every {interval} with the web UI at http://{listen}: tidepool run --daemon",
            import_path = self.import_arg(Path::new(name)),
            interval = crate::config::types::format_duration(
                config.schedule.interval.0.unwrap_or_default()
            ),
            listen = config.server.listen,
        )?;
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "tidepool".into());
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

    fn open_state(&self) -> anyhow::Result<State> {
        Ok(State::open(&self.paths.state_db)?)
    }

    fn lock(&self) -> anyhow::Result<LibraryLock> {
        Ok(lock::acquire(&self.paths.lock)?)
    }

    fn display_path(path: &Path) -> String {
        std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string()
    }

    fn warn(&self, warnings: &[String]) {
        for warning in warnings {
            tracing::warn!("{warning}");
        }
    }

    /// The config to run with: the stored revision (`--config` was imported
    /// up front), or with `--dry-run`, the `--config` file without storing it.
    fn current_config(&self, state: &State) -> anyhow::Result<Loaded> {
        if let (Some(path), true) = (&self.cli.config, self.cli.dry_run) {
            let loaded = load_file(path)?;
            self.warn(&loaded.warnings);
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
        self.warn(&loaded.warnings);
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
                self.warn(&loaded.warnings);
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
                        writeln!(out, "unchanged: {name} matches revision {rev}")?
                    }
                }
            }
            Some(ConfigCmd::Export { rev }) => {
                let state = self.open_state()?;
                let store = ConfigStore::new(&state.conn);
                let revision = match rev {
                    Some(rev) => store.get(*rev)?,
                    None => {
                        self.current_config(&state)?;
                        store.current()?.expect("loaded above")
                    }
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
                self.print_history(&history, out)?;
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
            Some(ConfigCmd::Merge { .. }) => {
                bail!("`config merge` is not implemented yet (planned for M7)")
            }
        }
        Ok(exit::OK)
    }

    fn print_history(&self, history: &[Revision], out: &mut dyn Write) -> anyhow::Result<()> {
        let current = history.last().map(|r| r.rev);
        for r in history {
            let marker = if Some(r.rev) == current { "*" } else { " " };
            let created = r
                .created_at
                .parse::<jiff::Timestamp>()
                .map(|t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|_| r.created_at.clone());
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
        let file_path = match path
            .map(Path::to_path_buf)
            .or_else(|| self.cli.config.clone())
        {
            Some(path) => path,
            None => {
                let last = match &store {
                    Some(store) => store.last_import()?,
                    None => None,
                };
                last.and_then(|r| r.source_path)
                    .map(PathBuf::from)
                    .filter(|p| p.exists())
                    .unwrap_or_else(|| self.dirs.default_config_file())
            }
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
                    self.warn(&loaded.warnings);
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
                let same = text.as_ref().is_some_and(|text| {
                    crate::config::store::hash(text) == current.source_hash
                        || file
                            .as_ref()
                            .is_some_and(|f| loaded.is_some() && f.normalized() == current.json)
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
        };

        let Some(loaded) = file.or(stored) else {
            bail!(
                "{} invalid; see above",
                invalid.join(" and ") + if invalid.len() > 1 { " are" } else { " is" }
            );
        };
        let env_failed = self.report(
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
        source: &Option<SourceRef>,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        use crate::state::items;

        let state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = effective_sources(&loaded.config);
        let limit =
            usize::try_from(loaded.config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
        let now = jiff::Timestamp::now();

        // A video tidepool knows: everything stored about it, in each source.
        let known = items::videos(&state.conn, &[video.to_string()])?;
        if let Some(item) = known.get(video) {
            let statuses = items::item_statuses(&state.conn, video)?;
            let picked: Vec<&EffectiveSource> = match source {
                Some(r) => vec![resolve_source(r, &sources)?],
                None => sources
                    .iter()
                    .filter(|s| statuses.iter().any(|(key, _)| *key == s.key))
                    .collect(),
            };
            describe_video(out, item)?;
            if picked.is_empty() {
                writeln!(out, "in no configured source; pass --source to test one")?;
            }
            for source in picked {
                let rules = RuleSet::compile(source, limit, now).map_err(anyhow::Error::msg)?;
                let status = statuses
                    .iter()
                    .find(|(key, _)| *key == source.key)
                    .map(|(_, status)| status.replace('_', " "))
                    .unwrap_or_else(|| "not in this source".into());
                writeln!(out, "\nsource {} ({status})", source.key)?;
                write_rules(out, &rules, &Fields::from_item(item))?;
            }
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

/// What's stored about a video, the fields rules look at.
fn describe_video(out: &mut dyn Write, item: &crate::index::RemoteItem) -> std::io::Result<()> {
    writeln!(out, "{} “{}”", item.video_id, item.title)?;
    let unknown = || "not known yet".to_string();
    let kind = item
        .kind
        .map(|k| k.as_str().to_string())
        .unwrap_or_else(|| "not known yet (checked before download)".into());
    // 4m26s, 1h2m3s.
    let duration = item
        .duration
        .map(|d| {
            let (h, m, s) = (d / 3600, d % 3600 / 60, d % 60);
            match (h, m) {
                (0, 0) => format!("{s}s"),
                (0, _) => format!("{m}m{s}s"),
                _ => format!("{h}h{m}m{s}s"),
            }
        })
        .unwrap_or_else(unknown);
    let published = item
        .published_at
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_else(unknown);
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
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into());
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
        .map(|line| {
            let color = if line.starts_with("+++") || line.starts_with("---") {
                "1"
            } else if line.starts_with('+') {
                "32"
            } else if line.starts_with('-') {
                "31"
            } else if line.starts_with("@@") {
                "36"
            } else {
                return line.to_string();
            };
            let body = line.trim_end_matches('\n');
            let end = &line[body.len()..];
            format!("\x1b[{color}m{body}\x1b[0m{end}")
        })
        .collect()
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
