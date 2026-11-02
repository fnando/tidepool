//! Command-line interface definition.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "tidepool",
    version,
    about = "Keep local mirrors of YouTube playlists in sync"
)]
pub struct Cli {
    /// TOML file to import (if changed) before running
    #[arg(
        long,
        short = 'c',
        env = "TIDEPOOL_CONFIG",
        value_name = "PATH",
        global = true
    )]
    pub config: Option<PathBuf>,

    #[command(flatten)]
    pub config_flags: ConfigFlags,

    /// Increase log verbosity (-v, -vv)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Only log errors
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Log output format
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty, global = true)]
    pub log_format: LogFormat,

    /// Plan everything, change nothing
    #[arg(long, global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// How `--config` is weighed against the stored config.
#[derive(Debug, Clone, Copy, Args)]
pub struct ConfigFlags {
    /// Import --config even if the stored config was edited since its last import
    #[arg(long = "force-config", global = true)]
    pub force: bool,

    /// Run with the stored config even if the config file has changes that
    /// aren't imported yet
    #[arg(long = "skip-config-check", global = true)]
    pub skip_check: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Create a commented config file and start tidepool's state
    Init(InitArgs),
    /// Index only: update the DB with remote items and filter results
    Index(ScopeArgs),
    /// Download items already indexed and pending; no re-index
    Download(DownloadArgs),
    /// Remove downloaded videos your `cleanup` limits no longer allow
    Cleanup(ScopeArgs),
    /// See, restore or empty what cleanup put in the trash
    Trash {
        #[command(subcommand)]
        cmd: TrashCmd,
    },
    /// Run hooks again
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Show the current config, or manage stored revisions
    Config(ConfigArgs),
    /// Include/exclude rules
    Filter {
        #[command(subcommand)]
        cmd: FilterCmd,
    },
    /// (Re)apply metadata/cover embedding
    Embed(EmbedArgs),
    /// Clean up titles of videos you already have (`clean_title`), renaming their files
    CleanTitles(VideoSelection),
    /// Forget an item so it is downloaded again
    Reset(ResetArgs),
    /// List open problems, or acknowledge one
    Problems {
        #[command(subcommand)]
        cmd: Option<ProblemsCmd>,
    },
    /// Inspect, update or roll back yt-dlp and Deno
    Tools {
        #[command(subcommand)]
        cmd: ToolsCmd,
    },
    /// Sync every source once; --daemon keeps syncing in the background
    Run(RunArgs),
    /// See and control the background daemon (`tidepool run --daemon`)
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Debug, Clone, Args)]
pub struct ScopeArgs {
    /// Limit to these sources: name, playlist id or URL; repeatable
    #[arg(long = "source", short = 's', value_name = "NAME|ID|URL")]
    pub sources: Vec<SourceRef>,
}

