//! Command dispatch: turns a parsed [`Cli`] into work and an exit code.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::Value;

use crate::cli::{
    Cli, Command, ConfigArgs, ConfigCmd, DownloadArgs, FilterCmd, InitArgs, ResetArgs, RunArgs,
    ScopeArgs, SourceRef, YtdlpCmd,
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
}

/// A command-line usage problem found after parsing (exit 2).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

struct App<'a> {
    cli: &'a Cli,
    dirs: Dirs,
    paths: StatePaths,
}

/// Run a parsed command; returns the process exit code.
pub async fn run(cli: &Cli, out: &mut dyn Write) -> anyhow::Result<i32> {
    let dirs = Dirs::from_env().map_err(anyhow::Error::msg)?;
    let paths = dirs.state();
    let app = App { cli, dirs, paths };
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
            cmd: FilterCmd::Test { title, source },
        } => app.filter_test(title, source, out),
        Command::Ytdlp { cmd } => app.ytdlp(cmd, out).await,
        Command::Index(scope) => app.index(scope, out).await,
        Command::Download(args) => app.download(args, out).await,
        Command::Run(args) if !args.daemon => app.run_once(args, out).await,
        Command::Reset(args) => app.reset(args, out),
        Command::Problems { cmd } => app.problems(cmd, out),
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
        Command::Index(_) | Command::Download(_) | Command::Reset(_) | Command::Filter { .. } => {
            true
        }
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
        | Command::Ytdlp { .. }
        | Command::Index(_)
        | Command::Download(_)
        | Command::Reset(_) => true,
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
                "{}: {} (category {}), format {}, output {}",
                source.key,
                source.url,
                source.category,
                source.format,
                source.output
            );
            let rules = |rules: &[crate::config::Rule]| {
                rules
                    .iter()
                    .map(|r| match r {
                        crate::config::Rule::Pattern(p) => format!("{p:?}"),
                        crate::config::Rule::Field(f) => {
                            format!("{:?} on {:?}", f.pattern, f.field).to_lowercase()
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

    /// The yt-dlp to run: the managed one (downloaded or updated if needed)
    /// or `ytdlp.path`. Failed update checks become a problem.
    async fn ytdlp_binary(
        &self,
        state: &State,
        config: &crate::config::Config,
    ) -> anyhow::Result<PathBuf> {
        use crate::problems::{Kind, record, resolve};
        use crate::ytdlp::manager::{Manager, UpdateOutcome};

        let ytdlp = &config.ytdlp;
        if !ytdlp.managed {
            let path = PathBuf::from(&ytdlp.path);
            log_ytdlp(&path, "your own; tidepool doesn't update it");
            return Ok(path);
        }
        let now = jiff::Timestamp::now();
        let manager = Manager::new(&self.dirs);
        let version = manager.state(ytdlp.channel).map(|s| s.version);
        let ensured = match manager
            .ensure(ytdlp.channel, ytdlp.update_check.0, now)
            .await
        {
            Ok(ensured) => ensured,
            Err(err) => {
                let message = err.to_string();
                let hint = Kind::UpdateFailed.hint(version.as_deref(), true);
                record(
                    &state.conn,
                    Kind::UpdateFailed,
                    1,
                    &message,
                    version.as_deref(),
                    &Default::default(),
                    &hint,
                    now,
                )?;
                return Err(err.into());
            }
        };
        if let Some(warning) = &ensured.warning {
            tracing::warn!("{warning}");
            let hint = Kind::UpdateFailed.hint(version.as_deref(), true);
            record(
                &state.conn,
                Kind::UpdateFailed,
                1,
                warning,
                version.as_deref(),
                &Default::default(),
                &hint,
                now,
            )?;
        }
        if let Some(outcome) = ensured.outcome {
            resolve(&state.conn, Kind::UpdateFailed, now)?;
            match outcome {
                UpdateOutcome::Installed { .. } | UpdateOutcome::Updated { .. } => {
                    tracing::info!("{outcome}")
                }
                _ => tracing::debug!("{outcome}"),
            }
        }
        log_ytdlp(
            &ensured.path,
            &format!("managed, {} channel", ytdlp.channel.as_str()),
        );
        Ok(ensured.path)
    }

    /// End of a run: if it hit something a newer yt-dlp usually fixes, check
    /// for one right away (at most hourly); then record and resolve problems.
    async fn finish_problems(
        &self,
        state: &State,
        config: &crate::config::Config,
        tracker: crate::problems::Tracker,
        ytdlp: &Path,
    ) -> anyhow::Result<()> {
        let managed = config.ytdlp.managed;
        if managed && tracker.wants_update() {
            let manager = crate::ytdlp::manager::Manager::new(&self.dirs);
            let hour = std::time::Duration::from_secs(3600);
            match manager
                .ensure(config.ytdlp.channel, hour, jiff::Timestamp::now())
                .await
            {
                Ok(ensured) => {
                    if let Some(outcome @ crate::ytdlp::manager::UpdateOutcome::Updated { .. }) =
                        ensured.outcome
                    {
                        tracing::info!("{outcome}; failed videos are retried on the next run");
                    }
                }
                Err(err) => tracing::warn!("could not check for a yt-dlp update: {err}"),
            }
        }
        let version = crate::ytdlp::manager::tool_version(ytdlp).ok();
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
        self.finish_problems(&state, &loaded.config, tracker, &ytdlp)
            .await?;
        Ok(if failed > 0 { exit::PARTIAL } else { exit::OK })
    }

    /// Index `sources`, printing a line per source; returns how many failed.
    async fn index_sources(
        &self,
        state: &mut State,
        config: &crate::config::Config,
        sources: &[EffectiveSource],
        ytdlp: &Path,
        tracker: &mut crate::problems::Tracker,
        out: &mut dyn Write,
    ) -> anyhow::Result<usize> {
        let now = jiff::Timestamp::now();
        let mut failed = 0;
        for source in sources {
            match crate::pipeline::index_source(state, config, source, &self.dirs, ytdlp, now).await
            {
                Ok(summary) => {
                    tracker.index_succeeded(&source.key);
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
                    tracker.index_failed(&source.key, &err);
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
        ytdlp: PathBuf,
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
            dirs: self.dirs.clone(),
            ytdlp_version: crate::ytdlp::manager::tool_version(&ytdlp).ok(),
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
            cancel: tokio_util::sync::CancellationToken::new(),
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
        let summary =
            crate::download::download_all(state, sources, paths, ctx, options, tracker, out)
                .await
                .map_err(anyhow::Error::msg)?;

        if summary.interrupted.is_some() {
            writeln!(
                out,
                "stopped; unfinished downloads will resume on the next run"
            )?;
        } else if summary.downloaded + summary.skipped + summary.failed == 0 {
            writeln!(out, "nothing to download")?;
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
            writeln!(out, "{}", parts.join("; "))?;
        }
        Ok(summary)
    }

    fn exit_code(summary: &crate::download::Summary, index_failures: usize) -> i32 {
        match summary.interrupted {
            Some(code) => code,
            None if summary.failed > 0 || index_failures > 0 => exit::PARTIAL,
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
        Ok(Self::exit_code(&summary, failed))
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
            for dir in reset.iter().filter_map(|(_, dir)| dir.as_ref()) {
                // Only folders tidepool made: they hold its metadata.json.
                let path = PathBuf::from(dir);
                if path.is_absolute() && path.join("metadata.json").is_file() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("cannot delete {}", path.display()))?;
                    writeln!(out, "deleted {}", path.display())?;
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

    async fn ytdlp(&self, cmd: &YtdlpCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
        use crate::ytdlp::manager::Manager;

        let config = self.config_or_defaults()?;
        let ytdlp = &config.ytdlp;
        if !ytdlp.managed {
            if !matches!(cmd, YtdlpCmd::Status) {
                bail!(
                    "ytdlp.managed = false: tidepool doesn't update your yt-dlp; update it yourself \
                     (e.g. `yt-dlp -U`, pip or brew), or set ytdlp.managed = true"
                );
            }
            let version = crate::ytdlp::manager::tool_version(Path::new(&ytdlp.path))
                .map_err(|e| anyhow::anyhow!("cannot run {:?} (ytdlp.path): {e}", ytdlp.path))?;
            writeln!(
                out,
                "yt-dlp {version} at {} (ytdlp.managed = false: tidepool doesn't update it)",
                ytdlp.path
            )?;
            return Ok(exit::OK);
        }

        let manager = Manager::new(&self.dirs);
        let channel = ytdlp.channel;
        let when = |t: jiff::Timestamp| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string();
        match cmd {
            YtdlpCmd::Status => {
                let path = manager.binary(channel);
                let Some(state) = manager.state(channel).filter(|_| path.exists()) else {
                    writeln!(
                        out,
                        "yt-dlp isn't downloaded yet (managed, {} channel); it's downloaded on first use, \
                         or run `tidepool ytdlp update`",
                        channel.as_str()
                    )?;
                    return Ok(exit::OK);
                };
                writeln!(
                    out,
                    "yt-dlp {} (managed, {} channel)",
                    state.version,
                    channel.as_str()
                )?;
                writeln!(out, "path: {}", path.display())?;
                writeln!(out, "installed: {}", when(state.installed_at))?;
                writeln!(
                    out,
                    "last update check: {} (checks every {})",
                    when(state.last_check_at),
                    crate::config::types::format_duration(ytdlp.update_check.0)
                )?;
                if let Some(previous) = &state.previous_version {
                    writeln!(
                        out,
                        "previous version: {previous} (`tidepool ytdlp rollback` restores it)"
                    )?;
                }
                if let Some(skipped) = &state.skip_version {
                    writeln!(
                        out,
                        "{skipped} was rolled back; staying on {} until a newer release",
                        state.version
                    )?;
                }
            }
            YtdlpCmd::Update => {
                if self.cli.dry_run {
                    writeln!(
                        out,
                        "dry run: would check {} for a newer release",
                        channel.repository()
                    )?;
                    return Ok(exit::OK);
                }
                let outcome = manager.update(channel, jiff::Timestamp::now()).await?;
                writeln!(out, "{outcome}")?;
            }
            YtdlpCmd::Rollback => {
                let (from, to) = manager.rollback(channel)?;
                writeln!(
                    out,
                    "rolled back yt-dlp {from} → {to}; {from} won't be installed again until a newer release"
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

    fn report(&self, findings: Vec<Finding>) -> usize {
        let mut errors = 0;
        for finding in findings {
            match finding {
                Finding::Ok(message) => tracing::info!("{message}"),
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
        let errors = self.report(check_environment(&loaded.config, &self.dirs, Some(&path)));

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
        let (_, loaded) = ConfigStore::new(&state.conn).load_current()?;
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
                self.current_config(&state)?;
                let current = ConfigStore::new(&state.conn)
                    .current()?
                    .expect("loaded above");
                write!(out, "{}", show(&current.toml))?;
            }
            Some(ConfigCmd::Schema) => write!(out, "{}", schema_json())?,
            Some(ConfigCmd::Check { path, reconcile }) => {
                if *reconcile {
                    bail!("--reconcile is not implemented yet (planned for M5)");
                }
                return self.config_check(path.as_deref(), args.show_secrets, out);
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

    fn config_check(
        &self,
        path: Option<&Path>,
        show_secrets: bool,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let (loaded, source, file) = match path {
            Some(path) => (
                load_file(path)?,
                Self::display_path(path),
                Some(path.to_path_buf()),
            ),
            None => {
                let state = self.open_state()?;
                let loaded = self.current_config(&state)?;
                let current = ConfigStore::new(&state.conn)
                    .current()?
                    .expect("loaded above");
                let file = current
                    .source_path
                    .as_ref()
                    .map(PathBuf::from)
                    .filter(|p| p.exists());
                (loaded, format!("config revision {}", current.rev), file)
            }
        };
        if path.is_some() {
            self.warn(&loaded.warnings);
        }

        let failed = self.report(check_environment(
            &loaded.config,
            &self.dirs,
            file.as_deref(),
        )) > 0;
        write!(
            out,
            "{}",
            EffectiveConfig::new(&loaded.config, &self.dirs, show_secrets).to_toml()
        )?;
        if failed {
            bail!("{source} is valid, but the environment checks above failed");
        }
        tracing::info!(
            "{source} is valid ({} sources)",
            loaded.config.sources.len()
        );
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
        title: &str,
        source: &SourceRef,
        out: &mut dyn Write,
    ) -> anyhow::Result<i32> {
        let state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = effective_sources(&loaded.config);
        let source = resolve_source(source, &sources)?;
        let limit =
            usize::try_from(loaded.config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
        let rules =
            RuleSet::compile(source, limit, jiff::Timestamp::now()).map_err(anyhow::Error::msg)?;
        // A bare title has no duration or date: test only the include/exclude rules.
        let rules = RuleSet {
            min_duration: None,
            max_duration: None,
            cutoff: None,
            ..rules
        };
        let item = Fields {
            title: Some(title),
            ..Fields::default()
        };

        writeln!(out, "source {} ({})", source.key, source.id)?;
        if rules.include.is_empty() && rules.exclude.is_empty() {
            writeln!(out, "  no include/exclude rules")?;
        }
        for rule in rules.include.iter().chain(&rules.exclude) {
            let fields: Vec<String> = rule
                .fields
                .iter()
                .map(|f| format!("{f:?}").to_lowercase())
                .collect();
            let result = match rule.test(&item) {
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
        let decision = rules.decide(&item);
        writeln!(out, "decision: {decision}")?;
        Ok(exit::OK)
    }
}

/// Find the configured source a `--source` value refers to.
pub fn resolve_source<'a>(
    r: &SourceRef,
    sources: &'a [EffectiveSource],
) -> anyhow::Result<&'a EffectiveSource> {
    let found = match r {
        SourceRef::Key(key) => sources.iter().find(|p| &p.key == key),
        SourceRef::Id(id) | SourceRef::Url { id, .. } => sources.iter().find(|p| &p.id == id),
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
        let version = crate::ytdlp::manager::tool_version(path)
            .unwrap_or_else(|e| format!("(not runnable: {e})"));
        tracing::debug!("yt-dlp {version} at {} ({how})", path.display());
    }
}
