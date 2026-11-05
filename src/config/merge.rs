//! Resolve each source's settings: built-in defaults, then
//! `[category.default]`, then the source's category, then the source itself.
//!
//! - Scalars: the last layer that sets a value wins. `embed`, `cleanup` and
//!   `youtube` merge key by key.
//! - Lists (`include`, `exclude`, `hooks`, `ytdlp_args`, `match_fields`): a
//!   layer's list replaces what came before, unless that layer sets
//!   `inherit_rules` (include/exclude) or `inherit_hooks` (hooks), which add
//!   to it instead.

use std::collections::BTreeMap;

use serde::Serialize;

use super::duration::Cutoff;
use super::types::{
    CleanupDelete, CleanupKeep, CleanupOrder, Config, CutoffValue, HumanDuration, OptionalDuration,
    OptionalSize, Options, Rule, RuleField, SourceConfig, Unmatched, YoutubeConfig,
};

/// The category every source builds on, and the one sources use by default.
pub const DEFAULT_CATEGORY: &str = "default";

/// Built-in option values, used when no layer sets one.
pub mod builtin {
    pub const FORMAT: &str = "bv*+ba/b/bv*/ba";
    pub const MERGE_OUTPUT_FORMAT: &str = "mkv";
    pub const OUTPUT_DIR: &str = "{media_dir}/{source}/{upload_date} - {title} [{id}]";
    pub const OUTPUT_FILE: &str = "{output_dir}/{normalized_title}.{ext}";
    pub const COVER_FILE: &str = "{output_dir}/{normalized_title}-poster.jpg";
    pub const SUBTITLE_FILE: &str = "{output_dir}/{normalized_title}.{lang}.{ext}";
    pub const METADATA_FILE: &str = "{output_dir}/metadata.json";
    pub const TMP_DIR: &str = "{data_dir}/tmp";
    pub const TRASH_DIR: &str = "{data_dir}/trash";
    /// How long removed files stay in the trash.
    pub const TRASH_FOR: std::time::Duration = std::time::Duration::from_hours(168);
}

/// One YouTube list a source reads: a playlist, or a channel's uploads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceList {
    pub url: String,
    /// The playlist id, or the channel as written (`@handle`, `UC…`,
    /// `c/name`, `user/name`).
    pub id: String,
    /// A channel: its uploads are listed.
    pub channel: bool,
}

impl SourceList {
    /// `None` for a URL that isn't a playlist or channel (config validation
    /// rejects those).
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let (id, channel) = match super::schema::parse_source_url(url).ok()? {
            super::schema::SourceUrl::Playlist(id) => (id, false),
            super::schema::SourceUrl::Channel(id) => (id, true),
        };
        Some(Self {
            url: url.to_string(),
            id,
            channel,
        })
    }
}

/// One source with every option resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the source's settings in the config file, and is printed as such"
)]
pub struct EffectiveSource {
    #[serde(skip)]
    pub key: String,
    pub urls: Vec<String>,
    pub category: String,
    pub enabled: bool,
    /// Unset means the default media folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_dir: Option<String>,
    pub tmp_dir: String,
    pub trash_dir: String,
    pub format: String,
    pub merge_output_format: String,
    pub output_dir: String,
    pub output_file: String,
    pub cover_file: String,
    pub subtitle_file: String,
    pub metadata_file: String,
    pub ytdlp_args: Vec<String>,
    pub cookies_file: String,
    pub match_fields: Vec<RuleField>,
    pub include: Vec<Rule>,
    pub exclude: Vec<Rule>,
    pub min_duration: OptionalDuration,
    pub max_duration: OptionalDuration,
    pub cutoff: CutoffValue,
    pub hooks: Vec<String>,
    pub redownload_missing: bool,
    pub keep_raw_info: bool,
    /// Download its videos (else only index it): the most specific
    /// `download` set, else `general.download`.
    pub download: bool,
    pub clean_title: crate::config::CleanTitle,
    pub embed: Embed,
    pub cleanup: Cleanup,
    pub youtube: YoutubeConfig,
}

impl EffectiveSource {
    /// The lists `urls` names, in order.
    #[must_use]
    pub fn lists(&self) -> Vec<SourceList> {
        self.urls
            .iter()
            .filter_map(|u| SourceList::parse(u))
            .collect()
    }

