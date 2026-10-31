//! Embedding tags, the cover, chapters and subtitles into the media file with
//! one ffmpeg stream-copy pass (no re-encoding).

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::info::Chapter;
use super::thumbnail::{self, CoverFormat};

/// What the container can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    /// mp4, m4v, mov: cover as an attached picture, subtitles as mov_text.
    Mp4,
    /// m4a: cover as an attached picture, no subtitles.
    M4a,
    /// mkv, mka: cover as an attachment, subtitles copied.
    Matroska,
    /// mp3 and flac: cover as an attached picture, no subtitles.
    Audio,
    /// webm: WebVTT subtitles only, no cover.
    Webm,
    /// ogg, opus and anything else: tags and chapters only.
    Other,
}

impl Container {
    fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" | "mov" => Self::Mp4,
            "m4a" => Self::M4a,
            "mkv" | "mka" => Self::Matroska,
            "mp3" | "flac" => Self::Audio,
            "webm" => Self::Webm,
            _ => Self::Other,
        }
    }

    fn takes_subtitle(self, ext: &str) -> bool {
        match self {
            Self::Mp4 => matches!(ext, "vtt" | "srt" | "ass" | "ssa"),
            Self::Matroska => matches!(ext, "vtt" | "srt" | "ass" | "ssa"),
            Self::Webm => ext == "vtt",
            _ => false,
        }
    }
}

/// What to put into the media file.
pub struct Request<'a> {
    pub media: &'a Path,
    /// Tags, in order (`metadata`).
    pub tags: Vec<(&'static str, String)>,
    /// The poster (`cover`).
    pub cover: Option<(&'a Path, CoverFormat)>,
    /// Chapter markers (`chapters`).
    pub chapters: &'a [Chapter],
    /// `(file, language)` for each subtitle (`subtitles`).
    pub subtitles: Vec<(PathBuf, String)>,
}

/// What ended up in the file, recorded in metadata.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Embedded {
    pub metadata: bool,
    pub cover: bool,
    pub chapters: bool,
    pub subtitles: bool,
}

impl Embedded {
    pub fn any(&self) -> bool {
        self.metadata || self.cover || self.chapters || self.subtitles
    }
}

/// The result of a successful pass: the new file (next to `media`), what it
/// holds, and what had to be left out.
#[derive(Debug)]
pub struct Output {
    pub path: PathBuf,
    pub embedded: Embedded,
    pub skipped: Vec<String>,
}