#[derive(Debug, Clone, Args)]
pub struct DownloadArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Download at most N items
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// Parallel downloads (overrides general.concurrency)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub concurrency: Option<u32>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HooksCmd {
    /// Run hooks again on videos already downloaded
    Run {
        #[command(flatten)]
        select: VideoSelection,
        /// Only this hook; repeatable
        #[arg(long = "hook", value_name = "NAME")]
        hooks: Vec<String>,
        /// Only hooks whose last run failed
        #[arg(long)]
        failed: bool,
    },
}

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ConfigArgs {
    /// Print API keys unmasked
    #[arg(long, global = true)]
    pub show_secrets: bool,
    /// Also print every setting left out, with its default
    #[arg(long)]
    pub include_defaults: bool,

    #[command(subcommand)]
    pub cmd: Option<ConfigCmd>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCmd {
    /// Validate a file and the stored config, and say whether they match; quiet when all is well
    Check { path: Option<PathBuf> },
    /// Print the JSON Schema for the config file
    Schema,
    /// Validate a file and store it as a new revision. Without a path: the
    /// --config file, or ~/.config/tidepool/config.toml
    Import { path: Option<PathBuf> },
    /// Write a revision as TOML to stdout
    Export {
        #[arg(long, value_name = "N")]
        rev: Option<i64>,
    },
    /// List stored revisions
    History,
    /// Make an old revision current again (as a new revision)
    Rollback { rev: i64 },
    /// Show how a file differs from the current revision
    Diff { path: Option<PathBuf> },
}

#[derive(Debug, Clone, Subcommand)]
pub enum FilterCmd {
    /// Show which rules match a video tidepool knows (by id), or a title
    Test {
        /// A video id (tested with everything known about it) or a title
        video: String,
        /// Source whose rules to test (name, playlist id or URL); for a video id,
        /// every source it's in when left out
        #[arg(long = "source", short = 's', value_name = "NAME|ID|URL")]
        source: Option<SourceRef>,
    },
}

/// Which downloaded videos a command works on.
#[derive(Debug, Clone, Args)]
pub struct VideoSelection {
    /// Only these videos
    pub video_ids: Vec<String>,
    /// Every downloaded video
    #[arg(long)]
    pub all: bool,
    /// Only videos of these sources: name, playlist id or URL; repeatable
    #[arg(long = "source", short = 's', value_name = "NAME|ID|URL")]
    pub sources: Vec<SourceRef>,
    /// Only videos of sources in this category; repeatable
    #[arg(long = "category", value_name = "NAME")]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct EmbedArgs {
    #[command(flatten)]
    pub select: VideoSelection,
    /// Also take out what's switched off in `embed` (covers, subtitles, chapters, tags)
    #[arg(long)]
    pub remove_disabled: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ResetArgs {
    pub video_id: String,
    /// Also delete the video's files
    #[arg(long)]
    pub delete_files: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConfigMismatch {
    /// Store the file as a new config revision
    Import,
    /// Write the stored config to the file (the old file is kept as .bak)
    Export,
    /// Stop with the differences (the default)
    Fail,
}

/// `tidepool run`: sync once and exit; with `--daemon`, keep syncing every
/// `schedule.interval` and serve the web UI (unless `server.enabled = false`).
#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// Keep running: sync every schedule.interval, and serve the web UI
    #[arg(long)]
    pub daemon: bool,

    /// Only sync these sources: name, playlist id or URL; repeatable
    #[arg(
        long = "source",
        short = 's',
        value_name = "NAME|ID|URL",
        conflicts_with = "daemon"
    )]
    pub sources: Vec<SourceRef>,

    /// With --daemon: what to do when the config file (--config, or the one
    /// last imported) differs from the stored config; without it, the daemon
    /// stops and shows the differences
    #[arg(long, value_enum, requires = "daemon")]
    pub on_config_mismatch: Option<ConfigMismatch>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TrashCmd {
    /// What's in the trash, and when it's deleted for good
    List,
    /// Put videos back where they were; they count as downloaded again
    Restore {
        #[arg(required = true)]
        video_ids: Vec<String>,
        /// Only from these sources: name, playlist id or URL; repeatable
        #[arg(long = "source", short = 's', value_name = "NAME|ID|URL")]
        sources: Vec<SourceRef>,
    },
    /// Delete everything in the trash now
    Empty,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DaemonCmd {
    /// Whether it's running, and what each part is doing
    Status,
    /// Stop a part (or every part) until you resume it, even across restarts
    Pause { service: Option<DaemonService> },
    /// Start a paused part (or every part) again
    Resume { service: Option<DaemonService> },
    /// Stop a part (or every part) and start it again
    Restart { service: Option<DaemonService> },
    /// Sync now instead of waiting for the schedule
    Sync,
}

/// A part of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DaemonService {
    /// Checks sources, downloads, cleans up
    Syncer,
    /// Finishes downloads and runs queued hooks
    Processor,
    /// The web UI
    Server,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProblemsCmd {
    /// Hide a problem until it happens again
    Ack { id: i64 },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ToolsCmd {
    /// Versions, paths and the last update checks
    Status,
    /// Check for new releases now (both tools, or one)
    Update { tool: Option<ToolName> },
    /// Restore a tool's previous version until its next release
    Rollback { tool: ToolName },
}

/// A tool tidepool manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolName {
    #[value(name = "yt-dlp")]
    Ytdlp,
    Deno,
}

/// `tidepool init`. The config file goes to the global --config path
/// (default ~/.config/tidepool/config.toml).
#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// Replace an existing config file with a fresh one, backing it up first
    /// (as <name>.<UTC timestamp>.bak). A stored config stays as it is until
    /// you run `tidepool config import`
    #[arg(long)]
    pub force: bool,
    /// Print the config file it would write; create nothing
    #[arg(long)]
    pub print: bool,
}

/// A source reference from the command line, resolved after config load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRef {
    /// A source name from the config, e.g. `lofi`.
    Key(String),
    /// A YouTube playlist id, e.g. `PLxxxxxxxx`.
    Id(String),
    /// A URL with `list=`; holds the extracted playlist id.
    Url { url: String, id: String },
}

