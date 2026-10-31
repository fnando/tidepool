//! yt-dlp's per-video info JSON (`--write-info-json` or `-J`), reduced to the
//! fields tidepool uses.

use jiff::Timestamp;
use serde::Deserialize;

use crate::index::Thumbnail;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Chapter {
    pub start_time: f64,
    pub end_time: f64,
    pub title: String,
}

/// What a download (or a pre-download check) learned about a video.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VideoInfo {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub channel: Option<String>,
    pub channel_id: Option<String>,
    pub channel_url: Option<String>,
    /// `YYYYMMDD`.
    pub upload_date: Option<String>,
    pub timestamp: Option<i64>,
    pub duration: Option<f64>,
    pub tags: Option<Vec<String>>,
    pub categories: Option<Vec<String>>,
    pub chapters: Option<Vec<Chapter>>,
    pub format_id: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub webpage_url: Option<String>,
    #[serde(default)]
    thumbnails: Vec<RawThumbnail>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct RawThumbnail {
    url: String,
    width: Option<u32>,
    height: Option<u32>,
}

impl VideoInfo {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("unexpected yt-dlp info JSON: {e}"))
    }

    /// Publish time: the exact timestamp, else the upload date at midnight UTC.
    pub fn published_at(&self) -> Option<Timestamp> {
        self.timestamp
            .and_then(|t| Timestamp::from_second(t).ok())
            .or_else(|| {
                let date: jiff::civil::Date = self.upload_date_iso()?.parse().ok()?;
                date.to_zoned(jiff::tz::TimeZone::UTC)
                    .ok()
                    .map(|z| z.timestamp())
            })
    }

    /// `YYYY-MM-DD`.
    pub fn upload_date_iso(&self) -> Option<String> {
        let d = self.upload_date.as_deref()?;
        if d.len() != 8 || !d.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let iso = format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..]);
        iso.parse::<jiff::civil::Date>().ok().map(|_| iso)
    }

    pub fn duration_seconds(&self) -> Option<u64> {
        self.duration
            .filter(|d| *d >= 0.0)
            .map(|d| d.round() as u64)
    }

    pub fn thumbnails(&self) -> Vec<Thumbnail> {
        self.thumbnails
            .iter()
            .map(|t| Thumbnail {
                url: t.url.clone(),
                width: t.width,
                height: t.height,
            })
            .collect()
    }

    /// Codecs, with yt-dlp's "none" meaning absent.
    pub fn codec(value: &Option<String>) -> Option<String> {
        value.clone().filter(|c| c != "none")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> VideoInfo {
        let path = format!(
            "{}/tests/fixtures/ytdlp/videos/dQw4w9WgXcQ.info.json",
            env!("CARGO_MANIFEST_DIR")
        );
        VideoInfo::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn parses_real_info() {
        let info = fixture();
        assert_eq!(info.id, "dQw4w9WgXcQ");
        assert_eq!(info.channel.as_deref(), Some("Rick Astley"));
        assert_eq!(info.duration_seconds(), Some(213));
        assert_eq!(info.upload_date_iso().as_deref(), Some("2009-10-25"));
        assert_eq!(
            info.published_at(),
            Some(Timestamp::from_second(1256453853).unwrap())
        );
        assert_eq!(info.chapters.as_ref().unwrap()[1].title, "Song");
        assert!(
            info.tags
                .as_ref()
                .unwrap()
                .contains(&"rick roll".to_string())
        );
        let thumbs = info.thumbnails();
        assert!(thumbs.iter().any(
            |t| t.url.ends_with("vi_webp/dQw4w9WgXcQ/maxresdefault.webp") && t.width == Some(1920)
        ));
    }

    #[test]
    fn falls_back_to_the_upload_date() {
        let info = VideoInfo::parse(r#"{"id":"x","title":"t","upload_date":"20250301"}"#).unwrap();
        assert_eq!(info.upload_date_iso().as_deref(), Some("2025-03-01"));
        assert_eq!(
            info.published_at(),
            Some("2025-03-01T00:00:00Z".parse().unwrap())
        );
        let info = VideoInfo::parse(r#"{"id":"x","title":"t","upload_date":"bogus"}"#).unwrap();
        assert_eq!((info.upload_date_iso(), info.published_at()), (None, None));
        assert_eq!(VideoInfo::codec(&Some("none".into())), None);
    }
}
