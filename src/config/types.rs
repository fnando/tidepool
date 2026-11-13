//! The config file's shape. These types generate the JSON Schema (via
//! `schemars`) and are deserialized only after the schema validated the input.
//!
//! `[category.<name>]` and `[sources.<name>]` share every option. A source's
//! settings come from the built-in defaults, then `[category.default]`, then
//! its category, then the source itself (see [`super::merge`]).

use std::collections::BTreeMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::duration::{Cutoff, parse_duration};
use super::size::parse_size;

/// A tidepool config file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "tidepool config", transform = super::schema::transform_root)]
pub struct Config {
    /// Config format version. Optional; when set, must be 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1))]
    pub version: Option<u32>,
    /// Settings for the whole run: concurrency, duplicates, shutdown.
    #[serde(default)]
    pub general: GeneralConfig,
    /// The tools tidepool runs: yt-dlp and Deno.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// Scheduling for `tidepool run --daemon`.
    #[serde(default)]
    pub schedule: ScheduleConfig,
    /// The web UI served by `tidepool run --daemon`.
    #[serde(default)]
    pub server: ServerConfig,
    /// Groups of settings sources pick with `category`; `default` applies to
    /// every source, other categories on top of it.
    #[serde(default)]
    pub category: BTreeMap<String, CategoryConfig>,
    /// Named post-download hooks, attached to sources by name.
    #[serde(default)]
    pub hooks: BTreeMap<String, HookConfig>,
    /// Sources to keep in sync, by name.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
}

/// `[general]`: settings for the whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct GeneralConfig {
    /// Parallel yt-dlp downloads.
    #[schemars(range(min = 1))]
    pub concurrency: u32,
    /// Videos processed at once after downloading: hooks (a transcode, …),
    /// embedding, saving.
    #[schemars(range(min = 1))]
    pub process_concurrency: u32,
    /// What to do when the same video is in several sources.
    pub duplicates: Duplicates,
    /// fancy-regex backtracking step limit per match.
    #[schemars(range(min = 1))]
    pub regex_backtrack_limit: u64,
    /// Grace period for child processes after SIGINT/SIGTERM.
    pub shutdown_timeout: HumanDuration,
    /// Download videos; false = only check sources (index) unless a category
    /// or source sets `download` itself.
    pub download: bool,
    /// Where you put videos you downloaded yourself, to hand them to tidepool
    /// from a video's page in the web UI. A full path (`{data_dir}` allowed).
    #[schemars(extend("format" = "trash-dir-template"))]
    pub backfill_dir: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            download: true,
            concurrency: 2,
            process_concurrency: 1,
            duplicates: Duplicates::Hardlink,
            regex_backtrack_limit: 1_000_000,
            shutdown_timeout: HumanDuration(Duration::from_secs(10)),
            backfill_dir: "{data_dir}/backfill".into(),
        }
    }
}

/// How titles are cleaned up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanTitle {
    /// As YouTube has it.
    #[default]
    None,
    /// The song's title only, for music videos.
    MusicVideo,
}

/// Same video in several sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Duplicates {
    Hardlink,
    Symlink,
    Copy,
    Download,
}

/// `[tools]`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ToolsConfig {
    /// yt-dlp: managed by tidepool (default) or your own binary.
    #[serde(rename = "yt-dlp")]
    pub ytdlp: YtdlpConfig,
    /// Deno, the JavaScript runtime yt-dlp needs for YouTube.
    pub deno: DenoConfig,
}

/// `[tools.deno]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct DenoConfig {
    /// tidepool downloads Deno 2.x and keeps it updated.
    pub managed: bool,
    /// How often to look for a new release.
    pub update_check: HumanDuration,
    /// Deno executable; only used when `managed = false`.
    #[schemars(length(min = 1))]
    pub path: String,
}

impl Default for DenoConfig {
    fn default() -> Self {
        Self {
            managed: true,
            update_check: HumanDuration(Duration::from_hours(24)),
            path: "deno".into(),
        }
    }
}

/// `[tools.yt-dlp]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct YtdlpConfig {
    /// tidepool downloads yt-dlp and keeps it updated.
    pub managed: bool,
    /// Release channel for the managed binary.
    pub channel: YtdlpChannel,
    /// How often to look for a new release.
    pub update_check: HumanDuration,
    /// yt-dlp executable; only used when `managed = false`.
    #[schemars(length(min = 1))]
    pub path: String,
}

