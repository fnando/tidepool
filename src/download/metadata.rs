//! The normalized, versioned `metadata.json` written next to each video.

use jiff::Timestamp;
use serde_json::{Value, json};

use super::info::VideoInfo;
use super::thumbnail::Cover;
use crate::config::merge::EffectiveSource;
use crate::state::items::VideoFiles;

pub struct Metadata<'a> {
    pub info: &'a VideoInfo,
    pub source: &'a EffectiveSource,
    pub source_title: Option<&'a str>,
    pub position: u32,
    pub files: &'a VideoFiles,
    pub media_bytes: u64,
    pub cover: Option<&'a Cover>,
    pub embedded: &'a super::embed::Embedded,
    /// The tags written into the media file, after `embed` hooks.
    pub embedded_tags: Option<&'a std::collections::BTreeMap<String, String>>,
    pub embed_error: Option<&'a str>,
    pub downloaded_at: Timestamp,
    pub ytdlp_version: Option<&'a str>,
}

/// Update the `files` section of a placed metadata.json (after a hook moved
/// or removed files). A missing metadata file is left alone.
pub fn rewrite_files(files: &VideoFiles) -> std::io::Result<()> {
    let path = std::path::Path::new(&files.metadata);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let mut meta: Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    meta["files"] = files_json(files);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tidepool-partial"));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&meta).expect("JSON") + "\n",
    )?;
    std::fs::rename(&tmp, path)
}

/// Where a video's files are, for metadata.json: relative to the metadata
/// file's folder when they're under it, absolute otherwise.
pub fn files_json(files: &VideoFiles) -> Value {
    Metadata::files_json_of(files)
}

impl Metadata<'_> {
    fn files_json(&self) -> Value {
        Self::files_json_of(self.files)
    }

    fn files_json_of(files: &VideoFiles) -> Value {
        let base = std::path::Path::new(&files.metadata)
            .parent()
            .unwrap_or(std::path::Path::new("/"));
        let show = |path: &str| {
            std::path::Path::new(path)
                .strip_prefix(base)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.to_string())
        };
        let subtitles: Vec<Value> = files
            .subtitles
            .iter()
            .filter(|s| !files.is_removed(&format!("subtitle:{}", s.lang)))
            .map(|s| json!({ "lang": s.lang, "path": show(&s.path) }))
            .collect();
        json!({
            "media": (!files.is_removed("media")).then(|| show(&files.media)),
            "cover": files.cover.as_deref().filter(|_| !files.is_removed("cover")).map(show),
            "subtitles": subtitles,
        })
    }

    pub fn to_json(&self) -> Value {
        let info = self.info;
        let chapters: Vec<Value> = info
            .chapters
            .iter()
            .flatten()
            .map(|c| json!({ "start": c.start_time, "end": c.end_time, "title": c.title }))
            .collect();
        json!({
            "schema_version": 1,
            "id": info.id,
            "url": format!("https://www.youtube.com/watch?v={}", info.id),
            "title": info.title,
            "description": info.description,
            "channel": {
                "id": info.channel_id,
                "name": info.channel,
                "url": info.channel_url,
            },
            "upload_date": info.upload_date_iso(),
            "published_at": info.published_at().map(|t| t.to_string()),
            "duration_seconds": info.duration_seconds(),
            "tags": info.tags.clone().unwrap_or_default(),
            "categories": info.categories.clone().unwrap_or_default(),
            "chapters": chapters,
            "source": {
                "name": self.source.key,
                "url": self.source.url,
                "id": self.source.id,
                "title": self.source_title,
                "position": self.position,
            },
            "files": self.files_json(),
            "media": {
                "format_id": info.format_id,
                "width": info.width,
                "height": info.height,
                "fps": info.fps,
                "vcodec": VideoInfo::codec(&info.vcodec),
                "acodec": VideoInfo::codec(&info.acodec),
                "size_bytes": self.media_bytes,
            },
            "cover": self.cover.map(|c| json!({ "source_url": c.source_url, "width": c.width, "height": c.height })),
            "embedded": self.embedded,
            "embedded_tags": self.embedded_tags,
            "embed_error": self.embed_error,
            "downloaded_at": self.downloaded_at.to_string(),
            "tool": { "tidepool": env!("CARGO_PKG_VERSION"), "yt_dlp": self.ytdlp_version },
            "index_source": "ytdlp",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::merge::effective_sources;

    #[test]
    fn writes_the_documented_shape() {
        let config: Config =
            toml::from_str("[sources.lofi]\nurl = 'https://www.youtube.com/playlist?list=PLabc'")
                .unwrap();
        let source = effective_sources(&config).remove(0);
        let info = VideoInfo::parse(
            &std::fs::read_to_string(format!(
                "{}/tests/fixtures/ytdlp/videos/dQw4w9WgXcQ.info.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let files = VideoFiles {
            dir: "/m/lofi/x".into(),
            media: "/m/lofi/x/x.mkv".into(),
            cover: Some("/covers/x-poster.jpg".into()),
            subtitles: vec![crate::state::items::Subtitle {
                lang: "en".into(),
                path: "/m/lofi/x/subs/x.en.vtt".into(),
            }],
            metadata: "/m/lofi/x/metadata.json".into(),
            raw_info: None,
            removed: vec![],
        };
        let cover = Cover {
            bytes: vec![],
            extension: "jpg",
            width: 1920,
            height: 1080,
            source_url: "https://i/x.webp".into(),
        };
        let json = Metadata {
            info: &info,
            source: &source,
            source_title: Some("Mix"),
            position: 3,
            files: &files,
            media_bytes: 1234,
            cover: Some(&cover),
            embedded: &crate::download::embed::Embedded {
                metadata: true,
                ..Default::default()
            },
            embedded_tags: None,
            embed_error: None,
            downloaded_at: "2026-10-31T12:00:00Z".parse().unwrap(),
            ytdlp_version: Some("2026.10.29"),
        }
        .to_json();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["url"], "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(json["channel"]["name"], "Rick Astley");
        assert_eq!(json["published_at"], "2009-10-25T06:57:33Z");
        assert_eq!(json["duration_seconds"], 213);
        assert_eq!(
            json["chapters"][0],
            json!({ "start": 0.0, "end": 30.5, "title": "Intro" })
        );
        assert_eq!(
            json["source"],
            json!({ "name": "lofi", "url": "https://www.youtube.com/playlist?list=PLabc", "id": "PLabc", "title": "Mix", "position": 3 })
        );
        assert_eq!(
            json["files"],
            json!({
                "media": "x.mkv",
                "cover": "/covers/x-poster.jpg",
                "subtitles": [{ "lang": "en", "path": "subs/x.en.vtt" }],
            }),
            "relative under the metadata file's folder, absolute elsewhere"
        );
        assert_eq!(json["media"]["size_bytes"], 1234);
        assert_eq!(json["media"]["acodec"], Value::Null);
        assert_eq!(json["cover"]["width"], 1920);
        assert_eq!(json["tool"]["yt_dlp"], "2026.10.29");
        assert_eq!(
            json["embedded"],
            json!({ "metadata": true, "cover": false, "chapters": false, "subtitles": false })
        );
        assert_eq!(json["embed_error"], Value::Null);
    }
}
