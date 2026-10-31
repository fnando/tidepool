//! `tidepool init`: write a commented config file and create the library
//! behind it.
//!
//! The generated file lists every option with its default and a one-line
//! comment; options left at their default are commented out, so the file
//! doubles as documentation. A test keeps it in sync with the schema.

use std::io::IsTerminal;

use crate::cli::{InitArgs, InitPlaylist, parse_init_playlist};
use crate::config::duration::parse_duration;
use crate::config::is_valid_playlist_key;
use crate::config::schema::SCHEMA_ID;

/// Default sync interval offered by `init`.
pub const DEFAULT_INTERVAL: &str = "6h";

/// Everything `init` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answers {
    /// `library.root`; `None` = the default (~/.local/share/tidepool/media).
    pub root: Option<String>,
    /// (key, playlist), in the order given.
    pub playlists: Vec<(String, InitPlaylist)>,
    pub api_key: Option<String>,
    /// `schedule.interval`; "" = no schedule.
    pub interval: String,
    pub server_enabled: bool,
}

/// The default key for a playlist: its id, lowercased.
pub fn default_key(id: &str) -> String {
    id.to_ascii_lowercase()
}

/// Asks questions; implemented with dialoguer, and scripted in tests.
pub trait Ask {
    /// Free-text input; `validate` returns an error message to re-ask.
    fn input(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> anyhow::Result<String>;

    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool>;
}

/// Terminal prompts.
pub struct TerminalAsk;

impl TerminalAsk {
    /// Prompts need a terminal on both stdin and stderr.
    pub fn available() -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
}

impl Ask for TerminalAsk {
    fn input(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> anyhow::Result<String> {
        let mut input = dialoguer::Input::<String>::new()
            .with_prompt(prompt)
            .allow_empty(true)
            .validate_with(|s: &String| validate(s));
        if let Some(default) = default {
            input = input.default(default.to_string());
        }
        Ok(input.interact_text()?.trim().to_string())
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        Ok(dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(default)
            .interact()?)
    }
}

/// Answers from flags alone (`--yes`), defaults for the rest.
pub fn answers_from_flags(args: &InitArgs) -> anyhow::Result<Answers> {
    let mut playlists = Vec::new();
    for playlist in &args.playlists {
        add_playlist(&mut playlists, default_key(&playlist.id), playlist.clone())?;
    }
    Ok(Answers {
        root: args.root.clone().filter(|r| !r.is_empty()),
        playlists,
        api_key: args.api_key.clone().filter(|k| !k.is_empty()),
        interval: args
            .interval
            .clone()
            .unwrap_or_else(|| DEFAULT_INTERVAL.to_string()),
        server_enabled: !args.no_server,
    })
}

fn add_playlist(
    playlists: &mut Vec<(String, InitPlaylist)>,
    key: String,
    playlist: InitPlaylist,
) -> anyhow::Result<()> {
    if let Some((other, _)) = playlists.iter().find(|(_, p)| p.id == playlist.id) {
        anyhow::bail!("playlist {} was given twice (as {other})", playlist.id);
    }
    if playlists.iter().any(|(k, _)| *k == key) {
        anyhow::bail!("playlist key {key:?} is used twice");
    }
    playlists.push((key, playlist));
    Ok(())
}

/// Ask for whatever the flags didn't answer.
pub fn ask_answers(
    args: &InitArgs,
    ask: &mut dyn Ask,
    default_root: &str,
) -> anyhow::Result<Answers> {
    let mut answers = answers_from_flags(args)?;

    if args.root.is_none() {
        let root = ask.input("Where should videos go?", Some(default_root), &|_| Ok(()))?;
        answers.root = (root != default_root && !root.is_empty()).then_some(root);
    }

    if args.playlists.is_empty() {
        loop {
            let prompt = if answers.playlists.is_empty() {
                "Playlist URL or id to add (Enter to skip)"
            } else {
                "Another playlist URL or id (Enter to finish)"
            };
            let taken: Vec<String> = answers
                .playlists
                .iter()
                .map(|(_, p)| p.id.clone())
                .collect();
            let value = ask.input(prompt, None, &|s| {
                if s.trim().is_empty() {
                    return Ok(());
                }
                let playlist = parse_init_playlist(s.trim())?;
                if taken.contains(&playlist.id) {
                    return Err(format!("{} is already added", playlist.id));
                }
                Ok(())
            })?;
            if value.is_empty() {
                break;
            }
            let playlist = parse_init_playlist(&value).map_err(anyhow::Error::msg)?;
            let keys: Vec<String> = answers.playlists.iter().map(|(k, _)| k.clone()).collect();
            let key = ask.input(
                "Name for this playlist",
                Some(&default_key(&playlist.id)),
                &|s| {
                    if !is_valid_playlist_key(s) {
                        Err("use lowercase letters, digits, '-' and '_'".into())
                    } else if keys.iter().any(|k| k == s) {
                        Err(format!("{s:?} is already used"))
                    } else {
                        Ok(())
                    }
                },
            )?;
            add_playlist(&mut answers.playlists, key, playlist)?;
        }
    }

    if args.api_key.is_none() {
        let key = ask.input(
            "YouTube Data API key (Enter to skip and use yt-dlp only)",
            None,
            &|_| Ok(()),
        )?;
        answers.api_key = (!key.is_empty()).then_some(key);
    }

    if args.interval.is_none() {
        answers.interval = ask.input(
            "Sync interval for `tidepool run` (empty for none)",
            Some(DEFAULT_INTERVAL),
            &|s| {
                if s.is_empty() {
                    Ok(())
                } else {
                    parse_duration(s).map(drop)
                }
            },
        )?;
    }

    if !args.no_server {
        answers.server_enabled = ask.confirm(
            "Enable the web UI in `tidepool run` (http://127.0.0.1:8787)?",
            true,
        )?;
    }
    Ok(answers)
}

/// Pad a setting so its trailing comment lines up (always at least one space).
fn pad(setting: &str) -> String {
    format!("{setting:<35} ")
}

/// A TOML string literal.
fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Render the config file for `answers`.
pub fn render(answers: &Answers) -> String {
    let set_or_default = |value: Option<String>, default: &str, key: &str| match value {
        Some(value) => format!("{key} = {value}"),
        None => format!("# {key} = {default}"),
    };
    let root = set_or_default(
        answers.root.as_deref().map(q),
        "\"~/.local/share/tidepool/media\"",
        "root",
    );
    let api_key = set_or_default(answers.api_key.as_deref().map(q), "\"AIza...\"", "api_key");
    let server_enabled = set_or_default(
        (!answers.server_enabled).then(|| "false".to_string()),
        "true",
        "enabled",
    );

    let mut playlists = String::new();
    for (key, playlist) in &answers.playlists {
        playlists.push_str(&format!("[playlists.{key}]\n"));
        match &playlist.url {
            Some(url) => playlists.push_str(&format!("url = {}\n", q(url))),
            None => playlists.push_str(&format!("id = {}\n", q(&playlist.id))),
        }
        playlists.push('\n');
    }
    if answers.playlists.is_empty() {
        playlists.push_str(
            "# No playlists yet: add one like the examples below, then apply the change with\n\
             # `tidepool config import <this file>` (or pass --config <this file> to any command).\n\n",
        );
    }

    TEMPLATE
        .replace("@SCHEMA_ID@", SCHEMA_ID)
        .replace("@ROOT@", &pad(&root))
        .replace("@API_KEY@", &pad(&api_key))
        .replace(
            "@INTERVAL@",
            &pad(&format!("interval = {}", q(&answers.interval))),
        )
        .replace("@SERVER_ENABLED@", &pad(&server_enabled))
        .replace("@PLAYLISTS@\n", &playlists)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// The config file template. Commented-out settings show built-in defaults.
const TEMPLATE: &str = r#"#:schema @SCHEMA_ID@
#
# tidepool config, created by `tidepool init`.
#
# Settings that are commented out show their default. The library keeps its
# own copy of this file; after editing, apply it with
# `tidepool config import <this file>` or pass --config <this file> to any command.

version = 1

[library]
@ROOT@# where videos go
# concurrency = 2                   # parallel yt-dlp downloads
# duplicates = "hardlink"           # same video in several playlists: hardlink | symlink | copy | download
# regex_backtrack_limit = 1000000   # fancy-regex step limit per match
# shutdown_timeout = "10s"          # grace period for child processes after SIGINT/SIGTERM
# tmp_dir = ".tidepool-tmp"         # work area, relative to root (same filesystem, atomic rename)

[ytdlp]
# managed = true                    # tidepool downloads yt-dlp and keeps it updated
# channel = "nightly"               # stable | nightly | master
# update_check = "1h"               # how often to look for a new yt-dlp release
# path = "yt-dlp"                   # your own yt-dlp; only used when managed = false

[youtube]
@API_KEY@# optional; without it, indexing uses yt-dlp only
# daily_quota = 10000               # your Google Cloud project's daily quota
# quota_warn_at = 0.8               # warn at this fraction of daily_quota
# fallback_to_ytdlp = true          # with a key: use yt-dlp on quota exhaustion or API errors
# requests_per_second = 5           # API request rate limit

[schedule]
@INTERVAL@# tidepool run: sync + cleanup this often; "" = off

[server]                            # web UI inside tidepool run
@SERVER_ENABLED@# false = sync daemon only, no UI
# listen = "127.0.0.1:8787"         # UI address
# token = ""                        # required for a non-loopback listen address
# read_only_config = false          # true = config editor disabled

# Options every playlist inherits; each playlist can override any of them.
[defaults]
# format = "bv*+ba/b"               # yt-dlp format selector (-f)
# merge_output_format = "mkv"       # container for merged formats
# output = "{playlist_dir}/{upload_date} - {title} [{id}]"   # each video's directory, must contain {id}
# ytdlp_args = []                   # extra yt-dlp arguments, e.g. ["--write-subs", "--sub-langs", "en.*"]
# cookies_file = ""                 # Netscape cookies file, for private playlists
# thumbnail_format = "jpg"          # jpg | webp | original
# cover_name = "{title}-poster"     # poster file name, no extension
# match_fields = ["title"]          # title | description | channel | tags | id
# include = []                      # keep a video if any regex matches (empty = keep all)
# exclude = []                      # drop a video if any regex matches, e.g. ['(?i)#shorts']
# min_duration = ""                 # skip shorter videos, e.g. "60s"
# max_duration = ""                 # skip longer videos, e.g. "4h"
# cutoff = "none"                   # skip videos published before: "30d", "2024-01-01" or "none"
# hooks = []                        # hook names, run in order after each download
# redownload_missing = false        # download again when a downloaded file is gone
# keep_raw_info = false             # keep yt-dlp's raw info.json

[defaults.embed]                    # what goes into the media file
# metadata = false                  # title, channel, date, description, URL, album/track
# cover = false                     # the poster image (needs ffmpeg)
# chapters = false                  # chapter markers
# subtitles = false                 # subtitle tracks for --sub-langs
# info_json = false                 # attach metadata.json (mkv/mka only)
# after_hooks = true                # re-embed if a hook replaced the media file
# tags = {}                         # extra tags, e.g. { genre = "Music", album_artist = "{channel}" }

[defaults.cleanup]                  # limits that keep each playlist bounded
# max_items = 0                     # keep at most this many videos; 0 = unlimited
# max_age = ""                      # remove videos older than this, e.g. "30d"
# max_size = ""                     # remove oldest videos until media fits, e.g. "20GiB"
# order = "oldest_downloaded"       # oldest_downloaded | oldest_published | playlist_position
# removed_from_playlist = "keep"    # keep | remove videos no longer in the playlist
# keep = "metadata"                 # nothing | metadata (keep metadata.json + cover, drop media)
# marker_file = ""                  # remove videos whose directory contains this file, e.g. ".watched"
# keep_latest = 0                   # protect the newest N videos from every other limit

# Hooks: commands run after each download (or before cleanup). Example:
#
# [hooks.notify]
# command = ["ntfy", "publish", "videos", "New: {title}"]   # argv, no shell; {placeholders} expanded
# event = "downloaded"              # downloaded | before_cleanup
# timeout = "10m"                   # killed after this long
# on_failure = "fail"               # fail | warn | retry
# retries = 0                       # used by on_failure = "retry"
# env = {}                          # extra environment variables
# cwd = "."                         # working directory; defaults to the video's directory

# Playlists to mirror. Any [defaults] option can be overridden per playlist.

@PLAYLISTS@
# [playlists.example]
# url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
# dir = "example"                   # directory under root; defaults to the key
# enabled = true                    # disabled playlists are skipped
# inherit_rules = false             # true = add [defaults] include/exclude instead of replacing them
# inherit_hooks = false             # true = add [defaults] hooks instead of replacing them
# hooks = ["notify"]
#
# [playlists.example-by-id]
# id = "PLyyyyyyyy"                 # a bare playlist id works too
"#;

/// Whether init can ask questions, or must be told to use flags.
pub fn needs_terminal_error() -> anyhow::Error {
    anyhow::anyhow!(
        "tidepool init asks a few questions and needs a terminal; \
         pass --yes to use flags and defaults (e.g. --yes --root ~/Videos --playlist <url>)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_playlist;
    use crate::config::schema::generate;
    use crate::config::{
        Config, PlaylistConfig, ServerConfig, YoutubeConfig, YtdlpConfig, load_str,
    };
    use clap::Parser;
    use serde_json::Value;

    fn answers() -> Answers {
        Answers {
            root: None,
            playlists: Vec::new(),
            api_key: None,
            interval: DEFAULT_INTERVAL.into(),
            server_enabled: true,
        }
    }

    /// The template with every commented-out setting and table enabled.
    fn uncommented(text: &str) -> String {
        text.lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest)
                    if rest.starts_with('[')
                        || rest.split_once(" = ").is_some_and(|(k, _)| {
                            !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        }) =>
                {
                    rest
                }
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn default_answers_render_a_valid_config() {
        let text = render(&answers());
        assert!(text.starts_with(&format!("#:schema {SCHEMA_ID}\n")));
        let loaded = load_str("init.toml", &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(loaded.warnings.is_empty());
        let config = loaded.config;
        assert_eq!(config.library.root, None);
        assert_eq!(config.youtube, YoutubeConfig::default());
        assert_eq!(config.server, ServerConfig::default());
        assert_eq!(
            config.schedule.interval.0,
            Some(std::time::Duration::from_secs(6 * 3600))
        );
        assert!(config.playlists.is_empty());
        assert!(text.contains("# No playlists yet"));
        assert!(text.contains("# root = \"~/.local/share/tidepool/media\" # where videos go"));
    }

    #[test]
    fn answers_are_written_uncommented() {
        let text = render(&Answers {
            root: Some("~/Videos/yt \"x\"".into()),
            playlists: vec![
                (
                    "plabc".into(),
                    InitPlaylist {
                        id: "PLabc".into(),
                        url: None,
                    },
                ),
                (
                    "lofi".into(),
                    InitPlaylist {
                        id: "PLdef".into(),
                        url: Some("https://www.youtube.com/playlist?list=PLdef".into()),
                    },
                ),
            ],
            api_key: Some("AIzaSyD-1234567890x1Qw".into()),
            interval: String::new(),
            server_enabled: false,
        });
        let config = load_str("init.toml", &text)
            .unwrap_or_else(|e| panic!("{e}\n{text}"))
            .config;
        assert_eq!(config.library.root.as_deref(), Some("~/Videos/yt \"x\""));
        assert_eq!(
            config.youtube.api_key.as_deref(),
            Some("AIzaSyD-1234567890x1Qw")
        );
        assert_eq!(config.schedule.interval.0, None);
        assert!(!config.server.enabled);
        assert_eq!(config.playlists["plabc"].id.as_deref(), Some("PLabc"));
        assert!(
            config.playlists["lofi"]
                .url
                .as_deref()
                .unwrap()
                .ends_with("list=PLdef")
        );
        assert!(!text.contains("No playlists yet"));
    }

    #[test]
    fn template_documents_every_option_with_its_real_default() {
        let text = uncommented(&render(&answers()));
        let config = load_str("uncommented.toml", &text)
            .unwrap_or_else(|e| panic!("uncommented template is invalid: {e}\n{text}"))
            .config;

        // Commented values are the built-in defaults.
        assert_eq!(config.ytdlp, YtdlpConfig::default());
        let mut youtube = config.youtube.clone();
        youtube.api_key = None;
        assert_eq!(youtube, YoutubeConfig::default());
        assert_eq!(config.server, ServerConfig::default());
        let mut library = config.library.clone();
        library.root = None;
        assert_eq!(library, crate::config::LibraryConfig::default());
        let plain = PlaylistConfig {
            id: Some("PLx".into()),
            ..PlaylistConfig::default()
        };
        assert_eq!(
            effective_playlist("x", &plain, &config.defaults),
            effective_playlist("x", &plain, &Config::default().defaults),
            "[defaults] in the template must show the built-in defaults"
        );

        // Every schema property appears in the template.
        let json: Value = toml::from_str(&text).unwrap();
        let schema = generate().to_value();
        let props = |def: &str| -> Vec<String> {
            let node = if def.is_empty() {
                &schema
            } else {
                &schema["$defs"][def]
            };
            node["properties"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        };
        let where_documented: &[(&str, &[&str])] = &[
            ("", &[""]),
            ("LibraryConfig", &["/library"]),
            ("YtdlpConfig", &["/ytdlp"]),
            ("YoutubeConfig", &["/youtube"]),
            ("ScheduleConfig", &["/schedule"]),
            ("ServerConfig", &["/server"]),
            ("Defaults", &["/defaults"]),
            ("EmbedOptions", &["/defaults/embed"]),
            ("CleanupOptions", &["/defaults/cleanup"]),
            ("HookConfig", &["/hooks/notify"]),
            (
                "PlaylistConfig",
                &["/playlists/example", "/playlists/example-by-id"],
            ),
        ];
        let defaults_props = props("Defaults");
        for (def, pointers) in where_documented {
            for prop in props(def) {
                if *def == "PlaylistConfig" && defaults_props.contains(&prop) {
                    continue; // documented under [defaults]
                }
                let found = pointers
                    .iter()
                    .any(|p| json.pointer(&format!("{p}/{prop}")).is_some());
                assert!(found, "{def}.{prop} is missing from the init template");
            }
        }
        // New config tables must be added to the check above.
        let defs = schema["$defs"].as_object().unwrap();
        for (name, def) in defs {
            if def.get("properties").is_some() && name != "FieldRule" {
                assert!(
                    where_documented.iter().any(|(d, _)| d == name),
                    "add {name} to the template coverage check"
                );
            }
        }
    }

    fn args(argv: &[&str]) -> InitArgs {
        let mut full = vec!["tidepool", "init"];
        full.extend_from_slice(argv);
        match crate::cli::Cli::parse_from(full).command {
            crate::cli::Command::Init(args) => args,
            _ => unreachable!(),
        }
    }

    #[test]
    fn flags_alone_use_defaults_and_lowercased_ids() {
        let answers = answers_from_flags(&args(&[
            "--yes",
            "-p",
            "PLAbc_DEF",
            "-p",
            "https://www.youtube.com/playlist?list=PLxyz",
        ]))
        .unwrap();
        let keys: Vec<&str> = answers.playlists.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["plabc_def", "plxyz"]);
        assert_eq!(answers.interval, "6h");
        assert!(answers.server_enabled && answers.root.is_none() && answers.api_key.is_none());

        let err =
            answers_from_flags(&args(&["-p", "PLa", "-p", "https://x/?list=PLa"])).unwrap_err();
        assert!(err.to_string().contains("given twice"));
    }

    /// Replays answers; records the prompts asked.
    struct Scripted {
        replies: std::collections::VecDeque<&'static str>,
        asked: Vec<String>,
    }

    impl Ask for Scripted {
        fn input(
            &mut self,
            prompt: &str,
            default: Option<&str>,
            validate: &dyn Fn(&str) -> Result<(), String>,
        ) -> anyhow::Result<String> {
            loop {
                let reply = self.replies.pop_front().expect("no scripted reply left");
                self.asked.push(prompt.to_string());
                let value = if reply.is_empty() {
                    default.unwrap_or("")
                } else {
                    reply
                };
                if validate(value).is_ok() {
                    return Ok(value.to_string());
                }
            }
        }

        fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
            self.asked.push(prompt.to_string());
            Ok(
                match self.replies.pop_front().expect("no scripted reply left") {
                    "y" => true,
                    "n" => false,
                    _ => default,
                },
            )
        }
    }

    #[test]
    fn asks_for_what_flags_did_not_answer() {
        let mut ask = Scripted {
            replies: [
                "~/Videos",                                   // root
                "not a url!",                                 // rejected, asked again
                "https://www.youtube.com/playlist?list=PLaa", // playlist
                "Bad Key",                                    // rejected, asked again
                "lofi",                                       // key
                "PLbb",                                       // second playlist
                "",                                           // key: default (plbb)
                "PLaa",                                       // duplicate, asked again
                "",                                           // done
                "",                                           // no API key
                "soon",                                       // rejected
                "12h",                                        // interval
                "n",                                          // no web UI
            ]
            .into(),
            asked: Vec::new(),
        };
        let answers = ask_answers(&args(&[]), &mut ask, "~/.local/share/tidepool/media").unwrap();
        assert!(ask.replies.is_empty(), "unused replies: {:?}", ask.replies);
        assert_eq!(answers.root.as_deref(), Some("~/Videos"));
        let keys: Vec<&str> = answers.playlists.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["lofi", "plbb"]);
        assert_eq!(answers.api_key, None);
        assert_eq!(answers.interval, "12h");
        assert!(!answers.server_enabled);
    }

    #[test]
    fn flags_skip_their_questions() {
        let mut ask = Scripted {
            replies: ["y"].into(),
            asked: Vec::new(),
        };
        let answers = ask_answers(
            &args(&[
                "--root",
                "/v",
                "-p",
                "PLaa",
                "--api-key",
                "k",
                "--interval",
                "1d",
            ]),
            &mut ask,
            "/default",
        )
        .unwrap();
        assert_eq!(
            ask.asked.len(),
            1,
            "only the web UI question: {:?}",
            ask.asked
        );
        assert_eq!(answers.root.as_deref(), Some("/v"));
        assert_eq!(answers.interval, "1d");

        // Accepting the default root leaves it unset (commented out).
        let mut ask = Scripted {
            replies: [""].into(),
            asked: Vec::new(),
        };
        let answers = ask_answers(
            &args(&[
                "-p",
                "PLaa",
                "--api-key",
                "k",
                "--interval",
                "1d",
                "--no-server",
            ]),
            &mut ask,
            "/default",
        )
        .unwrap();
        assert_eq!(answers.root, None);
    }
}