impl Default for YtdlpConfig {
    fn default() -> Self {
        Self {
            managed: true,
            channel: YtdlpChannel::Nightly,
            update_check: HumanDuration(Duration::from_secs(3600)),
            path: "yt-dlp".into(),
        }
    }
}

/// yt-dlp release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum YtdlpChannel {
    Stable,
    Nightly,
    Master,
}

impl YtdlpChannel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            YtdlpChannel::Stable => "stable",
            YtdlpChannel::Nightly => "nightly",
            YtdlpChannel::Master => "master",
        }
    }

    /// GitHub repository that publishes this channel's releases.
    #[must_use]
    pub fn repository(self) -> &'static str {
        match self {
            YtdlpChannel::Stable => "yt-dlp/yt-dlp",
            YtdlpChannel::Nightly => "yt-dlp/yt-dlp-nightly-builds",
            YtdlpChannel::Master => "yt-dlp/yt-dlp-master-builds",
        }
    }
}

/// `youtube` in a category or source: YouTube Data API settings, merged key
/// by key like the other options.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct YoutubeOptions {
    /// YouTube Data API v3 key, or `$NAME` to read it from that environment
    /// variable. Optional; without it, indexing uses yt-dlp only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub api_key: Option<String>,
    /// Daily quota of the key's Google Cloud project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub daily_quota: Option<u32>,
    /// Warn when this fraction of `daily_quota` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub quota_warn_at: Option<f64>,
    /// With an API key: use yt-dlp on quota exhaustion, a rejected key or repeated 5xx.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_to_ytdlp: Option<bool>,
    /// API request rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub requests_per_second: Option<u32>,
}

/// Resolved YouTube settings for a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct YoutubeConfig {
    /// YouTube Data API v3 key. Optional; without it, indexing uses yt-dlp only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub api_key: Option<String>,
    /// Daily quota of the Google Cloud project.
    #[schemars(range(min = 1))]
    pub daily_quota: u32,
    /// Warn when this fraction of `daily_quota` is used.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub quota_warn_at: f64,
    /// With an API key: use yt-dlp on quota exhaustion, a rejected key or repeated 5xx.
    pub fallback_to_ytdlp: bool,
    /// API request rate limit.
    #[schemars(range(min = 1))]
    pub requests_per_second: u32,
}

impl Default for YoutubeConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            daily_quota: 10_000,
            quota_warn_at: 0.8,
            fallback_to_ytdlp: true,
            requests_per_second: 5,
        }
    }
}

/// `[schedule]`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ScheduleConfig {
    /// Run sync + cleanup this often; "" = off.
    pub interval: OptionalDuration,
}

/// `[server]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Serve the web UI from `tidepool run --daemon`; false = sync daemon only.
    pub enabled: bool,
    /// Address the web UI listens on, e.g. "127.0.0.1:8787".
    #[schemars(extend("format" = "socket-address"))]
    pub listen: String,
    /// Access token, or `$NAME` to read it from that environment variable;
    /// required for a non-loopback `listen` address. "" = none.
    pub token: String,
    /// Disable the config editor in the web UI.
    pub read_only_config: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:8787".into(),
            token: String::new(),
            read_only_config: false,
        }
    }
}

/// A hook: a command run after a video is downloaded or before it is cleaned up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// argv array; `{placeholder}`s are expanded inside each element, no shell.
    #[schemars(schema_with = "hook_command_schema")]
    pub command: Vec<String>,
    /// When the hook runs: `prepare` (first; may change the video's details
    /// and paths), `downloaded` (raw files, before embedding; may replace
    /// them), `embed` (rewrites what gets embedded), `finished`
    /// (after the files are in place) or `before_cleanup`.
    #[serde(default, rename = "type")]
    pub kind: HookType,
    /// SIGTERM to the process group after this long, SIGKILL 10 s later.
    #[serde(default = "default_hook_timeout")]
    pub timeout: HumanDuration,
    /// What a non-zero exit does.
    #[serde(default)]
    pub on_failure: OnFailure,
    /// Retries when `on_failure = "retry"`.
    #[serde(default)]
    pub retries: u32,
    /// Extra environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory. Defaults to the video's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
}

fn hook_command_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "items": { "type": "string", "format": "hook-template" },
    })
}

fn tags_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": { "type": "string", "format": "tag-template" },
    })
}

fn default_hook_timeout() -> HumanDuration {
    HumanDuration(Duration::from_secs(600))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    /// First, after yt-dlp: may change the video's details (title,
    /// description, …) and where its files go.
    Prepare,
    /// On the raw files in the work area, before embedding; may replace them.
    Downloaded,
    /// Receives the data about to be embedded as JSON; returns it processed.
    Embed,
    /// After the video's files are in place.
    #[default]
    Finished,
    /// Before files are deleted; a non-zero exit vetoes the cleanup.
    BeforeCleanup,
}

