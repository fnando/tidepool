//! Command-line interface definition.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
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

    /// Import --config even if the stored config was edited since its last import
    #[arg(long, global = true)]
    pub force_config: bool,

    /// Library to operate on (state in ~/.local/share/tidepool/libraries/<name>/)
    #[arg(
        long,
        short = 'l',
        env = "TIDEPOOL_LIBRARY",
        default_value = "default",
        global = true,
        value_parser = parse_library_name
    )]
    pub library: String,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a commented config file and the library behind it
    Init(InitArgs),
    /// Full pipeline: index, filter, download, run hooks, clean up
    Sync(ScopeArgs),
    /// Index only: update the DB with remote items and filter results
    Index(ScopeArgs),
    /// Download items already indexed and pending; no re-index
    Download(DownloadArgs),
    /// Run cleanup policies only
    Cleanup(ScopeArgs),
    /// List known items
    List(ListArgs),
    /// Post-download hooks
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
    /// Exempt an item from cleanup
    Pin(VideoArgs),
    /// Remove a pin
    Unpin(VideoArgs),
    /// Forget an item so it is downloaded again
    Reset(ResetArgs),
    /// List open problems, or acknowledge one
    Problems {
        #[command(subcommand)]
        cmd: Option<ProblemsCmd>,
    },
    /// Inspect, update or roll back the managed yt-dlp binary
    Ytdlp {
        #[command(subcommand)]
        cmd: YtdlpCmd,
    },
    /// Daemon: sync + cleanup every schedule.interval, plus the web UI ([server])
    Run(RunArgs),
}

impl Command {
    /// Milestone in which a not-yet-implemented command lands.
    pub fn milestone(&self) -> Option<&'static str> {
        match self {
            Command::Init(_) | Command::Config(_) | Command::Filter { .. } => None,
            Command::Sync(_)
            | Command::Index(_)
            | Command::Download(_)
            | Command::Reset(_)
            | Command::Problems { .. }
            | Command::Ytdlp { .. } => Some("M2"),
            Command::Hooks { .. } | Command::Embed(_) => Some("M4"),
            Command::Cleanup(_) | Command::Pin(_) | Command::Unpin(_) => Some("M5"),
            Command::List(_) => Some("M6"),
            Command::Run(_) => Some("M7"),
        }
    }
}

