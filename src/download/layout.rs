//! Where a video goes: placeholder values and the full paths of its
//! directory (`output_dir`) and files (`output_file`, `cover_file`,
//! `subtitle_file`, `metadata_file`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::merge::EffectiveSource;
use crate::config::placeholders::{self, PLACEHOLDERS, Setting};
use crate::paths::Dirs;

use super::info::VideoInfo;
use super::thumbnail::CoverFormat;

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
        put(
            "normalized_title",
            Some(placeholders::normalize_title(&info.title)),
        );
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

/// Every destination of one video.
#[derive(Debug, Clone)]
pub struct Layout {
    values: BTreeMap<&'static str, String>,
    dirs: Dirs,
    media_dir: PathBuf,
    output_dir: PathBuf,
    output_file: String,
    cover_file: String,
    subtitle_file: String,
    metadata_file: String,
    /// Full paths `prepare` hooks set, by setting name (`{ext}`/`{lang}`
    /// still in them); they win over the templates.
    overrides: BTreeMap<String, String>,
}

impl Layout {
    pub fn new(
        source: &EffectiveSource,
        values: &Values,
        media_dir: &Path,
        dirs: &Dirs,
    ) -> Result<Self, String> {
        let mut layout = Self {
            values: values.for_file_names(),
            dirs: dirs.clone(),
            media_dir: media_dir.to_path_buf(),
            output_dir: PathBuf::new(),
            output_file: source.output_file.clone(),
            cover_file: source.cover_file.clone(),
            subtitle_file: source.subtitle_file.clone(),
            metadata_file: source.metadata_file.clone(),
            overrides: BTreeMap::new(),
        };
        layout.output_dir = layout.render(&source.output_dir, Setting::OutputDir, &[])?;
        Ok(layout)
    }