impl HookType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Downloaded => "downloaded",
            Self::Embed => "embed",
            Self::Finished => "finished",
            Self::BeforeCleanup => "before_cleanup",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// Stop the chain; the item becomes `hook_failed`.
    #[default]
    Fail,
    /// Log and continue.
    Warn,
    /// Retry up to `retries` times, then fail.
    Retry,
}

/// Defines a struct holding every shared option, all optional, plus any extra
/// fields, and its conversion to [`Options`]. Used for `[category.<name>]` and
/// `[sources.<name>]` so they can't drift apart (serde's `flatten` doesn't
/// work with `deny_unknown_fields`).
macro_rules! options_struct {
    (
        $(#[$meta:meta])*
        pub struct $name:ident { $($extra:tt)* }
    ) => {
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        $(#[$meta])*
        pub struct $name {
            $($extra)*
            /// The media folder, a full path; path settings refer to it as `{media_dir}`.
            /// Defaults to `~/.local/share/tidepool/media`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "full-path"))]
            pub media_dir: Option<String>,
            /// Work area for downloads in progress, a full path (`{data_dir}` and
            /// `{media_dir}` allowed). Defaults to `{data_dir}/tmp`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "tmp-dir-template"))]
            pub tmp_dir: Option<String>,
            /// Where cleanup puts removed files until they're deleted for good, a
            /// full path (`{data_dir}` and `{media_dir}` allowed). Defaults to
            /// `{data_dir}/trash`; on the media's disk, moving there is instant.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "trash-dir-template"))]
            pub trash_dir: Option<String>,
            /// yt-dlp format selector (`-f`).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub format: Option<String>,
            /// Container for merged formats (`--merge-output-format`).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub merge_output_format: Option<String>,
            /// Each video's directory, a full path; the file settings refer to it
            /// as `{output_dir}`. Defaults to `{media_dir}/{source}/{upload_date} - {title} [{id}]`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "output-dir-template"))]
            pub output_dir: Option<String>,
            /// The media file, a full path; must contain `{ext}`.
            /// Defaults to `{output_dir}/{normalized_title}.{ext}`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "output-file-template"))]
            pub output_file: Option<String>,
            /// The poster, a full path ending in .jpg, .png or .webp (which picks
            /// the image format). Defaults to `{output_dir}/{normalized_title}-poster.jpg`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "cover-file-template"))]
            pub cover_file: Option<String>,
            /// Each subtitle file, a full path; must contain `{lang}` and `{ext}`.
            /// Defaults to `{output_dir}/{normalized_title}.{lang}.{ext}`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "subtitle-file-template"))]
            pub subtitle_file: Option<String>,
            /// tidepool's `metadata.json`, a full path. Defaults to `{output_dir}/metadata.json`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "metadata-file-template"))]
            pub metadata_file: Option<String>,
            /// Extra yt-dlp arguments, passed through.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub ytdlp_args: Option<Vec<String>>,
            /// Netscape cookies file, for private sources. "" = none.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub cookies_file: Option<String>,
            /// Field(s) plain-string rules are tested against.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(length(min = 1))]
            pub match_fields: Option<Vec<RuleField>>,
            /// Keep an item if any rule matches (empty = keep all).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub include: Option<Vec<Rule>>,
            /// Drop an item if any rule matches. Wins over include.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub exclude: Option<Vec<Rule>>,
            /// Add include/exclude to the ones inherited (category, then `default`) instead of replacing them.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub inherit_rules: Option<bool>,
            /// Skip videos shorter than this.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub min_duration: Option<OptionalDuration>,
            /// Skip videos longer than this.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub max_duration: Option<OptionalDuration>,
            /// Skip videos published before this: "30d", "2024-01-01" or "none".
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub cutoff: Option<CutoffValue>,
            /// Hook names, run in order after each download.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub hooks: Option<Vec<String>>,
            /// Add hooks to the ones inherited instead of replacing them.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub inherit_hooks: Option<bool>,
            /// Restore a downloaded video's missing files (video, poster, subtitles,
            /// metadata); otherwise each run reports them.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub redownload_missing: Option<bool>,
            /// Keep yt-dlp's raw info JSON next to the metadata file (as `<metadata file name>.ytdlp.json`).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub keep_raw_info: Option<bool>,
            /// Download its videos; false = only check the source (index).
            /// Defaults to `general.download`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub download: Option<bool>,
            /// Clean up YouTube titles: `music_video` keeps only the song's title
            /// ("Artist - Song (Official Video)" → "Song"), for videos tagged with
            /// `MUSICBRAINZ_ARTISTID` (the artist's name comes from MusicBrainz).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub clean_title: Option<CleanTitle>,
            /// Metadata embedded in the media file.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub embed: Option<EmbedOptions>,
            /// Cleanup policies.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub cleanup: Option<CleanupOptions>,
            /// YouTube Data API settings.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub youtube: Option<YoutubeOptions>,
        }

        impl $name {
            /// The shared options, for merging.
            #[must_use]
            pub fn options(&self) -> Options {
                Options {
                    media_dir: self.media_dir.clone(),
                    tmp_dir: self.tmp_dir.clone(),
                    trash_dir: self.trash_dir.clone(),
                    format: self.format.clone(),
                    merge_output_format: self.merge_output_format.clone(),
                    output_dir: self.output_dir.clone(),
                    output_file: self.output_file.clone(),
                    cover_file: self.cover_file.clone(),
                    subtitle_file: self.subtitle_file.clone(),
                    metadata_file: self.metadata_file.clone(),
                    ytdlp_args: self.ytdlp_args.clone(),
                    cookies_file: self.cookies_file.clone(),
                    match_fields: self.match_fields.clone(),
                    include: self.include.clone(),
                    exclude: self.exclude.clone(),
                    inherit_rules: self.inherit_rules.clone(),
                    min_duration: self.min_duration.clone(),
                    max_duration: self.max_duration.clone(),
                    cutoff: self.cutoff.clone(),
                    hooks: self.hooks.clone(),
                    inherit_hooks: self.inherit_hooks.clone(),
                    redownload_missing: self.redownload_missing.clone(),
                    keep_raw_info: self.keep_raw_info.clone(),
                    download: self.download,
                    clean_title: self.clean_title,
                    embed: self.embed.clone(),
                    cleanup: self.cleanup.clone(),
                    youtube: self.youtube.clone(),
                }
            }
        }
    };
}

options_struct! {
    /// The options categories and sources share, as one layer of settings.
    pub struct Options {}
}

options_struct! {
    /// `[category.<name>]`: settings shared by the sources that pick it.
    /// `[category.default]` applies to every source.
    pub struct CategoryConfig {}
}

options_struct! {
    /// One source to keep in sync.
    pub struct SourceConfig {
        /// Where videos come from: YouTube playlist URLs (with `list=`) and channel
        /// URLs (`/@handle`, `/channel/UC…`, `/c/name`, `/user/name`: all its uploads).
        /// Empty for a source whose videos only come from its backfill folder.
        #[schemars(
            extend("uniqueItems" = true, "items" = { "type": "string", "format": "source-url" })
        )]
        pub urls: Vec<String>,
        /// The category whose settings this source uses. Defaults to "default".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(regex(pattern = r"^[a-z0-9][a-z0-9_-]*$"))]
        pub category: Option<String>,
        /// Disabled sources are skipped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub enabled: Option<bool>,
    }
}

