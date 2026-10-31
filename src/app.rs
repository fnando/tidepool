//! Command dispatch: turns a parsed [`Cli`] into work and an exit code.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::Value;

use crate::cli::{
    Cli, Command, ConfigArgs, ConfigCmd, FilterCmd, InitArgs, ScopeArgs, SourceRef, YtdlpCmd,
};
use crate::config::check::{EffectiveConfig, Finding, check_environment, media_root};
use crate::config::diff::{key_changes, mask_json, mask_toml, text_diff};
use crate::config::merge::{EffectiveSource, effective_sources};
use crate::config::schema::schema_json;
use crate::config::store::{ConfigStore, Imported, Revision};
use crate::config::{Loaded, load_file, load_str};
use crate::filter::{Fields, Match, RuleSet};
use crate::init;
use crate::lock::{self, LibraryLock};
use crate::paths::{Dirs, LibraryPaths};
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
    library: LibraryPaths,
}

/// Run a parsed command; returns the process exit code.
pub async fn run(cli: &Cli, out: &mut dyn Write) -> anyhow::Result<i32> {
    let dirs = Dirs::from_env().map_err(anyhow::Error::msg)?;
    let library = dirs.library(&cli.library);
    let app = App { cli, dirs, library };
    if cli.config.is_some() && !cli.dry_run && uses_stored_config(&cli.command) {
        app.import_config_flag()?;
    }
    match &cli.command {
        Command::Init(args) => app.init(args, out),
        Command::Config(args) => app.config(args, out),
        Command::Filter {
            cmd: FilterCmd::Test { title, source },
        } => app.filter_test(title, source, out),
        Command::Ytdlp { cmd } => app.ytdlp(cmd, out).await,
        Command::Index(scope) => app.index(scope, out).await,
        other => {
            let milestone = other.milestone().unwrap_or("a later milestone");
            bail!("this command is not implemented yet (planned for {milestone})")
        }
    }
}

/// Commands that read the library's stored config, so `--config` is imported
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
        Command::Filter { .. } | Command::Ytdlp { .. } | Command::Index(_) => true,
        _ => false,
    }
}

