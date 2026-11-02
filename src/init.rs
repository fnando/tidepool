//! `tidepool init`: write a commented config file and store it
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

[general]                           # settings for the whole run
# concurrency = 2                   # parallel yt-dlp downloads
# process_concurrency = 1           # videos processed at once after downloading (hooks, embedding)
# duplicates = "hardlink"           # same video in several sources: hardlink | symlink | copy | download
# regex_backtrack_limit = 1000000   # fancy-regex step limit per match
# shutdown_timeout = "10s"          # grace period for child processes after SIGINT/SIGTERM

[tools.yt-dlp]
# managed = true                    # tidepool downloads yt-dlp and keeps it updated
# channel = "nightly"               # stable | nightly | master
# update_check = "1h"               # how often to look for a new yt-dlp release
# path = "yt-dlp"                   # your own yt-dlp; only used when managed = false

[tools.deno]                        # the JavaScript runtime yt-dlp needs for YouTube
# managed = true                    # tidepool downloads Deno 2.x and keeps it updated
# update_check = "1d"               # how often to look for a new Deno release
# path = "deno"                     # your own deno; only used when managed = false

[schedule]
interval = "6h"                     # tidepool run --daemon: sync + cleanup this often; "" = off

[server]                            # web UI served by tidepool run --daemon
# enabled = true                    # false = sync daemon only, no UI
# listen = "127.0.0.1:8787"         # UI address
# token = ""                        # required for a non-loopback listen address
# read_only_config = false          # true = config editor disabled

# Categories group settings. Every source uses [category.default] unless it
# picks another category; other categories only list what differs from
# default, and a source can override any of these settings for itself.

[category.default]
# media_dir = "~/.local/share/tidepool/media" # where videos go; {media_dir} in output
# tmp_dir = "{data_dir}/tmp"        # work area for downloads in progress
# trash_dir = "{data_dir}/trash"    # where `tidepool cleanup` puts removed files; on the media's disk (e.g. "{media_dir}/.trash") moving there is instant
# format = "bv*+ba/b/bv*/ba"        # best video + audio, else best single file, else video or audio alone
# merge_output_format = "mkv"       # container for merged formats
# output_dir = "{media_dir}/{source}/{upload_date} - {title} [{id}]" # each video's folder; {output_dir} below
# output_file = "{output_dir}/{normalized_title}.{ext}"          # the media file; must contain {ext}
# cover_file = "{output_dir}/{normalized_title}-poster.jpg"      # the poster; .jpg, .png or .webp picks the format
# subtitle_file = "{output_dir}/{normalized_title}.{lang}.{ext}" # each subtitle; must contain {lang} and {ext}
# metadata_file = "{output_dir}/metadata.json"                   # tidepool's metadata
# ytdlp_args = []                   # extra yt-dlp arguments, e.g. ["--write-subs", "--sub-langs", "en.*"]
# cookies_file = ""                 # Netscape cookies file, for private playlists
# match_fields = ["title"]          # title | description | channel | tags | id
# include = []                      # keep a video if any regex matches (empty = keep all)
# exclude = []                      # drop a video if any regex matches, e.g. ['(?i)trailer'], or by kind: [{ field = "kind", value = "short" }] (video, short, live)
# inherit_rules = false             # elsewhere: true = add to the include/exclude inherited instead of replacing them
# min_duration = ""                 # skip shorter videos, e.g. "60s"
# max_duration = ""                 # skip longer videos, e.g. "4h"
# cutoff = "none"                   # skip videos published before: "30d", "2024-01-01" or "none"
# hooks = []                        # hook names; each runs at its type's stage, in this order
# inherit_hooks = false             # elsewhere: true = add to the hooks inherited instead of replacing them
# redownload_missing = false        # restore files that went missing (else each run reports them)
# keep_raw_info = false             # keep yt-dlp's raw info.json

[category.default.youtube]          # YouTube Data API (optional)
# api_key = "AIza..."               # without one, indexing uses yt-dlp only
# daily_quota = 10000               # the key's Google Cloud project daily quota
# quota_warn_at = 0.8               # warn at this fraction of daily_quota
# fallback_to_ytdlp = true          # with a key: use yt-dlp on quota exhaustion or API errors
# requests_per_second = 5           # API request rate limit

[category.default.embed]            # what goes into the media file
# metadata = false                  # title, artist/album artist (channel), genre, date, description, URL, album (source title), track
# cover = false                     # the poster, as in cover_file (not for webm, opus or ogg)
# chapters = false                  # chapter markers
# subtitles = false                 # subtitles from --write-subs; the subtitle files are kept too
# tags = {}                         # tags to add or replace (with metadata = true), e.g. { genre = "Music", album_artist = "{channel}" }; "" removes one
# musicbrainz = { server = "https://musicbrainz.org", enabled = true } # add album/track ids to videos tagged MUSICBRAINZ_ARTISTID (mkv); off unless set

