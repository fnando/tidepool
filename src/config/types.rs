//! The config file's shape. These types generate the JSON Schema (via
//! `schemars`) and are deserialized only after the schema validated the input.
//!
//! `[defaults]` and `[sources.<key>]` share every option; sources override
//! defaults key by key (see [`super::merge`]).

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
    /// Where media goes and how downloads run.
    #[serde(default)]
    pub library: LibraryConfig,
    /// The yt-dlp binary: managed by tidepool, or your own.
    #[serde(default)]
    pub ytdlp: YtdlpConfig,
    /// YouTube Data API settings.
    #[serde(default)]
    pub youtube: YoutubeConfig,
    /// Scheduling for `tidepool run`.
    #[serde(default)]
    pub schedule: ScheduleConfig,
    /// The web UI served by `tidepool run`.
    #[serde(default)]
    pub server: ServerConfig,
    /// Options every source inherits.
    #[serde(default)]
    pub defaults: Defaults,
    /// Named post-download hooks, attached to sources by name.
    #[serde(default)]
    pub hooks: BTreeMap<String, HookConfig>,
    /// Sources to keep in sync, by name.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
}

/// `[library]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct LibraryConfig {
    /// Media root. Defaults to ~/.local/share/tidepool/media.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Parallel yt-dlp downloads.
    #[schemars(range(min = 1))]
    pub concurrency: u32,
    /// What to do when the same video is in several sources.
    pub duplicates: Duplicates,
    /// fancy-regex backtracking step limit per match.
    #[schemars(range(min = 1))]
    pub regex_backtrack_limit: u64,
    /// Grace period for child processes after SIGINT/SIGTERM.
    pub shutdown_timeout: HumanDuration,
    /// Work area, relative to `root` (same filesystem, so the final rename is atomic).
    pub tmp_dir: String,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            root: None,
            concurrency: 2,
            duplicates: Duplicates::Hardlink,
            regex_backtrack_limit: 1_000_000,
            shutdown_timeout: HumanDuration(Duration::from_secs(10)),
            tmp_dir: ".tidepool-tmp".into(),
        }
    }
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

/// `[ytdlp]`
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

/// `[youtube]`
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
    /// Serve the web UI from `tidepool run`; false = sync daemon only.
    pub enabled: bool,
    /// Address the web UI listens on, e.g. "127.0.0.1:8787".
    #[schemars(extend("format" = "socket-address"))]
    pub listen: String,
    /// Access token; required for a non-loopback `listen` address. "" = none.
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
    /// When the hook runs.
    #[serde(default)]
    pub event: HookEvent,
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
    pub cwd: Option<String>,
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
pub enum HookEvent {
    /// After a video is committed to the library.
    #[default]
    Downloaded,
    /// Before files are deleted; a non-zero exit vetoes the cleanup.
    BeforeCleanup,
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

/// Defines a struct holding every per-source option, all optional, plus any
/// extra fields. Used for both `[defaults]` and `[sources.<key>]` so the two
/// can't drift apart (serde's `flatten` doesn't work with `deny_unknown_fields`).
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
            /// yt-dlp format selector (`-f`).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub format: Option<String>,
            /// Container for merged formats (`--merge-output-format`).
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub merge_output_format: Option<String>,
            /// Each video's directory, relative to `root`. Must contain {id}.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "output-template"))]
            pub output: Option<String>,
            /// Extra yt-dlp arguments, passed through.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub ytdlp_args: Option<Vec<String>>,
            /// Netscape cookies file, for private sources. "" = none.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub cookies_file: Option<String>,
            /// Cover image format.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub thumbnail_format: Option<ThumbnailFormat>,
            /// Poster file name without extension; same placeholders as `output`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            #[schemars(extend("format" = "file-name-template"))]
            pub cover_name: Option<String>,
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
            /// Download again when a downloaded item's media file is gone.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub redownload_missing: Option<bool>,
            /// Keep yt-dlp's raw info.json next to metadata.json.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub keep_raw_info: Option<bool>,
            /// Metadata embedded in the media file.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub embed: Option<EmbedOptions>,
            /// Cleanup policies.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub cleanup: Option<CleanupOptions>,
        }
    };
}

options_struct! {
    /// `[defaults]`: options every source inherits.
    pub struct Defaults {}
}

options_struct! {
    /// One source to keep in sync.
    pub struct SourceConfig {
        /// Where videos come from: a YouTube playlist URL (with `list=`).
        #[schemars(extend("format" = "source-url"))]
        pub url: String,
        /// Directory under `root`. Defaults to the source name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1))]
        pub dir: Option<String>,
        /// Disabled sources are skipped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub enabled: Option<bool>,
        /// Concatenate `[defaults]` include/exclude instead of replacing them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub inherit_rules: Option<bool>,
        /// Concatenate `[defaults]` hooks instead of replacing them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub inherit_hooks: Option<bool>,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThumbnailFormat {
    Jpg,
    Webp,
    Original,
}

/// A field rules can match against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleField {
    Title,
    Description,
    Channel,
    Tags,
    Id,
}

/// An include/exclude rule: a regex tested against `match_fields`, or a
/// `{ field, pattern }` table targeting one field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Rule {
    /// Regex (fancy-regex syntax) tested against every field in `match_fields`.
    Pattern(#[schemars(extend("format" = "fancy-regex"))] String),
    /// Regex tested against one field.
    Field(FieldRule),
}

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
    /// Attach metadata.json (mkv/mka only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_json: Option<bool>,
    /// Re-embed if a hook replaced the media file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_hooks: Option<bool>,
    /// Extra tags, literal or templated, e.g. `album_artist = "{channel}"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "tags_schema")]
    pub tags: Option<BTreeMap<String, String>>,
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
    /// Who goes first for count, size and age limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<CleanupOrder>,
    /// Videos no longer in the source (or no longer passing the rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_from_source: Option<RemovedFromSource>,
    /// What to keep of a removed item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<CleanupKeep>,
    /// Remove items whose video dir contains this file; "" = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_file: Option<String>,
    /// Protect the newest N items from every other policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_latest: Option<u32>,
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
pub enum RemovedFromSource {
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
        s.serialize_str(&self.0.map(format_duration).unwrap_or_default())
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
