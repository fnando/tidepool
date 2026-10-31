//! `tidepool init`: write a commented config file and create the library
//! behind it, without asking anything.
//!
//! The generated file lists every option with its default and a one-line
//! comment; options left at their default are commented out, so the file
//! doubles as documentation. It includes one short sample source so the
//! first sync has something to do. A test keeps it in sync with the schema.

use crate::config::schema::SCHEMA_ID;

/// The sample source in a new config: one short, unlisted video.
pub const SAMPLE_SOURCE_URL: &str = "https://www.youtube.com/playlist?list=PLXLpuA-bwY4s";

/// The config file `init` writes.
pub fn render() -> String {
    TEMPLATE
        .replace("@SCHEMA_ID@", SCHEMA_ID)
        .replace("@SAMPLE_SOURCE_URL@", SAMPLE_SOURCE_URL)
}

/// The config file template. Commented-out settings show built-in defaults.
const TEMPLATE: &str = r#"#:schema @SCHEMA_ID@
#
# tidepool config, created by `tidepool init`.
#
# Settings that are commented out show their default. tidepool keeps its own
# copy of this file; after editing, apply it with
# `tidepool config import <this file>` or pass --config <this file> to any command.

version = 1

[library]
# root = "~/.local/share/tidepool/media" # where videos go
# concurrency = 2                   # parallel yt-dlp downloads
# duplicates = "hardlink"           # same video in several sources: hardlink | symlink | copy | download
# regex_backtrack_limit = 1000000   # fancy-regex step limit per match
# shutdown_timeout = "10s"          # grace period for child processes after SIGINT/SIGTERM
# tmp_dir = ".tidepool-tmp"         # work area, relative to root (same filesystem, atomic rename)

[ytdlp]
# managed = true                    # tidepool downloads yt-dlp and keeps it updated
# channel = "nightly"               # stable | nightly | master
# update_check = "1h"               # how often to look for a new yt-dlp release
# path = "yt-dlp"                   # your own yt-dlp; only used when managed = false

[youtube]
# api_key = "AIza..."               # optional; without it, indexing uses yt-dlp only
# daily_quota = 10000               # your Google Cloud project's daily quota
# quota_warn_at = 0.8               # warn at this fraction of daily_quota
# fallback_to_ytdlp = true          # with a key: use yt-dlp on quota exhaustion or API errors
# requests_per_second = 5           # API request rate limit

[schedule]
interval = "6h"                     # tidepool run --daemon: sync + cleanup this often; "" = off

[server]                            # web UI served by tidepool run --daemon
# enabled = true                    # false = sync daemon only, no UI
# listen = "127.0.0.1:8787"         # UI address
# token = ""                        # required for a non-loopback listen address
# read_only_config = false          # true = config editor disabled

# Options every source inherits; each source can override any of them.
[defaults]
# format = "bv*+ba/b"               # yt-dlp format selector (-f)
# merge_output_format = "mkv"       # container for merged formats
# output = "{source_dir}/{upload_date} - {title} [{id}]"   # each video's directory, must contain {id}
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

[defaults.cleanup]                  # limits that keep each source bounded
# max_items = 0                     # keep at most this many videos; 0 = unlimited
# max_age = ""                      # remove videos older than this, e.g. "30d"
# max_size = ""                     # remove oldest videos until media fits, e.g. "20GiB"
# order = "oldest_downloaded"       # oldest_downloaded | oldest_published | source_position
# removed_from_source = "keep"      # keep | remove videos no longer in the source
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

# Sources to keep in sync (YouTube playlists). Any [defaults] option can be
# overridden per source.

[sources.sample]                    # one short video, to see tidepool work; replace it with your own
url = "@SAMPLE_SOURCE_URL@"   # the playlist's URL
# dir = "sample"                    # folder under root; defaults to the source name
# enabled = true                    # disabled sources are skipped
# inherit_rules = false             # true = add [defaults] include/exclude instead of replacing them
# inherit_hooks = false             # true = add [defaults] hooks instead of replacing them
# hooks = ["notify"]                # hook names, run in order after each download
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_source;
    use crate::config::schema::generate;
    use crate::config::{Config, ServerConfig, SourceConfig, YoutubeConfig, YtdlpConfig, load_str};
    use serde_json::Value;

    #[test]
    fn renders_a_valid_config_with_the_sample_source() {
        let text = render();
        assert!(text.starts_with(&format!("#:schema {SCHEMA_ID}\n")));
        let loaded = load_str("init.toml", &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let config = loaded.config;
        assert_eq!(config.library.root, None);
        assert_eq!(config.youtube, YoutubeConfig::default());
        assert_eq!(config.server, ServerConfig::default());
        assert_eq!(
            config.schedule.interval.0,
            Some(std::time::Duration::from_secs(6 * 3600))
        );
        let keys: Vec<&String> = config.sources.keys().collect();
        assert_eq!(keys, ["sample"]);
        assert_eq!(config.sources["sample"].url, SAMPLE_SOURCE_URL);
        assert!(
            text.lines().all(|l| l == l.trim_end()),
            "trailing whitespace"
        );
        assert!(!text.contains("\"# "), "a value runs into its comment");
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
    fn template_documents_every_option_with_its_real_default() {
        let text = uncommented(&render());
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
        let plain = SourceConfig {
            url: "https://www.youtube.com/playlist?list=PLx".into(),
            ..SourceConfig::default()
        };
        assert_eq!(
            effective_source("x", &plain, &config.defaults),
            effective_source("x", &plain, &Config::default().defaults),
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
            ("SourceConfig", &["/sources/sample"]),
        ];
        let defaults_props = props("Defaults");
        for (def, pointers) in where_documented {
            for prop in props(def) {
                if *def == "SourceConfig" && defaults_props.contains(&prop) {
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
}