[category.default.cleanup]          # what `tidepool cleanup` removes; nothing unless set
# max_items = 0                     # keep at most this many videos (and download only the first ones); 0 = unlimited
# max_age = ""                      # remove videos downloaded longer ago than this, e.g. "30d"
# max_size = ""                     # remove videos until the source's media fits, e.g. "20GiB"
# order = "oldest_downloaded"       # who goes first for max_items/max_size: oldest_downloaded | oldest_published | source_position
# unmatched = "keep"                # keep | remove videos your include/exclude rules no longer match
# keep = "metadata"                 # nothing | metadata (keep metadata.json + cover, drop media and subtitles)
# marker_file = ""                  # remove videos whose folder contains this file, e.g. ".watched"
# keep_latest = 0                   # protect the newest N videos from every other limit
# delete = "trash"                  # trash: restorable with `tidepool trash restore` for trash_for | now
# trash_for = "7d"                  # how long removed files stay in trash_dir

[category.music]                    # an example: audio only, for sources with category = "music"
# media_dir = "~/Music/tidepool"    # e.g. keep music somewhere else
format = "ba/b"                     # best audio
ytdlp_args = ["-x", "--audio-format", "opus"]
output_dir = "{media_dir}/{source}/{title} [{id}]"
cover_file = "{output_dir}/cover.jpg"

# Hooks: your own commands, run at each stage of a download for the sources
# that list them in `hooks`. Examples:
#
# [hooks.clean-tags]
# type = "embed"                    # gets the tags, cover, chapters and subtitles about to be embedded as JSON on stdin; prints them back changed
# command = ["~/bin/clean-tags"]
#
# [hooks.notify]
# command = ["ntfy", "publish", "videos", "New: {title}"]   # argv, no shell; {placeholders} expanded
# type = "finished"                 # prepare (details, paths) | downloaded (raw files) | embed | finished (files in place) | before_cleanup
# timeout = "10m"                   # killed after this long
# on_failure = "fail"               # fail | warn | retry
# retries = 0                       # used by on_failure = "retry"
# env = {}                          # extra environment variables
# work_dir = "~"                    # working directory, a full path; defaults to the video's directory

# Sources to keep in sync: YouTube playlists or channels. A source can set any category
# setting for itself.

[sources.sample]                    # one short video, to see tidepool work; replace it with your own
urls = ["@SAMPLE_SOURCE_URL@"]   # playlist or channel URLs (https://www.youtube.com/@name), saved together
# category = "default"              # which category's settings to use, e.g. "music"
# enabled = true                    # disabled sources are skipped
# hooks = ["notify"]                # hook names; each runs at its type's stage, in this order
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_source;
    use crate::config::schema::generate;
    use crate::config::{Config, GeneralConfig, ServerConfig, SourceConfig, ToolsConfig, load_str};
    use serde_json::Value;

    #[test]
    fn renders_a_valid_config_with_the_sample_source() {
        let text = render();
        assert!(text.starts_with(&format!("#:schema {SCHEMA_ID}\n")));
        let loaded = load_str("init.toml", &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let config = loaded.config;
        assert_eq!(config.general, GeneralConfig::default());
        assert_eq!(config.server, ServerConfig::default());
        let categories: Vec<&String> = config.category.keys().collect();
        assert_eq!(categories, ["default", "music"]);
        assert_eq!(config.category["music"].format.as_deref(), Some("ba/b"));
        assert_eq!(
            config.schedule.interval.0,
            Some(std::time::Duration::from_secs(6 * 3600))
        );
        let keys: Vec<&String> = config.sources.keys().collect();
        assert_eq!(keys, ["sample"]);
        assert_eq!(config.sources["sample"].urls, [SAMPLE_SOURCE_URL]);
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
        assert_eq!(config.general, GeneralConfig::default());
        assert_eq!(config.tools, ToolsConfig::default());
        assert_eq!(config.server, ServerConfig::default());
        let plain = SourceConfig {
            urls: vec!["https://www.youtube.com/playlist?list=PLx".into()],
            ..SourceConfig::default()
        };
        let mut documented = effective_source(&config, "x", &plain);
        let builtin = effective_source(&Config::default(), "x", &plain);
        // The template shows these as examples rather than defaults.
        assert_eq!(
            documented.media_dir.as_deref(),
            Some("~/.local/share/tidepool/media")
        );
        documented.media_dir = None;
        documented.youtube.api_key = None;
        documented.embed.musicbrainz = None;
        assert_eq!(
            documented, builtin,
            "[category.default] in the template must show the built-in defaults"
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
            ("GeneralConfig", &["/general"]),
            ("ToolsConfig", &["/tools"]),
            ("YtdlpConfig", &["/tools/yt-dlp"]),
            ("DenoConfig", &["/tools/deno"]),
            ("ScheduleConfig", &["/schedule"]),
            ("ServerConfig", &["/server"]),
            ("CategoryConfig", &["/category/default"]),
            ("YoutubeOptions", &["/category/default/youtube"]),
            ("EmbedOptions", &["/category/default/embed"]),
            ("CleanupOptions", &["/category/default/cleanup"]),
            (
                "MusicBrainzOptions",
                &["/category/default/embed/musicbrainz"],
            ),
            ("HookConfig", &["/hooks/notify"]),
            ("SourceConfig", &["/sources/sample"]),
        ];
        let category_props = props("CategoryConfig");
        for (def, pointers) in where_documented {
            for prop in props(def) {
                if *def == "SourceConfig" && category_props.contains(&prop) {
                    continue; // documented under [category.default]
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
            if def.get("properties").is_some()
                && !matches!(name.as_str(), "FieldRule" | "ValueRule")
            {
                assert!(
                    where_documented.iter().any(|(d, _)| d == name),
                    "add {name} to the template coverage check"
                );
            }
        }
    }
}