/// A field rules can match against. `kind` (`video`, `short` or `live`)
/// only works with `{ field = "kind", value = … }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleField {
    Title,
    Description,
    Channel,
    Tags,
    Id,
    Kind,
}

/// An include/exclude rule: a regex tested against `match_fields`, a
/// `{ field, pattern }` table targeting one field, or a `{ field, value }`
/// table matching one field exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Rule {
    /// Regex (fancy-regex syntax) tested against every field in `match_fields`.
    Pattern(#[schemars(extend("format" = "fancy-regex"))] String),
    /// Regex tested against one field.
    Field(FieldRule),
    /// One field equal to a value, e.g. `{ field = "kind", value = "short" }`.
    Value(ValueRule),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValueRule {
    /// The field to test.
    pub field: RuleField,
    /// The exact value (for `kind`: `video`, `short` or `live`).
    #[schemars(length(min = 1))]
    pub value: String,
}

/// What `kind` can be.
pub const KINDS: [&str; 3] = ["video", "short", "live"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    /// The field to test.
    pub field: RuleField,
    /// Regex (fancy-regex syntax).
    #[schemars(extend("format" = "fancy-regex"))]
    pub pattern: String,
}

/// `embed`: what goes into the media file. Everything is off by default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmbedOptions {
    /// Title, channel, date, description, URL, album/track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<bool>,
    /// The poster image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<bool>,
    /// Chapter markers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapters: Option<bool>,
    /// Languages from --sub-langs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<bool>,
    /// An `.nfo` file next to the video (named after it) with the same
    /// details as the tags, for Jellyfin and Kodi. On by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfo: Option<bool>,
    /// Tags to add or replace, e.g. `{ MUSICBRAINZ_ARTISTID = "…", album_artist = "{channel}" }`;
    /// an empty value removes the tag. Applied before `embed` hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "tags_schema")]
    pub tags: Option<BTreeMap<String, String>>,
    /// Look up MusicBrainz album and track ids for videos whose tags have
    /// `MUSICBRAINZ_ARTISTID`, e.g. `{ server = "http://mirror:5000" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub musicbrainz: Option<MusicBrainzOptions>,
}

