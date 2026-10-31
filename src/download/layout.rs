//! Where a video goes: placeholder values, the video's directory (`output`)
//! and the poster file name (`cover_name`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::merge::EffectiveSource;
use crate::config::placeholders::{self, PLACEHOLDERS, Setting};
use crate::paths::Dirs;

use super::info::VideoInfo;

/// Everything a video's placeholders can resolve to, unsanitized. Missing
/// values are absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values(pub BTreeMap<&'static str, String>);

impl Values {
    pub fn new(
        info: &VideoInfo,
        source: &EffectiveSource,
        source_title: Option<&str>,
        position: u32,
    ) -> Self {
        let mut map = BTreeMap::new();
        let mut put = |key: &'static str, value: Option<String>| {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                map.insert(key, value);
            }
        };
        put("id", Some(info.id.clone()));
        put("video_id", Some(info.id.clone()));
        put("title", Some(info.title.clone()));
        put("channel", info.channel.clone());
        put("channel_id", info.channel_id.clone());
        put("upload_date", info.upload_date_iso());
        put(
            "upload_year",
            info.upload_date_iso().map(|d| d[..4].to_string()),
        );
        put("duration", info.duration_seconds().map(|d| d.to_string()));
        put("position", Some(position.to_string()));
        put("source", Some(source.key.clone()));
        put("source_id", Some(source.id.clone()));
        put("source_title", source_title.map(str::to_string));
        put(
            "url",
            Some(format!("https://www.youtube.com/watch?v={}", info.id)),
        );
        put("category", Some(source.category.clone()));
        Self(map)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Values ready for file names: sanitized, `NA` when unknown.
    fn for_file_names(&self) -> BTreeMap<&'static str, String> {
        PLACEHOLDERS
            .iter()
            .map(|p| {
                let value = match (p.name, self.get(p.name)) {
                    (_, Some(value)) => match placeholders::sanitize(value) {
                        s if s.is_empty() => "NA".into(),
                        s => s,
                    },
                    (_, None) => "NA".into(),
                };
                (p.name, value)
            })
            .collect()
    }
}

fn render_file_name(template: &str, setting: Setting, values: &Values) -> Result<String, String> {
    let segments =
        placeholders::validate(template, setting).map_err(|e| format!("{template:?}: {e}"))?;
    let values = values.for_file_names();
    // Values are already sanitized.
    Ok(placeholders::render(&segments, Setting::Tags, |name| {
        values.get(name).cloned()
    }))
}

/// The video's directory (`output`), a full path.
pub fn video_dir(
    source: &EffectiveSource,
    values: &Values,
    media_dir: &Path,
    dirs: &Dirs,
) -> Result<PathBuf, String> {
    let values = values.for_file_names();
    placeholders::render_path(
        &source.output,
        Setting::Output,
        &dirs.home,
        |name| match name {
            "media_dir" => Some(media_dir.display().to_string()),
            "data_dir" => Some(dirs.data_dir.display().to_string()),
            _ => values.get(name).cloned(),
        },
    )
    .map_err(|e| format!("output {e}"))
}

/// The poster file name, e.g. `Some title-poster.jpg`.
pub fn cover_file_name(
    source: &EffectiveSource,
    values: &Values,
    extension: &str,
) -> Result<String, String> {
    let rendered = render_file_name(&source.cover_name, Setting::CoverName, values)?;
    let stem = rendered.trim().trim_matches('.');
    let stem = if stem.is_empty() { "poster" } else { stem };
    Ok(format!("{stem}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::merge::effective_sources;

    fn source(extra: &str) -> EffectiveSource {
        let config: Config = toml::from_str(&format!(
            "[sources.lofi]\nurl = 'https://www.youtube.com/playlist?list=PLabc'\n{extra}"
        ))
        .unwrap();
        effective_sources(&config).remove(0)
    }

    fn info(title: &str) -> VideoInfo {
        VideoInfo::parse(&format!(
            r#"{{"id":"dQw4w9WgXcQ","title":{},"channel":"Rick Astley","upload_date":"20091025","duration":213}}"#,
            serde_json::to_string(title).unwrap()
        ))
        .unwrap()
    }

    fn values(source: &EffectiveSource, title: &str) -> Values {
        Values::new(&info(title), source, Some("Rick's hits"), 7)
    }

    fn dirs() -> Dirs {
        Dirs {
            home: "/home/me".into(),
            config_dir: "/c".into(),
            data_dir: "/d/tidepool".into(),
        }
    }

    fn dir_of(s: &EffectiveSource, v: &Values) -> Result<PathBuf, String> {
        video_dir(s, v, Path::new("/media"), &dirs())
    }

    #[test]
    fn renders_the_default_layout() {
        let s = source("");
        let v = values(&s, "Never: Gonna/Give You Up?");
        assert_eq!(
            dir_of(&s, &v).unwrap(),
            PathBuf::from("/media/lofi/2009-10-25 - Never GonnaGive You Up [dQw4w9WgXcQ]")
        );
        assert_eq!(
            cover_file_name(&s, &v, "jpg").unwrap(),
            "Never GonnaGive You Up-poster.jpg"
        );
    }

    #[test]
    fn full_paths_padding_and_missing_values() {
        let s = source(
            "output = '~/Music/./{source}/{upload_year}/{position:03} {channel_id} [{id}]'\ncover_name = 'cover'",
        );
        let v = values(&s, "x");
        assert_eq!(
            dir_of(&s, &v).unwrap(),
            PathBuf::from("/home/me/Music/lofi/2009/007 NA [dQw4w9WgXcQ]")
        );
        assert_eq!(cover_file_name(&s, &v, "webp").unwrap(), "cover.webp");
        assert_eq!(v.get("source_title"), Some("Rick's hits"));
        assert_eq!(
            v.get("url"),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        let s = source("output = '{data_dir}/{category}/{id}'");
        assert_eq!(
            dir_of(&s, &values(&s, "x")).unwrap(),
            PathBuf::from("/d/tidepool/default/dQw4w9WgXcQ")
        );
    }

    #[test]
    fn rejects_parent_dirs() {
        let s = source("output = '{media_dir}/../{id}'");
        assert!(
            dir_of(&s, &values(&s, "x"))
                .unwrap_err()
                .contains("uses \"..\"")
        );
    }
}