    /// Each list's id (playlist id or channel), in order.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lists().into_iter().map(|l| l.id).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the `embed` table in the config file, and is printed as such"
)]
pub struct Embed {
    pub metadata: bool,
    pub cover: bool,
    pub chapters: bool,
    pub subtitles: bool,
    pub nfo: bool,
    pub tags: BTreeMap<String, String>,
    /// The MusicBrainz server to look up album and track ids on, when on.
    #[serde(skip)]
    pub musicbrainz: Option<String>,
    /// The MusicBrainz server, even when ids aren't looked up (artist names
    /// for `clean_title` still are).
    #[serde(skip)]
    pub musicbrainz_server: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cleanup {
    pub max_items: u32,
    pub max_age: OptionalDuration,
    pub max_size: OptionalSize,
    pub order: CleanupOrder,
    pub unmatched: Unmatched,
    pub keep: CleanupKeep,
    pub marker_file: String,
    pub keep_latest: u32,
    pub delete: CleanupDelete,
    pub trash_for: HumanDuration,
}

/// The category a source uses.
#[must_use]
pub fn category_of(source: &SourceConfig) -> &str {
    source.category.as_deref().unwrap_or(DEFAULT_CATEGORY)
}

/// The option layers for a category, lowest priority first: `default`, then
/// the category itself (when it's another one).
#[must_use]
pub fn category_layers(config: &Config, category: &str) -> Vec<Options> {
    let mut layers = Vec::new();
    if let Some(default) = config.category.get(DEFAULT_CATEGORY) {
        layers.push(default.options());
    }
    if category != DEFAULT_CATEGORY
        && let Some(named) = config.category.get(category)
    {
        layers.push(named.options());
    }
    layers
}

/// Resolve every source in `config`, in name order.
#[must_use]
pub fn effective_sources(config: &Config) -> Vec<EffectiveSource> {
    config
        .sources
        .iter()
        .map(|(key, source)| effective_source(config, key, source))
        .collect()
}

/// Resolve one source against its categories.
#[must_use]
pub fn effective_source(config: &Config, key: &str, source: &SourceConfig) -> EffectiveSource {
    let category = category_of(source).to_string();
    let mut layers = category_layers(config, &category);
    layers.push(source.options());
    let mut effective = resolve(key, source, category, &layers);
    effective.download = downloads(config, &layers);
    effective
}

/// The most specific `download` set, else `general.download`.
fn downloads(config: &Config, layers: &[Options]) -> bool {
    layers
        .iter()
        .rev()
        .find_map(|l| l.download)
        .unwrap_or(config.general.download)
}

/// `$table.$field` from the most specific layer that sets it, else
/// `$default`.
macro_rules! table {
    ($layers:expr, $table:ident, $field:ident, $default:expr) => {
        $layers
            .iter()
            .rev()
            .find_map(|l| l.$table.as_ref().and_then(|t| t.$field.clone()))
            .unwrap_or_else(|| $default)
    };
}

fn resolve_embed(layers: &[Options]) -> Embed {
    Embed {
        metadata: table!(layers, embed, metadata, false),
        cover: table!(layers, embed, cover, false),
        chapters: table!(layers, embed, chapters, false),
        subtitles: table!(layers, embed, subtitles, false),
        nfo: table!(layers, embed, nfo, true),
        tags: merged_tags(layers),
        musicbrainz: musicbrainz(layers),
        musicbrainz_server: musicbrainz_server(layers),
    }
}

fn resolve_cleanup(layers: &[Options]) -> Cleanup {
    Cleanup {
        max_items: table!(layers, cleanup, max_items, 0),
        max_age: table!(layers, cleanup, max_age, OptionalDuration(None)),
        max_size: table!(layers, cleanup, max_size, OptionalSize::default()),
        order: table!(layers, cleanup, order, CleanupOrder::OldestDownloaded),
        unmatched: table!(layers, cleanup, unmatched, Unmatched::Keep),
        keep: table!(layers, cleanup, keep, CleanupKeep::Metadata),
        marker_file: table!(layers, cleanup, marker_file, String::new()),
        keep_latest: table!(layers, cleanup, keep_latest, 0),
        delete: table!(layers, cleanup, delete, CleanupDelete::Trash),
        trash_for: table!(
            layers,
            cleanup,
            trash_for,
            HumanDuration(builtin::TRASH_FOR)
        ),
    }
}

fn resolve_youtube(layers: &[Options]) -> YoutubeConfig {
    let defaults = YoutubeConfig::default();
    YoutubeConfig {
        api_key: layers
            .iter()
            .rev()
            .find_map(|l| l.youtube.as_ref().and_then(|y| y.api_key.as_deref()))
            .and_then(super::secret::resolve),
        daily_quota: table!(layers, youtube, daily_quota, defaults.daily_quota),
        quota_warn_at: table!(layers, youtube, quota_warn_at, defaults.quota_warn_at),
        fallback_to_ytdlp: table!(
            layers,
            youtube,
            fallback_to_ytdlp,
            defaults.fallback_to_ytdlp
        ),
        requests_per_second: table!(
            layers,
            youtube,
            requests_per_second,
            defaults.requests_per_second
        ),
    }
}

fn musicbrainz_server(layers: &[Options]) -> String {
    layers
        .iter()
        .rev()
        .filter_map(|l| l.embed.as_ref().and_then(|e| e.musicbrainz.as_ref()))
        .find_map(|t| t.server.clone())
        .unwrap_or_else(|| crate::musicbrainz::DEFAULT_SERVER.to_string())
}
/// On when any layer sets `embed.musicbrainz` and the last `enabled`
/// set isn't false; the last `server` set wins.
fn musicbrainz(layers: &[Options]) -> Option<String> {
    let tables: Vec<&crate::config::MusicBrainzOptions> = layers
        .iter()
        .filter_map(|l| l.embed.as_ref().and_then(|e| e.musicbrainz.as_ref()))
        .collect();
    if tables.is_empty() || tables.iter().rev().find_map(|t| t.enabled) == Some(false) {
        return None;
    }
    Some(
        tables
            .iter()
            .rev()
            .find_map(|t| t.server.clone())
            .unwrap_or_else(|| crate::musicbrainz::DEFAULT_SERVER.to_string()),
    )
}
fn list<T: Clone>(
    layers: &[Options],
    get: impl Fn(&Options) -> &Option<Vec<T>>,
    inherit: impl Fn(&Options) -> bool,
) -> Vec<T> {
    let mut out = Vec::new();
    for layer in layers {
        if let Some(items) = get(layer) {
            if !inherit(layer) {
                out.clear();
            }
            out.extend(items.iter().cloned());
        }
    }
    out
}

/// Apply `layers` (lowest priority first) over the built-in defaults.
fn resolve(
    key: &str,
    source: &SourceConfig,
    category: String,
    layers: &[Options],
) -> EffectiveSource {
    macro_rules! pick {
        ($field:ident, $default:expr) => {
            layers
                .iter()
                .rev()
                .find_map(|l| l.$field.clone())
                .unwrap_or_else(|| $default)
        };
    }
    let inherit_rules = |l: &Options| l.inherit_rules.unwrap_or(false);
    let inherit_hooks = |l: &Options| l.inherit_hooks.unwrap_or(false);

    EffectiveSource {
        key: key.to_string(),
        urls: source.urls.clone(),
        category,
        enabled: source.enabled.unwrap_or(true),
        media_dir: layers.iter().rev().find_map(|l| l.media_dir.clone()),
        tmp_dir: pick!(tmp_dir, builtin::TMP_DIR.into()),
        trash_dir: pick!(trash_dir, builtin::TRASH_DIR.into()),
        format: pick!(format, builtin::FORMAT.into()),
        merge_output_format: pick!(merge_output_format, builtin::MERGE_OUTPUT_FORMAT.into()),
        output_dir: pick!(output_dir, builtin::OUTPUT_DIR.into()),
        output_file: pick!(output_file, builtin::OUTPUT_FILE.into()),
        cover_file: pick!(cover_file, builtin::COVER_FILE.into()),
        subtitle_file: pick!(subtitle_file, builtin::SUBTITLE_FILE.into()),
        metadata_file: pick!(metadata_file, builtin::METADATA_FILE.into()),
        ytdlp_args: pick!(ytdlp_args, Vec::new()),
        cookies_file: pick!(cookies_file, String::new()),
        match_fields: pick!(match_fields, vec![RuleField::Title]),
        include: list(layers, |l| &l.include, inherit_rules),
        exclude: list(layers, |l| &l.exclude, inherit_rules),
        min_duration: pick!(min_duration, OptionalDuration(None)),
        max_duration: pick!(max_duration, OptionalDuration(None)),
        cutoff: pick!(
            cutoff,
            CutoffValue {
                text: "none".into(),
                value: Cutoff::None
            }
        ),
        hooks: list(layers, |l| &l.hooks, inherit_hooks),
        redownload_missing: pick!(redownload_missing, false),
        keep_raw_info: pick!(keep_raw_info, false),
        download: pick!(download, true),
        clean_title: pick!(clean_title, crate::config::CleanTitle::None),
        embed: resolve_embed(layers),
        cleanup: resolve_cleanup(layers),
        youtube: resolve_youtube(layers),
    }
}

/// `embed.tags` from every layer, name by name: a later layer adds tags or
/// replaces them (names match in any case); `""` stays, as "remove it".
fn merged_tags(layers: &[Options]) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    for layer in layers {
        let Some(layer_tags) = layer.embed.as_ref().and_then(|e| e.tags.as_ref()) else {
            continue;
        };
        for (key, value) in layer_tags {
            tags.retain(|k: &String, _| !k.eq_ignore_ascii_case(key));
            tags.insert(key.clone(), value.clone());
        }
    }
    tags
}