impl App<'_> {
    /// The stored config, or built-in defaults for a library without one
    /// (so library-independent commands like `ytdlp` work before `init`).
    fn config_or_defaults(&self) -> anyhow::Result<crate::config::Config> {
        if let (Some(path), true) = (&self.cli.config, self.cli.dry_run) {
            return Ok(load_file(path)?.config);
        }
        if !self.library.state_db.exists() {
            return Ok(crate::config::Config::default());
        }
        let state = self.open_state()?;
        match ConfigStore::new(&state.conn).load_current(&self.cli.library) {
            Ok((_, loaded)) => Ok(loaded.config),
            Err(crate::config::store::StoreError::Empty(_)) => Ok(crate::config::Config::default()),
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

    /// The yt-dlp to run: the managed one (downloaded or updated if needed)
    /// or `ytdlp.path`.
    async fn ytdlp_binary(&self, config: &crate::config::Config) -> anyhow::Result<PathBuf> {
        let ytdlp = &config.ytdlp;
        if !ytdlp.managed {
            return Ok(PathBuf::from(&ytdlp.path));
        }
        let manager = crate::ytdlp::manager::Manager::new(&self.dirs);
        let ensured = manager
            .ensure(ytdlp.channel, ytdlp.update_check.0, jiff::Timestamp::now())
            .await?;
        if let Some(warning) = ensured.warning {
            tracing::warn!("{warning}");
        }
        match ensured.outcome {
            Some(outcome @ crate::ytdlp::manager::UpdateOutcome::Installed { .. })
            | Some(outcome @ crate::ytdlp::manager::UpdateOutcome::Updated { .. }) => {
                tracing::info!("{outcome}")
            }
            Some(outcome) => tracing::debug!("{outcome}"),
            None => {}
        }
        Ok(ensured.path)
    }

    async fn index(&self, scope: &ScopeArgs, out: &mut dyn Write) -> anyhow::Result<i32> {
        let mut state = self.open_state()?;
        let loaded = self.current_config(&state)?;
        let sources = self.scoped_sources(&loaded.config, &scope.sources)?;
        if sources.is_empty() {
            writeln!(
                out,
                "no enabled sources; add one under [sources.<name>] in your config"
            )?;
            return Ok(exit::OK);
        }
        let _lock = self.lock()?;
        let ytdlp = self.ytdlp_binary(&loaded.config).await?;
        let now = jiff::Timestamp::now();
        let mut failed = 0;
        for source in &sources {
            match crate::pipeline::index_source(
                &mut state,
                &loaded.config,
                source,
                &self.dirs,
                &ytdlp,
                now,
            )
            .await
            {
                Ok(summary) => writeln!(
                    out,
                    "{}",
                    crate::pipeline::describe_index(&source.key, &summary)
                )?,
                Err(err) => {
                    failed += 1;
                    tracing::error!("{}: {err}", source.key);
                }
            }
        }
        Ok(if failed > 0 { exit::PARTIAL } else { exit::OK })
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
                tracing::info!(
                    "dry run: would write {name} and create library {:?}",
                    self.cli.library
                );
            }
            write!(out, "{text}")?;
            return Ok(exit::OK);
        }

        self.ensure_library_is_new()?;
        if path.exists() && !args.force {
            bail!(
                "{name} already exists; import it with `tidepool config import {name}`, \
                 or pass --force to replace it (the old file is kept as {name}.bak)"
            );
        }

        write_private_file(&path, &text, args.force)?;
        let state = self.open_state()?;
        let _lock = self.lock()?;
        let rev = ConfigStore::new(&state.conn)
            .import(Some(&name), &text, &loaded)?
            .rev();
        let errors = self.report(check_environment(&loaded.config, &self.dirs, Some(&path)));

        self.print_next_steps(&name, rev, &loaded, errors, out)?;
        Ok(exit::OK)
    }

    /// `init` never touches a library that already has a config.
    fn ensure_library_is_new(&self) -> anyhow::Result<()> {
        if !self.library.state_db.exists() {
            return Ok(());
        }
        let state = self.open_state()?;
        if let Some(current) = ConfigStore::new(&state.conn).current()? {
            bail!(
                "library {:?} already has a config (revision {}); change it with \
                 `tidepool config import <path>`, or create another library with \
                 `tidepool init --library <name>`",
                self.cli.library,
                current.rev
            );
        }
        Ok(())
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
        let library = if self.cli.library == "default" {
            String::new()
        } else {
            format!(" --library {}", self.cli.library)
        };
        let config = &loaded.config;
        writeln!(out, "Created {name}")?;
        writeln!(
            out,
            "Created library {:?} (config revision {rev})",
            self.cli.library
        )?;
        if let Ok(root) = media_root(config, &self.dirs) {
            writeln!(out, "Videos go to {}", root.display())?;
        }
        if errors > 0 {
            writeln!(out, "\nFix the problems reported above before syncing.")?;
        }
        writeln!(
            out,
            "\nThe config includes a sample source with one short video. Next steps:\n\
             \x20 1. Edit {name}: replace the sample with your sources, set where videos go.\n\
             \x20 2. Apply your changes: tidepool config import{import_path}{library}\n\
             \x20 3. Sync once: tidepool run{library}\n\
             \x20    Or keep syncing every {interval} with the web UI at http://{listen}: tidepool run --daemon{library}",
            import_path = if Path::new(name) == self.dirs.default_config_file() {
                String::new()
            } else {
                format!(" {name}")
            },
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
             [Service]\nExecStart={exe} run --daemon{library} --config {name} --on-config-mismatch import\n\
             Restart=on-failure\n\n\
             [Install]\nWantedBy=default.target"
        )?;
        Ok(())
    }

    fn open_state(&self) -> anyhow::Result<State> {
        Ok(State::open(&self.library.state_db)?)
    }

    fn lock(&self) -> anyhow::Result<LibraryLock> {
        Ok(lock::acquire(&self.library.lock)?)
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
        let (_, loaded) = ConfigStore::new(&state.conn).load_current(&self.cli.library)?;
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
            .ok_or_else(|| crate::config::store::StoreError::Empty(self.cli.library.clone()))?;
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
            usize::try_from(loaded.config.library.regex_backtrack_limit).unwrap_or(usize::MAX);
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

/// Write `text` to `path` with owner-only permissions. With `replace`, an
/// existing file is moved to `<path>.bak` first.
fn write_private_file(path: &Path, text: &str, replace: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    if replace && path.exists() {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        std::fs::rename(path, &backup)
            .with_context(|| format!("cannot move {} aside", path.display()))?;
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
