//! Merge `[defaults]` into each playlist: the effective, fully-resolved options.
//!
//! - Scalars and tables: playlist value wins, then `[defaults]`, then built-in
//!   defaults. `embed` and `cleanup` merge key by key.
//! - Lists (`include`, `exclude`, `hooks`, `ytdlp_args`, `match_fields`): the
//!   playlist replaces defaults, unless `inherit_rules` (include/exclude) or
//!   `inherit_hooks` (hooks) concatenates them.

use std::collections::BTreeMap;

use serde::Serialize;

use super::duration::Cutoff;
use super::types::*;
use crate::cli::playlist_id_from_url;

/// Built-in option values, used when neither the playlist nor `[defaults]` sets one.
pub mod builtin {
    pub const FORMAT: &str = "bv*+ba/b";
    pub const MERGE_OUTPUT_FORMAT: &str = "mkv";
    pub const OUTPUT: &str = "{playlist_dir}/{upload_date} - {title} [{id}]";
    pub const COVER_NAME: &str = "{title}-poster";
}

/// One playlist with every option resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectivePlaylist {
    #[serde(skip)]
    pub key: String,
    /// YouTube playlist id (from `id`, or extracted from `url`).
    pub id: String,
    pub url: String,
    pub dir: String,
    pub enabled: bool,
    pub format: String,
    pub merge_output_format: String,
    pub output: String,
    pub ytdlp_args: Vec<String>,
    pub cookies_file: String,
    pub thumbnail_format: ThumbnailFormat,
    pub cover_name: String,
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
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Embed {
    pub metadata: bool,
    pub cover: bool,
    pub chapters: bool,
    pub subtitles: bool,
    pub info_json: bool,
    pub after_hooks: bool,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cleanup {
    pub max_items: u32,
    pub max_age: OptionalDuration,
    pub max_size: OptionalSize,
    pub order: CleanupOrder,
    pub removed_from_playlist: RemovedFromPlaylist,
    pub keep: CleanupKeep,
    pub marker_file: String,
    pub keep_latest: u32,
}

/// Resolve every playlist in `config`, in key order.
pub fn effective_playlists(config: &Config) -> Vec<EffectivePlaylist> {
    config
        .playlists
        .iter()
        .map(|(key, playlist)| effective_playlist(key, playlist, &config.defaults))
        .collect()
}

/// Resolve one playlist against `[defaults]`.
pub fn effective_playlist(key: &str, p: &PlaylistConfig, d: &Defaults) -> EffectivePlaylist {
    macro_rules! pick {
        ($field:ident, $default:expr) => {
            p.$field
                .clone()
                .or_else(|| d.$field.clone())
                .unwrap_or_else(|| $default)
        };
    }
    fn concat<T: Clone>(inherit: bool, own: &Option<Vec<T>>, base: &Option<Vec<T>>) -> Vec<T> {
        match (own, inherit) {
            (Some(own), true) => base.iter().flatten().chain(own).cloned().collect(),
            (Some(own), false) => own.clone(),
            (None, _) => base.clone().unwrap_or_default(),
        }
    }

    let inherit_rules = p.inherit_rules.unwrap_or(false);
    let inherit_hooks = p.inherit_hooks.unwrap_or(false);
    let (id, url) = match (&p.id, &p.url) {
        (Some(id), _) => (
            id.clone(),
            format!("https://www.youtube.com/playlist?list={id}"),
        ),
        (None, Some(url)) => (playlist_id_from_url(url).unwrap_or_default(), url.clone()),
        (None, None) => (String::new(), String::new()),
    };

    EffectivePlaylist {
        key: key.to_string(),
        id,
        url,
        dir: p.dir.clone().unwrap_or_else(|| key.to_string()),
        enabled: p.enabled.unwrap_or(true),
        format: pick!(format, builtin::FORMAT.into()),
        merge_output_format: pick!(merge_output_format, builtin::MERGE_OUTPUT_FORMAT.into()),
        output: pick!(output, builtin::OUTPUT.into()),
        ytdlp_args: pick!(ytdlp_args, Vec::new()),
        cookies_file: pick!(cookies_file, String::new()),
        thumbnail_format: pick!(thumbnail_format, ThumbnailFormat::Jpg),
        cover_name: pick!(cover_name, builtin::COVER_NAME.into()),
        match_fields: pick!(match_fields, vec![RuleField::Title]),
        include: concat(inherit_rules, &p.include, &d.include),
        exclude: concat(inherit_rules, &p.exclude, &d.exclude),
        min_duration: pick!(min_duration, OptionalDuration(None)),
        max_duration: pick!(max_duration, OptionalDuration(None)),
        cutoff: pick!(
            cutoff,
            CutoffValue {
                text: "none".into(),
                value: Cutoff::None
            }
        ),
        hooks: concat(inherit_hooks, &p.hooks, &d.hooks),
        redownload_missing: pick!(redownload_missing, false),
        keep_raw_info: pick!(keep_raw_info, false),
        embed: merge_embed(p.embed.as_ref(), d.embed.as_ref()),
        cleanup: merge_cleanup(p.cleanup.as_ref(), d.cleanup.as_ref()),
    }
}

fn merge_embed(p: Option<&EmbedOptions>, d: Option<&EmbedOptions>) -> Embed {
    macro_rules! pick {
        ($field:ident, $default:expr) => {
            p.and_then(|e| e.$field.clone())
                .or_else(|| d.and_then(|e| e.$field.clone()))
                .unwrap_or($default)
        };
    }
    Embed {
        metadata: pick!(metadata, false),
        cover: pick!(cover, false),
        chapters: pick!(chapters, false),
        subtitles: pick!(subtitles, false),
        info_json: pick!(info_json, false),
        after_hooks: pick!(after_hooks, true),
        tags: pick!(tags, BTreeMap::new()),
    }
}

fn merge_cleanup(p: Option<&CleanupOptions>, d: Option<&CleanupOptions>) -> Cleanup {
    macro_rules! pick {
        ($field:ident, $default:expr) => {
            p.and_then(|c| c.$field.clone())
                .or_else(|| d.and_then(|c| c.$field.clone()))
                .unwrap_or($default)
        };
    }
    Cleanup {
        max_items: pick!(max_items, 0),
        max_age: pick!(max_age, OptionalDuration(None)),
        max_size: pick!(max_size, OptionalSize::default()),
        order: pick!(order, CleanupOrder::OldestDownloaded),
        removed_from_playlist: pick!(removed_from_playlist, RemovedFromPlaylist::Keep),
        keep: pick!(keep, CleanupKeep::Metadata),
        marker_file: pick!(marker_file, String::new()),
        keep_latest: pick!(keep_latest, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    fn only(toml: &str) -> EffectivePlaylist {
        let mut all = effective_playlists(&config(toml));
        assert_eq!(all.len(), 1);
        all.remove(0)
    }

    fn pat(s: &str) -> Rule {
        Rule::Pattern(s.into())
    }

    #[test]
    fn falls_back_to_builtin_defaults() {
        let p = only("[playlists.lofi]\nid = 'PLx'");
        assert_eq!(p.id, "PLx");
        assert_eq!(p.url, "https://www.youtube.com/playlist?list=PLx");
        assert_eq!(p.dir, "lofi");
        assert!(p.enabled);
        assert_eq!(p.format, builtin::FORMAT);
        assert_eq!(p.output, builtin::OUTPUT);
        assert_eq!(p.cover_name, builtin::COVER_NAME);
        assert_eq!(p.match_fields, vec![RuleField::Title]);
        assert_eq!(p.cutoff.value, Cutoff::None);
        assert!(!p.embed.metadata && p.embed.after_hooks);
        assert_eq!(p.cleanup.keep, CleanupKeep::Metadata);
        assert_eq!(p.cleanup.order, CleanupOrder::OldestDownloaded);
    }

    #[test]
    fn extracts_id_from_url() {
        let p = only("[playlists.a]\nurl = 'https://www.youtube.com/watch?v=x&list=PLabc'");
        assert_eq!(p.id, "PLabc");
    }

    #[test]
    fn playlist_scalars_override_defaults() {
        let p = only(
            "[defaults]\nformat = 'a'\ncutoff = '30d'\nthumbnail_format = 'webp'\n\
             [playlists.x]\nid = 'PLx'\nformat = 'b'\ncutoff = 'none'",
        );
        assert_eq!(p.format, "b");
        assert_eq!(p.cutoff.value, Cutoff::None);
        assert_eq!(p.thumbnail_format, ThumbnailFormat::Webp);
    }

    #[test]
    fn lists_replace_unless_inherited() {
        let base = "[defaults]\ninclude = ['d1']\nexclude = ['dx']\nhooks = ['h1']\n";
        let p = only(&format!(
            "{base}[playlists.x]\nid = 'PLx'\ninclude = ['p1']\nhooks = ['h2']"
        ));
        assert_eq!(p.include, vec![pat("p1")]);
        assert_eq!(
            p.exclude,
            vec![pat("dx")],
            "unset list falls back to defaults"
        );
        assert_eq!(p.hooks, vec!["h2"]);

        let p = only(&format!(
            "{base}[playlists.x]\nid = 'PLx'\ninclude = ['p1']\nexclude = ['px']\nhooks = ['h2']\n\
             inherit_rules = true\ninherit_hooks = true"
        ));
        assert_eq!(p.include, vec![pat("d1"), pat("p1")]);
        assert_eq!(p.exclude, vec![pat("dx"), pat("px")]);
        assert_eq!(p.hooks, vec!["h1", "h2"]);
    }

    #[test]
    fn cleanup_and_embed_merge_key_by_key() {
        let p = only(
            "[defaults.cleanup]\nmax_items = 10\nmax_age = '30d'\n[defaults.embed]\ncover = true\n\
             [playlists.x]\nid = 'PLx'\n[playlists.x.cleanup]\nmax_items = 50\n\
             [playlists.x.embed]\nmetadata = true",
        );
        assert_eq!(p.cleanup.max_items, 50);
        assert_eq!(
            p.cleanup.max_age,
            OptionalDuration(Some(Duration::from_secs(30 * 86400)))
        );
        assert!(p.embed.cover && p.embed.metadata);
    }
}