#[derive(Debug, Args)]
pub struct ScopeArgs {
    /// Limit to these playlists: config key, playlist id or URL; repeatable
    #[arg(long = "playlist", short = 'p', value_name = "KEY|ID|URL")]
    pub playlists: Vec<PlaylistRef>,
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Download at most N items
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// Parallel downloads (overrides library.concurrency)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub concurrency: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ItemStatus {
    Pending,
    Done,
    Filtered,
    CleanedUp,
    Failed,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    #[arg(long, value_enum)]
    pub status: Option<ItemStatus>,
    /// Downloaded items whose media is no longer in the library
    #[arg(long)]
    pub missing: bool,
    /// Show API quota used today
    #[arg(long)]
    pub quota: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum HooksCmd {
    /// Re-run post-processing for one video
    Run {
        video_id: String,
        /// Only this hook
        #[arg(long)]
        hook: Option<String>,
        /// Re-run hooks that already succeeded
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ConfigArgs {
    /// Print API keys unmasked
    #[arg(long, global = true)]
    pub show_secrets: bool,

    #[command(subcommand)]
    pub cmd: Option<ConfigCmd>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Validate a file (or the current revision) and print the effective config
    Check {
        path: Option<PathBuf>,
        /// Walk the disk and fix size/presence drift
        #[arg(long)]
        reconcile: bool,
    },
    /// Print the JSON Schema for the config file
    Schema,
    /// Validate a file and store it as a new revision
    Import { path: PathBuf },
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
    /// Three-way merge a file into the stored config
    Merge { path: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum FilterCmd {
    /// Show which include/exclude rule matches a title for a playlist
    Test {
        title: String,
        /// Playlist whose rules to test (config key, id or URL)
        #[arg(long = "playlist", short = 'p', value_name = "KEY|ID|URL")]
        playlist: PlaylistRef,
    },
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct EmbedTarget {
    pub video_id: Option<String>,
    #[arg(long = "playlist", short = 'p', value_name = "KEY|ID|URL")]
    pub playlist: Option<PlaylistRef>,
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct EmbedArgs {
    #[command(flatten)]
    pub target: EmbedTarget,
}

#[derive(Debug, Args)]
pub struct VideoArgs {
    pub video_id: String,
}

#[derive(Debug, Args)]
pub struct ResetArgs {
    pub video_id: String,
    /// Also delete the video's files
    #[arg(long)]
    pub delete_files: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConfigMismatch {
    Import,
    Merge,
    Export,
    Fail,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// What to do when the config file differs from the stored config. Without
    /// it: ask in a terminal, resolve in the web UI, or fail when the UI is off.
    #[arg(long, value_enum)]
    pub on_config_mismatch: Option<ConfigMismatch>,
}

#[derive(Debug, Subcommand)]
pub enum ProblemsCmd {
    /// Hide a problem until it happens again
    Ack { id: i64 },
}

#[derive(Debug, Subcommand)]
pub enum YtdlpCmd {
    /// Path, version, channel and last update check
    Status,
    /// Check for a new release now
    Update,
    /// Restore the previous binary until the next release
    Rollback,
}

/// `tidepool init`. The config file goes to the global --config path
/// (default ~/.config/tidepool/config.toml); --library names the library.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Replace an existing config file (kept as <file>.bak)
    #[arg(long)]
    pub force: bool,
    /// Print the config file it would write; create nothing
    #[arg(long)]
    pub print: bool,
}

/// A playlist reference from the command line, resolved after config load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistRef {
    /// A playlist config key, e.g. `lofi`.
    Key(String),
    /// A YouTube playlist id, e.g. `PLxxxxxxxx`.
    Id(String),
    /// A URL with `list=`; holds the extracted playlist id.
    Url { url: String, id: String },
}

impl PlaylistRef {
    pub fn raw(&self) -> &str {
        match self {
            PlaylistRef::Key(s) | PlaylistRef::Id(s) => s,
            PlaylistRef::Url { url, .. } => url,
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

impl FromStr for PlaylistRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains("://") || s.contains("list=") {
            return playlist_id_from_url(s)
                .map(|id| PlaylistRef::Url {
                    url: s.to_string(),
                    id,
                })
                .ok_or_else(|| format!("{s:?} is a URL without a list= parameter"));
        }
        if looks_like_playlist_id(s) {
            return Ok(PlaylistRef::Id(s.to_string()));
        }
        if crate::config::is_valid_playlist_key(s) {
            return Ok(PlaylistRef::Key(s.to_string()));
        }
        Err(format!(
            "{s:?} is not a playlist key, playlist id or playlist URL"
        ))
    }
}

fn parse_library_name(s: &str) -> Result<String, String> {
    if crate::config::is_valid_playlist_key(s) {
        Ok(s.to_string())
    } else {
        Err("library names use lowercase letters, digits, '-' and '_'".to_string())
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
    fn parses_playlist_refs() {
        assert_eq!("lofi".parse(), Ok(PlaylistRef::Key("lofi".into())));
        assert_eq!(
            "PLabcdefghij123".parse(),
            Ok(PlaylistRef::Id("PLabcdefghij123".into()))
        );
        assert_eq!(
            "https://www.youtube.com/watch?v=x&list=PLabc&index=2".parse(),
            Ok(PlaylistRef::Url {
                url: "https://www.youtube.com/watch?v=x&list=PLabc&index=2".into(),
                id: "PLabc".into()
            })
        );
        assert!(
            "https://www.youtube.com/watch?v=x"
                .parse::<PlaylistRef>()
                .is_err()
        );
        assert!("Not A Key".parse::<PlaylistRef>().is_err());
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
        assert!(Cli::try_parse_from(["tidepool", "ytdlp", "status"]).is_ok());
        assert!(Cli::try_parse_from(["tidepool", "problems", "ack", "3"]).is_ok());
        assert!(Cli::try_parse_from(["tidepool", "problems"]).is_ok());
    }

    #[test]
    fn config_flag_is_global() {
        let cli = Cli::try_parse_from(["tidepool", "sync", "--config", "a.toml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("a.toml")));
    }
}