    fn render(
        &self,
        template: &str,
        setting: Setting,
        extra: &[(&str, &str)],
    ) -> Result<PathBuf, String> {
        placeholders::render_path(template, setting, &self.dirs.home, |name| match name {
            "media_dir" => Some(self.media_dir.display().to_string()),
            "data_dir" => Some(self.dirs.data_dir.display().to_string()),
            "output_dir" => Some(self.output_dir.display().to_string()),
            _ => extra
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| match placeholders::sanitize(v) {
                    s if s.is_empty() => "NA".into(),
                    s => s,
                })
                .or_else(|| self.values.get(name).cloned()),
        })
        .map_err(|e| format!("{setting} {e}"))
    }

    /// Use the paths `prepare` hooks set. A new `output_dir` also moves the
    /// files whose templates use `{output_dir}`.
    pub fn with_overrides(mut self, overrides: &BTreeMap<String, String>) -> Self {
        if let Some(dir) = overrides.get("output_dir") {
            self.output_dir = PathBuf::from(dir);
        }
        self.overrides = overrides.clone();
        self
    }

    /// Every path as it would be, with `{ext}` and `{lang}` left in: what
    /// `prepare` hooks are shown.
    pub fn proposed(&self) -> Result<BTreeMap<String, String>, String> {
        let show = |p: PathBuf| p.display().to_string();
        Ok([
            ("output_dir", show(self.output_dir.clone())),
            ("output_file", show(self.output_file("{ext}")?)),
            ("cover_file", show(self.cover_file()?.0)),
            (
                "subtitle_file",
                show(self.subtitle_file("{lang}", "{ext}")?),
            ),
            ("metadata_file", show(self.metadata_file()?)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect())
    }

    /// A path a `prepare` hook set, with `extra` placeholders filled in.
    fn overridden(&self, key: &str, extra: &[(&str, &str)]) -> Option<PathBuf> {
        let mut path = self.overrides.get(key)?.clone();
        for (name, value) in extra {
            let value = match placeholders::sanitize(value) {
                s if s.is_empty() => "NA".into(),
                s => s,
            };
            path = path.replace(&format!("{{{name}}}"), &value);
        }
        Some(PathBuf::from(path))
    }

    /// The video's directory (`output_dir`).
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }

    /// The media file (`output_file`) for yt-dlp's extension.
    pub fn output_file(&self, ext: &str) -> Result<PathBuf, String> {
        if let Some(path) = self.overridden("output_file", &[("ext", ext)]) {
            return Ok(path);
        }
        self.render(&self.output_file, Setting::OutputFile, &[("ext", ext)])
    }

    /// The poster (`cover_file`) and the image format its extension asks for.
    pub fn cover_file(&self) -> Result<(PathBuf, CoverFormat), String> {
        let path = match self.overridden("cover_file", &[]) {
            Some(path) => path,
            None => self.render(&self.cover_file, Setting::CoverFile, &[])?,
        };
        let format = path
            .extension()
            .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))
            .ok_or_else(|| {
                format!(
                    "cover_file {:?} must end with .jpg, .png or .webp",
                    self.cover_file
                )
            })?;
        Ok((path, format))
    }

    /// One subtitle (`subtitle_file`).
    pub fn subtitle_file(&self, lang: &str, ext: &str) -> Result<PathBuf, String> {
        if let Some(path) = self.overridden("subtitle_file", &[("lang", lang), ("ext", ext)]) {
            return Ok(path);
        }
        self.render(
            &self.subtitle_file,
            Setting::SubtitleFile,
            &[("lang", lang), ("ext", ext)],
        )
    }

    /// tidepool's metadata.json (`metadata_file`).
    pub fn metadata_file(&self) -> Result<PathBuf, String> {
        if let Some(path) = self.overridden("metadata_file", &[]) {
            return Ok(path);
        }
        self.render(&self.metadata_file, Setting::MetadataFile, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::merge::effective_sources;

    fn source(extra: &str) -> EffectiveSource {
        let config: Config = toml::from_str(&format!(
            "[sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLabc']\n{extra}"
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

    fn layout(s: &EffectiveSource, v: &Values) -> Result<Layout, String> {
        Layout::new(s, v, Path::new("/media"), &dirs())
    }

    #[test]
    fn renders_the_default_layout() {
        let s = source("");
        let v = values(&s, "Never: Gonna/Give You Up? 東京");
        let l = layout(&s, &v).unwrap();
        let dir = "/media/lofi/2009-10-25 - Never GonnaGive You Up 東京 [dQw4w9WgXcQ]";
        assert_eq!(l.output_dir(), Path::new(dir));
        assert_eq!(
            l.output_file("mkv").unwrap(),
            PathBuf::from(format!("{dir}/Never GonnaGive You Up Dong Jing.mkv"))
        );
        assert_eq!(
            l.cover_file().unwrap(),
            (
                PathBuf::from(format!("{dir}/Never GonnaGive You Up Dong Jing-poster.jpg")),
                CoverFormat::Jpg
            )
        );
        assert_eq!(
            l.subtitle_file("en-US", "vtt").unwrap(),
            PathBuf::from(format!("{dir}/Never GonnaGive You Up Dong Jing.en-US.vtt"))
        );
        assert_eq!(
            l.metadata_file().unwrap(),
            PathBuf::from(format!("{dir}/metadata.json"))
        );
    }

    #[test]
    fn full_paths_anywhere_padding_and_missing_values() {
        let s = source(
            "output_dir = '~/Music/./{source}/{upload_year}'\n\
             output_file = '{output_dir}/{position:03} {channel_id} [{id}].{ext}'\n\
             cover_file = '/covers/{id}.WEBP'\n\
             metadata_file = '{data_dir}/meta/{id}.json'",
        );
        let v = values(&s, "x");
        let l = layout(&s, &v).unwrap();
        assert_eq!(l.output_dir(), Path::new("/home/me/Music/lofi/2009"));
        assert_eq!(
            l.output_file("opus").unwrap(),
            PathBuf::from("/home/me/Music/lofi/2009/007 NA [dQw4w9WgXcQ].opus")
        );
        assert_eq!(
            l.cover_file().unwrap(),
            (PathBuf::from("/covers/dQw4w9WgXcQ.WEBP"), CoverFormat::Webp)
        );
        assert_eq!(
            l.metadata_file().unwrap(),
            PathBuf::from("/d/tidepool/meta/dQw4w9WgXcQ.json")
        );
        assert_eq!(v.get("source_title"), Some("Rick's hits"));
        assert_eq!(
            v.get("url"),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
    }

    #[test]
    fn proposes_paths_and_takes_overrides() {
        let s = source("");
        let v = values(&s, "x");
        let l = layout(&s, &v).unwrap();
        let proposed = l.proposed().unwrap();
        let dir = "/media/lofi/2009-10-25 - x [dQw4w9WgXcQ]";
        assert_eq!(proposed["output_file"], format!("{dir}/x.{{ext}}"));
        assert_eq!(
            proposed["subtitle_file"],
            format!("{dir}/x.{{lang}}.{{ext}}")
        );

        let overrides: BTreeMap<String, String> = [
            ("output_dir", "/media/Rick Astley"),
            ("subtitle_file", "/media/subs/{lang}.{ext}"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let l = l.with_overrides(&overrides);
        assert_eq!(
            l.output_file("mkv").unwrap(),
            PathBuf::from("/media/Rick Astley/x.mkv"),
            "follows the new output_dir"
        );
        assert_eq!(
            l.subtitle_file("en/US", "vtt").unwrap(),
            PathBuf::from("/media/subs/enUS.vtt")
        );
    }

    #[test]
    fn rejects_parent_dirs() {
        let s = source("output_dir = '{media_dir}/../{id}'");
        assert!(
            layout(&s, &values(&s, "x"))
                .unwrap_err()
                .contains("uses \"..\"")
        );
    }
}
