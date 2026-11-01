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
use super::types::*;

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
    pub const TRASH_FOR: std::time::Duration = std::time::Duration::from_secs(7 * 86_400);
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
    pub embed: Embed,
    pub cleanup: Cleanup,
    pub youtube: YoutubeConfig,
}

impl EffectiveSource {
    /// The lists `urls` names, in order.
    pub fn lists(&self) -> Vec<SourceList> {
        self.urls
            .iter()
            .filter_map(|u| SourceList::parse(u))
            .collect()
    }

    /// Each list's id (playlist id or channel), in order.
    pub fn ids(&self) -> Vec<String> {
        self.lists().into_iter().map(|l| l.id).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Embed {
    pub metadata: bool,
    pub cover: bool,
    pub chapters: bool,
    pub subtitles: bool,
    pub tags: BTreeMap<String, String>,
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
pub fn category_of(source: &SourceConfig) -> &str {
    source.category.as_deref().unwrap_or(DEFAULT_CATEGORY)
}

/// The option layers for a category, lowest priority first: `default`, then
/// the category itself (when it's another one).
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
pub fn effective_sources(config: &Config) -> Vec<EffectiveSource> {
    config
        .sources
        .iter()
        .map(|(key, source)| effective_source(config, key, source))
        .collect()
}

/// Resolve one source against its categories.
pub fn effective_source(config: &Config, key: &str, source: &SourceConfig) -> EffectiveSource {
    let category = category_of(source).to_string();
    let mut layers = category_layers(config, &category);
    layers.push(source.options());
    resolve(key, source, category, &layers)
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
    macro_rules! table {
        ($table:ident, $field:ident, $default:expr) => {
            layers
                .iter()
                .rev()
                .find_map(|l| l.$table.as_ref().and_then(|t| t.$field.clone()))
                .unwrap_or_else(|| $default)
        };
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
    let inherit_rules = |l: &Options| l.inherit_rules.unwrap_or(false);
    let inherit_hooks = |l: &Options| l.inherit_hooks.unwrap_or(false);
    let youtube_default = YoutubeConfig::default();

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
        embed: Embed {
            metadata: table!(embed, metadata, false),
            cover: table!(embed, cover, false),
            chapters: table!(embed, chapters, false),
            subtitles: table!(embed, subtitles, false),
            tags: merged_tags(layers),
        },
        cleanup: Cleanup {
            max_items: table!(cleanup, max_items, 0),
            max_age: table!(cleanup, max_age, OptionalDuration(None)),
            max_size: table!(cleanup, max_size, OptionalSize::default()),
            order: table!(cleanup, order, CleanupOrder::OldestDownloaded),
            unmatched: table!(cleanup, unmatched, Unmatched::Keep),
            keep: table!(cleanup, keep, CleanupKeep::Metadata),
            marker_file: table!(cleanup, marker_file, String::new()),
            keep_latest: table!(cleanup, keep_latest, 0),
            delete: table!(cleanup, delete, CleanupDelete::Trash),
            trash_for: table!(cleanup, trash_for, HumanDuration(builtin::TRASH_FOR)),
        },
        youtube: YoutubeConfig {
            api_key: layers
                .iter()
                .rev()
                .find_map(|l| l.youtube.as_ref().and_then(|y| y.api_key.clone())),
            daily_quota: table!(youtube, daily_quota, youtube_default.daily_quota),
            quota_warn_at: table!(youtube, quota_warn_at, youtube_default.quota_warn_at),
            fallback_to_ytdlp: table!(
                youtube,
                fallback_to_ytdlp,
                youtube_default.fallback_to_ytdlp
            ),
            requests_per_second: table!(
                youtube,
                requests_per_second,
                youtube_default.requests_per_second
            ),
        },
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
pub fn category_settings(config: &Config, category: &str) -> EffectiveSource {
    let source = SourceConfig {
        urls: Vec::new(),
        ..SourceConfig::default()
    };
    resolve(
        "",
        &source,
        category.to_string(),
        &category_layers(config, category),
    )
}

/// A category's own resolved youtube settings.
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