/// Run ffmpeg to write a copy of `request.media` with everything embedded.
/// Nothing to embed (or nothing the container can hold) returns `Ok(None)`.
pub async fn embed(
    ffmpeg: &Path,
    ffprobe: &Path,
    request: &Request<'_>,
) -> Result<Option<Output>, String> {
    let ext = request
        .media
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let container = Container::from_extension(&ext);
    let dir = request.media.parent().unwrap_or(Path::new("."));
    let mut skipped = Vec::new();
    let mut embedded = Embedded::default();
    let before = probe(ffprobe, request.media).await?;

    let mut inputs: Vec<Vec<String>> = vec![vec!["-i".into(), path_arg(request.media)]];
    let mut args: Vec<String> = vec![
        "-map".into(),
        "0".into(),
        "-map_metadata".into(),
        "0".into(),
    ];
    let mut added = 0;

    // Cover.
    if let Some((cover, format)) = request.cover {
        match container {
            Container::Mp4 | Container::M4a | Container::Audio => {
                // These containers take JPEG or PNG only.
                let cover = if format == CoverFormat::Webp {
                    let bytes = std::fs::read(cover).map_err(|e| e.to_string())?;
                    let (jpg, _) =
                        thumbnail::convert(bytes, image::ImageFormat::WebP, CoverFormat::Jpg)?;
                    let path = dir.join("embed-cover.jpg");
                    std::fs::write(&path, jpg).map_err(|e| e.to_string())?;
                    path
                } else {
                    cover.to_path_buf()
                };
                let index = inputs.len();
                inputs.push(vec!["-i".into(), path_arg(&cover)]);
                args.extend(["-map".into(), index.to_string()]);
                args.extend([
                    format!("-disposition:v:{}", before.video),
                    "attached_pic".into(),
                ]);
                added += 1;
                embedded.cover = true;
            }
            Container::Matroska => {
                let (mime, name) = match format {
                    CoverFormat::Jpg => ("image/jpeg", "cover.jpg"),
                    CoverFormat::Png => ("image/png", "cover.png"),
                    CoverFormat::Webp => ("image/webp", "cover.webp"),
                };
                args.extend([
                    "-attach".into(),
                    path_arg(cover),
                    "-metadata:s:t:0".into(),
                    format!("mimetype={mime}"),
                    "-metadata:s:t:0".into(),
                    format!("filename={name}"),
                ]);
                added += 1;
                embedded.cover = true;
            }
            Container::Webm | Container::Other => skipped.push(format!(
                "the cover (a .{ext} file can't hold one; it's still saved as cover_file)"
            )),
        }
    }

    // Subtitles.
    let mut subtitle_index = before.subtitle;
    for (path, lang) in &request.subtitles {
        let sub_ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !container.takes_subtitle(&sub_ext) {
            skipped.push(format!(
                "{lang} subtitles (a .{ext} file can't hold .{sub_ext} subtitles; they're still saved as subtitle_file)"
            ));
            continue;
        }
        let index = inputs.len();
        inputs.push(vec!["-i".into(), path_arg(path)]);
        args.extend(["-map".into(), format!("{index}:s")]);
        args.extend([
            format!("-metadata:s:s:{subtitle_index}"),
            format!("language={lang}"),
        ]);
        subtitle_index += 1;
        added += 1;
        embedded.subtitles = true;
    }

    // Chapters, through an ffmetadata file.
    if !request.chapters.is_empty() {
        let path = dir.join("embed-chapters.txt");
        std::fs::write(&path, ffmetadata(request.chapters)).map_err(|e| e.to_string())?;
        let index = inputs.len();
        inputs.push(vec![
            "-f".into(),
            "ffmetadata".into(),
            "-i".into(),
            path_arg(&path),
        ]);
        args.extend(["-map_chapters".into(), index.to_string()]);
        embedded.chapters = true;
    }

    // Tags.
    for (key, value) in &request.tags {
        args.extend(["-metadata".into(), format!("{key}={value}")]);
        embedded.metadata = true;
    }

    if !embedded.any() {
        return Ok(None);
    }

    args.extend(["-c".into(), "copy".into()]);
    if container == Container::Mp4 && embedded.subtitles {
        args.extend(["-c:s".into(), "mov_text".into()]);
    }
    if ext.eq_ignore_ascii_case("mp3") {
        args.extend(["-id3v2_version".into(), "3".into()]);
    }
    let output = dir.join(format!("embedded.{ext}"));
    let mut command: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    command.extend(inputs.into_iter().flatten());
    command.extend(args);
    command.push(path_arg(&output));

    let result = tokio::process::Command::new(ffmpeg)
        .args(&command)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("cannot run {}: {e}", ffmpeg.display()))?;
    if !result.status.success() {
        let _ = std::fs::remove_file(&output);
        let stderr = String::from_utf8_lossy(&result.stderr);
        let detail = stderr.lines().last().unwrap_or("").trim();
        return Err(format!("ffmpeg exited with {}: {detail}", result.status));
    }

    // Trust the new file only if ffprobe reads it with every stream.
    let after = probe(ffprobe, &output).await.inspect_err(|_| {
        let _ = std::fs::remove_file(&output);
    })?;
    // mp4 stores chapters as an extra text track, so more is fine.
    if after.total < before.total + added {
        let _ = std::fs::remove_file(&output);
        return Err(format!(
            "the embedded file has {} streams instead of {}",
            after.total,
            before.total + added
        ));
    }
    Ok(Some(Output {
        path: output,
        embedded,
        skipped,
    }))
}

/// Stream counts, by type.
#[derive(Debug, Default)]
struct Streams {
    total: usize,
    video: usize,
    subtitle: usize,
}