impl SourceRef {
    #[must_use]
    pub fn raw(&self) -> &str {
        match self {
            SourceRef::Key(s) | SourceRef::Id(s) => s,
            SourceRef::Url { url, .. } => url,
        }
    }
}

/// Extract the `list=` parameter from a playlist or watch URL.
pub fn playlist_id_from_url(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("list="))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Playlist ids YouTube uses for playlists: `PL…`, plus the auto-generated
/// `UU…`, `OL…`, `FL…`, `LL`, `RD…` families.
fn looks_like_playlist_id(s: &str) -> bool {
    const PREFIXES: [&str; 6] = ["PL", "UU", "OL", "FL", "RD", "LL"];
    s.len() >= 12
        && PREFIXES.iter().any(|p| s.starts_with(p))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl FromStr for SourceRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains("://") || s.contains("list=") {
            return playlist_id_from_url(s)
                .map(|id| SourceRef::Url {
                    url: s.to_string(),
                    id,
                })
                .ok_or_else(|| format!("{s:?} is a URL without a list= parameter"));
        }
        if looks_like_playlist_id(s) {
            return Ok(SourceRef::Id(s.to_string()));
        }
        if crate::config::is_valid_name(s) {
            return Ok(SourceRef::Key(s.to_string()));
        }
        Err(format!(
            "{s:?} is not a source name, playlist id or playlist URL"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_source_refs() {
        assert_eq!("lofi".parse(), Ok(SourceRef::Key("lofi".into())));
        assert_eq!(
            "PLabcdefghij123".parse(),
            Ok(SourceRef::Id("PLabcdefghij123".into()))
        );
        assert_eq!(
            "https://www.youtube.com/watch?v=x&list=PLabc&index=2".parse(),
            Ok(SourceRef::Url {
                url: "https://www.youtube.com/watch?v=x&list=PLabc&index=2".into(),
                id: "PLabc".into()
            })
        );
        assert!(
            "https://www.youtube.com/watch?v=x"
                .parse::<SourceRef>()
                .is_err()
        );
        assert!("Not A Key".parse::<SourceRef>().is_err());
    }

    #[test]
    fn config_without_subcommand_shows_current() {
        let cli = Cli::try_parse_from(["tidepool", "config"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config(ConfigArgs { cmd: None, .. })
        ));
    }

    #[test]
    fn init_takes_only_force_and_print() {
        let cli = Cli::try_parse_from(["tidepool", "init", "--force", "--print"]).unwrap();
        let Command::Init(args) = cli.command else {
            panic!("expected init")
        };
        assert!(args.force && args.print);
        for removed in [
            "--yes",
            "--root",
            "--playlist",
            "--api-key",
            "--interval",
            "--no-server",
        ] {
            assert!(
                Cli::try_parse_from(["tidepool", "init", removed]).is_err(),
                "{removed}"
            );
        }
    }

    #[test]
    fn server_command_is_gone() {
        assert!(Cli::try_parse_from(["tidepool", "server"]).is_err());
        assert!(Cli::try_parse_from(["tidepool", "tools", "status"]).is_ok());
        assert!(Cli::try_parse_from(["tidepool", "tools", "update", "deno"]).is_ok());
        assert!(Cli::try_parse_from(["tidepool", "tools", "rollback", "yt-dlp"]).is_ok());
        assert!(Cli::try_parse_from(["tidepool", "tools", "rollback"]).is_err());
        assert!(Cli::try_parse_from(["tidepool", "problems", "ack", "3"]).is_ok());
        assert!(Cli::try_parse_from(["tidepool", "problems"]).is_ok());
    }

    #[test]
    fn config_flag_is_global() {
        let cli = Cli::try_parse_from(["tidepool", "run", "--config", "a.toml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("a.toml")));
    }
}
