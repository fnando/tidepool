//! The normalized, versioned `metadata.json` written next to each video.

use jiff::Timestamp;
use serde_json::{Value, json};

use super::info::VideoInfo;
use super::thumbnail::Cover;
use crate::config::merge::EffectiveSource;

/// Files in the video's directory, relative to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Files {
    pub media: String,
    pub cover: Option<String>,
    pub subtitles: Vec<String>,
}

pub struct Metadata<'a> {
    pub info: &'a VideoInfo,
    pub source: &'a EffectiveSource,
    pub source_title: Option<&'a str>,
    pub position: u32,
    pub files: &'a Files,
    pub media_bytes: u64,
    pub cover: Option<&'a Cover>,
    pub downloaded_at: Timestamp,
    pub ytdlp_version: Option<&'a str>,
}

impl Metadata<'_> {
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
            "files": {
                "media": self.files.media,
                "cover": self.files.cover,
                "subtitles": self.files.subtitles,
            },
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
        let files = Files {
            media: "media.mkv".into(),
            cover: Some("x-poster.jpg".into()),
            subtitles: vec!["media.en.vtt".into()],
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
        assert_eq!(json["files"]["subtitles"], json!(["media.en.vtt"]));
        assert_eq!(json["media"]["size_bytes"], 1234);
        assert_eq!(json["media"]["acodec"], Value::Null);
        assert_eq!(json["cover"]["width"], 1920);
        assert_eq!(json["tool"]["yt_dlp"], "2026.10.29");
    }
}