async fn probe(ffprobe: &Path, file: &Path) -> Result<Streams, String> {
    let result = tokio::process::Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("cannot run {}: {e}", ffprobe.display()))?;
    if !result.status.success() {
        return Err(format!(
            "ffprobe can't read {}: {}",
            file.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    let mut streams = Streams::default();
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let kind = line.trim().trim_end_matches(',');
        if kind.is_empty() {
            continue;
        }
        streams.total += 1;
        match kind {
            "video" => streams.video += 1,
            "subtitle" => streams.subtitle += 1,
            _ => {}
        }
    }
    if streams.total == 0 {
        return Err(format!("ffprobe found no streams in {}", file.display()));
    }
    Ok(streams)
}

/// ffmpeg reads `-i -foo` as an option; keep paths unambiguous.
fn path_arg(path: &Path) -> String {
    let text = path.display().to_string();
    if text.starts_with('-') {
        format!("./{text}")
    } else {
        text
    }
}

/// Chapters in ffmpeg's metadata format.
fn ffmetadata(chapters: &[Chapter]) -> String {
    let escape = |s: &str| {
        let mut out = String::new();
        for c in s.chars() {
            if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let mut out = String::from(";FFMETADATA1\n");
    for chapter in chapters {
        let ms = |s: f64| (s * 1000.0).round() as i64;
        out.push_str(&format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle={}\n",
            ms(chapter.start_time),
            ms(chapter.end_time),
            escape(&chapter.title)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn tool(name: &str) -> Option<PathBuf> {
        Command::new(name)
            .arg("-version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| PathBuf::from(name))
    }

    /// A one-second test file made by the real ffmpeg.
    fn sample(dir: &Path, name: &str, video: bool) -> PathBuf {
        let path = dir.join(name);
        let mut args: Vec<&str> = vec!["-hide_banner", "-loglevel", "error", "-y"];
        if video {
            args.extend(["-f", "lavfi", "-i", "testsrc=size=64x36:rate=10:duration=1"]);
        }
        args.extend(["-f", "lavfi", "-i", "sine=duration=1"]);
        if name.ends_with(".mp4") {
            args.extend(["-c:v", "mpeg4", "-c:a", "aac"]);
        }
        args.push(path.to_str().unwrap());
        assert!(
            Command::new("ffmpeg")
                .args(&args)
                .status()
                .unwrap()
                .success()
        );
        path
    }

    fn cover(dir: &Path, ext: &str, format: image::ImageFormat) -> PathBuf {
        let path = dir.join(format!("cover.{ext}"));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(32, 18)
            .write_to(&mut bytes, format)
            .unwrap();
        std::fs::write(&path, bytes.into_inner()).unwrap();
        path
    }

    fn probe_json(file: &Path) -> serde_json::Value {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "-of",
                "json",
            ])
            .arg(file)
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn tags() -> Vec<(&'static str, String)> {
        vec![
            ("title", "AFI - 17 Crimes".into()),
            ("artist", "AFI".into()),
            ("album_artist", "AFI".into()),
            ("genre", "Music".into()),
            ("date", "2013-08-16".into()),
            (
                "comment",
                "https://www.youtube.com/watch?v=Y9Vh8XGgqjE".into(),
            ),
            ("description", "Line one\nLine = two".into()),
            ("synopsis", "Line one\nLine = two".into()),
            ("album", "Music videos".into()),
            ("track", "3".into()),
        ]
    }

    fn chapters() -> Vec<Chapter> {
        vec![
            Chapter {
                start_time: 0.0,
                end_time: 0.5,
                title: "Intro; = #1".into(),
            },
            Chapter {
                start_time: 0.5,
                end_time: 1.0,
                title: "Outro".into(),
            },
        ]
    }

    #[test]
    fn writes_ffmetadata_chapters() {
        assert_eq!(
            ffmetadata(&chapters()),
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=500\ntitle=Intro\\; \\= \\#1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=500\nEND=1000\ntitle=Outro\n"
        );
    }

    #[tokio::test]
    async fn embeds_everything_into_mp4() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let media = sample(dir.path(), "media.mp4", true);
        let webp = cover(dir.path(), "webp", image::ImageFormat::WebP);
        let sub = dir.path().join("media.en.vtt");
        std::fs::write(&sub, "WEBVTT\n\n00:00.000 --> 00:00.900\nHello\n").unwrap();
        let chapters = chapters();
        let request = Request {
            media: &media,
            tags: tags(),
            cover: Some((&webp, CoverFormat::Webp)),
            chapters: &chapters,
            subtitles: vec![(sub, "en".into())],
        };
        let out = embed(&ffmpeg, &ffprobe, &request).await.unwrap().unwrap();
        assert_eq!(
            out.embedded,
            Embedded {
                metadata: true,
                cover: true,
                chapters: true,
                subtitles: true
            }
        );
        assert!(out.skipped.is_empty());
        let json = probe_json(&out.path);
        let tags = &json["format"]["tags"];
        assert_eq!(tags["title"], "AFI - 17 Crimes");
        assert_eq!(tags["artist"], "AFI");
        assert_eq!(tags["album_artist"], "AFI");
        assert_eq!(tags["genre"], "Music");
        assert_eq!(
            tags["comment"],
            "https://www.youtube.com/watch?v=Y9Vh8XGgqjE"
        );
        assert_eq!(tags["description"], "Line one\nLine = two");
        assert_eq!(tags["album"], "Music videos");
        assert_eq!(tags["track"], "3");
        let streams = json["streams"].as_array().unwrap();
        let pic = streams
            .iter()
            .find(|s| s["disposition"]["attached_pic"] == 1)
            .expect("an attached picture");
        assert_eq!(pic["codec_name"], "mjpeg", "WebP converted for mp4");
        let sub = streams
            .iter()
            .find(|s| s["codec_type"] == "subtitle")
            .unwrap();
        assert_eq!(sub["codec_name"], "mov_text");
        assert_eq!(json["chapters"].as_array().unwrap().len(), 2);
        assert_eq!(json["chapters"][0]["tags"]["title"], "Intro; = #1");
    }

    #[tokio::test]
    async fn embeds_into_mkv_and_skips_what_containers_cant_hold() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let media = sample(dir.path(), "media.mkv", true);
        let jpg = cover(dir.path(), "jpg", image::ImageFormat::Jpeg);
        let request = Request {
            media: &media,
            tags: tags(),
            cover: Some((&jpg, CoverFormat::Jpg)),
            chapters: &[],
            subtitles: vec![],
        };
        let out = embed(&ffmpeg, &ffprobe, &request).await.unwrap().unwrap();
        let json = probe_json(&out.path);
        let attachment = json["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["tags"]["mimetype"].is_string())
            .expect("a cover attachment");
        assert_eq!(attachment["tags"]["mimetype"], "image/jpeg");
        assert_eq!(json["format"]["tags"]["title"], "AFI - 17 Crimes");
        assert_eq!(json["format"]["tags"]["ARTIST"], "AFI");

        // opus: tags and chapters only; the cover and subtitles are skipped.
        let opus = sample(dir.path(), "media.opus", false);
        let sub = dir.path().join("media.en.vtt");
        std::fs::write(&sub, "WEBVTT\n\n00:00.000 --> 00:00.900\nHello\n").unwrap();
        let chapters = chapters();
        let request = Request {
            media: &opus,
            tags: tags(),
            cover: Some((&jpg, CoverFormat::Jpg)),
            chapters: &chapters,
            subtitles: vec![(sub, "en".into())],
        };
        let out = embed(&ffmpeg, &ffprobe, &request).await.unwrap().unwrap();
        assert_eq!(
            out.embedded,
            Embedded {
                metadata: true,
                cover: false,
                chapters: true,
                subtitles: false
            }
        );
        assert_eq!(out.skipped.len(), 2, "{:?}", out.skipped);
        assert!(out.skipped[0].starts_with("the cover (a .opus file can't hold one"));
        let json = probe_json(&out.path);
        assert_eq!(
            json["streams"][0]["tags"]["title"], "AFI - 17 Crimes",
            "Vorbis comments live on the stream"
        );
        assert_eq!(json["chapters"].as_array().unwrap().len(), 2);

        // Nothing to do.
        let request = Request {
            media: &media,
            tags: vec![],
            cover: None,
            chapters: &[],
            subtitles: vec![],
        };
        assert!(embed(&ffmpeg, &ffprobe, &request).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reports_ffmpeg_failures() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("media.mkv");
        std::fs::write(&bogus, "not a video").unwrap();
        let request = Request {
            media: &bogus,
            tags: tags(),
            cover: None,
            chapters: &[],
            subtitles: vec![],
        };
        let err = embed(&ffmpeg, &ffprobe, &request).await.unwrap_err();
        assert!(err.contains("ffprobe"), "{err}");
        assert!(!dir.path().join("embedded.mkv").exists());
    }
}