/// A category resolved on its own (`default` + the category, no source), for
/// checks about categories rather than sources.
#[must_use]
pub fn category_settings(config: &Config, category: &str) -> EffectiveSource {
    let source = SourceConfig {
        urls: Vec::new(),
        ..SourceConfig::default()
    };
    let layers = category_layers(config, category);
    let mut effective = resolve("", &source, category.to_string(), &layers);
    effective.download = downloads(config, &layers);
    effective
}

/// A category's own resolved youtube settings.
#[must_use]
pub fn category_youtube(config: &Config, category: &str) -> YoutubeConfig {
    category_settings(config, category).youtube
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const URL: &str = "urls = ['https://www.youtube.com/playlist?list=PLx']";

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    fn only(toml: &str) -> EffectiveSource {
        let mut all = effective_sources(&config(toml));
        assert_eq!(all.len(), 1);
        all.remove(0)
    }

    fn pat(s: &str) -> Rule {
        Rule::Pattern(s.into())
    }

    #[test]
    fn download_comes_from_the_most_specific_setting() {
        let config: Config = toml::from_str(&format!(
            "[general]\ndownload = false\n\
             [category.music]\ndownload = true\n\
             [sources.a]\n{URL}\n\
             [sources.b]\n{URL}\ncategory = 'music'\n\
             [sources.c]\n{URL}\ncategory = 'music'\ndownload = false\n\
             [sources.d]\n{URL}\ndownload = true\n"
        ))
        .unwrap();
        let downloads: Vec<(String, bool)> = effective_sources(&config)
            .into_iter()
            .map(|s| (s.key, s.download))
            .collect();
        assert_eq!(
            downloads,
            [
                ("a".into(), false),
                ("b".into(), true),
                ("c".into(), false),
                ("d".into(), true)
            ]
        );
        assert!(
            only(&format!("[sources.a]\n{URL}")).download,
            "on by default"
        );
    }

    #[test]
    fn falls_back_to_builtin_defaults() {
        let p = only(&format!("[sources.lofi]\n{URL}"));
        assert_eq!(p.ids(), ["PLx"]);
        assert_eq!(p.urls, ["https://www.youtube.com/playlist?list=PLx"]);
        assert_eq!(
            (p.category.as_str(), p.media_dir.as_deref()),
            ("default", None)
        );
        assert!(p.enabled);
        assert_eq!(p.tmp_dir, "{data_dir}/tmp");
        assert_eq!(
            p.format, "bv*+ba/b/bv*/ba",
            "best video + audio, then the best single file, then video or audio alone"
        );
        assert_eq!(p.output_dir, builtin::OUTPUT_DIR);
        assert_eq!(p.output_file, "{output_dir}/{normalized_title}.{ext}");
        assert_eq!(p.cover_file, "{output_dir}/{normalized_title}-poster.jpg");
        assert_eq!(p.subtitle_file, builtin::SUBTITLE_FILE);
        assert_eq!(p.metadata_file, "{output_dir}/metadata.json");
        assert_eq!(p.match_fields, vec![RuleField::Title]);
        assert_eq!(p.cutoff.value, Cutoff::None);
        assert!(!p.embed.metadata);
        assert_eq!(p.cleanup.keep, CleanupKeep::Metadata);
        assert_eq!(p.cleanup.order, CleanupOrder::OldestDownloaded);
        assert_eq!(p.youtube, YoutubeConfig::default());
    }

    #[test]
    fn extracts_id_from_url() {
        let p = only("[sources.a]\nurls = ['https://www.youtube.com/watch?v=x&list=PLabc']");
        assert_eq!(p.ids(), ["PLabc"]);
    }

    #[test]
    fn layers_stack_default_then_category_then_source() {
        let c = config(&format!(
            "[category.default]\nmedia_dir = '~/Videos'\nformat = 'a'\ncutoff = '30d'\ncover_file = '{{output_dir}}/cover.webp'\n\
             [category.music]\nmedia_dir = '/nas/music'\nformat = 'ba/b'\n\
             [sources.lofi]\n{URL}\ncategory = 'music'\ncutoff = 'none'\n\
             [sources.talks]\nurls = ['https://www.youtube.com/playlist?list=PLy']"
        ));
        let all = effective_sources(&c);
        let (lofi, talks) = (&all[0], &all[1]);
        assert_eq!(
            (
                lofi.category.as_str(),
                lofi.media_dir.as_deref(),
                lofi.format.as_str()
            ),
            ("music", Some("/nas/music"), "ba/b")
        );
        assert_eq!(lofi.cutoff.value, Cutoff::None, "the source wins");
        assert_eq!(
            lofi.cover_file, "{output_dir}/cover.webp",
            "from category.default"
        );
        assert_eq!(
            (
                talks.category.as_str(),
                talks.media_dir.as_deref(),
                talks.format.as_str()
            ),
            ("default", Some("~/Videos"), "a")
        );
    }

    #[test]
    fn lists_replace_unless_inherited_at_each_layer() {
        let base = "[category.default]\ninclude = ['d1']\nexclude = ['dx']\nhooks = ['h1']\n\
                    [category.music]\ninclude = ['c1']\ninherit_rules = true\n";
        let p = only(&format!("{base}[sources.x]\n{URL}\ncategory = 'music'"));
        assert_eq!(
            p.include,
            vec![pat("d1"), pat("c1")],
            "the category added to default"
        );
        assert_eq!(p.exclude, vec![pat("dx")], "unset lists fall back");
        assert_eq!(p.hooks, vec!["h1"]);

        let p = only(&format!(
            "{base}[sources.x]\n{URL}\ncategory = 'music'\ninclude = ['p1']\nhooks = ['h2']"
        ));
        assert_eq!(p.include, vec![pat("p1")], "the source replaced everything");
        assert_eq!(p.hooks, vec!["h2"]);

        let p = only(&format!(
            "{base}[sources.x]\n{URL}\ncategory = 'music'\ninclude = ['p1']\nhooks = ['h2']\ninherit_rules = true\ninherit_hooks = true"
        ));
        assert_eq!(p.include, vec![pat("d1"), pat("c1"), pat("p1")]);
        assert_eq!(p.hooks, vec!["h1", "h2"]);
    }

    #[test]
    fn tables_merge_key_by_key_across_layers() {
        let p = only(&format!(
            "[category.default.cleanup]\nmax_items = 10\nmax_age = '30d'\n[category.default.embed]\ncover = true\n\
             [category.default.youtube]\napi_key = 'AIza-default'\ndaily_quota = 5000\n\
             [category.music.youtube]\napi_key = 'AIza-music'\n\
             [sources.x]\n{URL}\ncategory = 'music'\n[sources.x.cleanup]\nmax_items = 50\n[sources.x.embed]\nmetadata = true"
        ));
        assert_eq!(p.cleanup.max_items, 50);
        assert_eq!(
            p.cleanup.max_age,
            OptionalDuration(Some(Duration::from_secs(30 * 86400)))
        );
        assert!(p.embed.cover && p.embed.metadata);
        assert_eq!(p.youtube.api_key.as_deref(), Some("AIza-music"));
        assert_eq!(
            p.youtube.daily_quota, 5000,
            "quota from default, key from the category"
        );
    }

    #[test]
    fn embed_tags_merge_name_by_name() {
        let p = only(&format!(
            "[category.default.embed]\ntags = {{ HANDLER_NAME = '', genre = 'Music' }}\n\
             [category.music.embed]\ntags = {{ album = '{{source_title}}' }}\n\
             [sources.x]\n{URL}\ncategory = 'music'\n\
             [sources.x.embed]\ntags = {{ MUSICBRAINZ_ARTISTID = 'abc', GENRE = 'Rock' }}"
        ));
        let tags: Vec<(&str, &str)> = p
            .embed
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            tags,
            [
                ("GENRE", "Rock"),
                ("HANDLER_NAME", ""),
                ("MUSICBRAINZ_ARTISTID", "abc"),
                ("album", "{source_title}")
            ],
            "every layer adds; later ones replace, in any case; \"\" stays as a removal"
        );
    }

    #[test]
    fn category_youtube_settings() {
        let c = config(
            "[category.default.youtube]\napi_key = 'k'\n[category.music.youtube]\nfallback_to_ytdlp = false",
        );
        let music = category_youtube(&c, "music");
        assert_eq!(
            (music.api_key.as_deref(), music.fallback_to_ytdlp),
            (Some("k"), false)
        );
        assert!(category_youtube(&c, "default").fallback_to_ytdlp);
    }
}