/// `embed.musicbrainz`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MusicBrainzOptions {
    /// Turn it off for a source or category (on when the table is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The MusicBrainz server (a mirror works); default `https://musicbrainz.org`,
    /// asked at most once a second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^https?://"))]
    pub server: Option<String>,
}

/// `cleanup`: limits that keep a mirror bounded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanupOptions {
    /// Keep at most this many items; 0 = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,
    /// Remove items older than this; "" = no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<OptionalDuration>,
    /// Remove oldest items until media fits; "" = no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<OptionalSize>,
    /// Who goes first for the count and size limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<CleanupOrder>,
    /// Downloaded videos your include/exclude rules (or duration limits, cutoff) no
    /// longer match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<Unmatched>,
    /// What to keep of a removed item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<CleanupKeep>,
    /// Remove items whose video dir contains this file; "" = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_file: Option<String>,
    /// Protect the newest N items from every other policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_latest: Option<u32>,
    /// `trash`: removed files wait in `trash_dir` for `trash_for`, and can be restored;
    /// `now`: they're deleted right away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<CleanupDelete>,
    /// How long removed files stay in the trash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_for: Option<HumanDuration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanupDelete {
    /// Into the trash, deleted for good after `trash_for`.
    Trash,
    /// Deleted right away.
    Now,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOrder {
    OldestDownloaded,
    OldestPublished,
    SourcePosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unmatched {
    Keep,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanupKeep {
    /// Remove the whole video directory.
    Nothing,
    /// Keep metadata.json and the cover, drop media and subtitles.
    Metadata,
}

/// Format a duration with the largest unit that divides it exactly.
#[must_use]
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    for (unit, size) in [("w", 604_800), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs > 0 && secs.is_multiple_of(size) {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// A duration written as a string, e.g. `"30d"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(self.0))
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_duration(&s)
            .map(HumanDuration)
            .map_err(serde::de::Error::custom)
    }
}

/// A duration or `""` (unset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OptionalDuration(pub Option<Duration>);

impl Serialize for OptionalDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.map_or_default(format_duration))
    }
}

impl<'de> Deserialize<'de> for OptionalDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(OptionalDuration(None));
        }
        parse_duration(&s)
            .map(|d| OptionalDuration(Some(d)))
            .map_err(serde::de::Error::custom)
    }
}

/// A byte size or `""` (unset).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OptionalSize {
    pub text: String,
    pub bytes: Option<u64>,
}

impl Serialize for OptionalSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for OptionalSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        if text.is_empty() {
            return Ok(OptionalSize { text, bytes: None });
        }
        let bytes = parse_size(&text).map_err(serde::de::Error::custom)?;
        Ok(OptionalSize {
            text,
            bytes: Some(bytes),
        })
    }
}

/// `cutoff` as written: `"30d"`, `"2024-01-01"` or `"none"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoffValue {
    pub text: String,
    pub value: Cutoff,
}

impl Serialize for CutoffValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for CutoffValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let value = text.parse().map_err(serde::de::Error::custom)?;
        Ok(CutoffValue { text, value })
    }
}

macro_rules! string_schema {
    ($ty:ty, $name:literal, $format:literal, $example:literal) => {
        impl JsonSchema for $ty {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }

            fn inline_schema() -> bool {
                true
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "format": $format,
                    "examples": [$example],
                })
            }
        }
    };
}

string_schema!(HumanDuration, "Duration", "duration", "30d");
string_schema!(
    OptionalDuration,
    "OptionalDuration",
    "optional-duration",
    "30d"
);
string_schema!(OptionalSize, "OptionalSize", "optional-byte-size", "20GiB");
string_schema!(CutoffValue, "Cutoff", "cutoff", "30d");
