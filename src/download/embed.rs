//! Embedding tags, the cover, chapters and subtitles into the media file with
//! one ffmpeg stream-copy pass (no re-encoding).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::shutdown::Shutdown;

use serde::Serialize;

use super::info::Chapter;
use super::thumbnail::{self, CoverFormat};

/// What the container can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    /// mp4, m4v, mov: cover as an attached picture, subtitles as `mov_text`.
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
    /// Tags (`metadata`).
    pub tags: BTreeMap<String, String>,
    /// The poster (`cover`).
    pub cover: Option<(&'a Path, CoverFormat)>,
    /// Chapter markers (`chapters`).
    pub chapters: &'a [Chapter],
    /// `(file, language)` for each subtitle (`subtitles`).
    pub subtitles: Vec<(PathBuf, String)>,
    /// What to take out of the file when this pass doesn't add it (covers,
    /// subtitles, chapters or tags embedded before).
    pub remove: Embedded,
    /// Tags to take out of the file and every stream in it (`embed.tags`
    /// set to ""), e.g. `HANDLER_NAME`, which each stream carries.
    pub clear: Vec<String>,
}

/// What ended up in the file, recorded in metadata.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent flags, serialized as such into metadata.json and read field by field in app.rs"
)]
pub struct Embedded {
    pub metadata: bool,
    pub cover: bool,
    pub chapters: bool,
    pub subtitles: bool,
}

impl Embedded {
    #[must_use]
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
    /// What was taken out (see `Request::remove`).
    pub removed: Embedded,
    pub skipped: Vec<String>,
}

/// Run ffmpeg to write a copy of `request.media` with everything embedded.
/// Nothing to embed (or nothing the container can hold) returns `Ok(None)`.
///
/// # Errors
///
/// Returns a message when ffprobe or ffmpeg can't be run or fail (including
/// when stopping starts), a cover or chapters file can't be written, or the
/// new file is missing streams the original had.
pub async fn embed(
    ffmpeg: &Path,
    ffprobe: &Path,
    request: &Request<'_>,
    shutdown: &Shutdown,
    grace: Duration,
) -> Result<Option<Output>, String> {
    let run = |program: &Path, args: Vec<std::ffi::OsString>| {
        let program = program.to_path_buf();
        async move {
            let mut command = tokio::process::Command::new(&program);
            command.args(&args);
            crate::process::output(&mut command, grace, shutdown)
                .await
                .map_err(|e| match e {
                    crate::process::Error::Stopped => "stopped".to_string(),
                    e => format!("cannot run {}: {e}", program.display()),
                })
        }
    };
    let ext = request
        .media
        .extension()
        .map_or_default(|e| e.to_string_lossy().into_owned());
    let container = Container::from_extension(&ext);
    let dir = request.media.parent().unwrap_or(Path::new("."));
    let before = probe(ffprobe, request.media, &run).await?;

    let replaced = Replaced::new(request, container, &before);
    let mut pass = Pass::keeping(request, container, &before, &replaced);
    pass.add_cover(request, container, &before, dir, &ext)?;
    pass.add_subtitles(request, container, &ext);
    pass.add_chapters(request, dir)?;
    pass.add_tags(request, &before);

    if !pass.embedded.any() && !replaced.removed.any() {
        return Ok(None);
    }
    if replaced.keep_mkv_cover
        && let Some((index, codec)) = &before.first_picture
    {
        let (kept_cover, mime) =
            copy_cover_out(ffmpeg, request.media, dir, *index, codec, &run).await?;
        let name = format!("cover.{}", if mime == "image/png" { "png" } else { "jpg" });
        pass.args
            .extend(attach(&kept_cover, mime, &name, pass.attachments));
        pass.kept += 1;
    }

    let output = dir.join(format!("embedded.{ext}"));
    let command = pass.command(container, &ext, &output);
    let result = match run(ffmpeg, command.into_iter().map(Into::into).collect()).await {
        Ok(result) => result,
        Err(e) => {
            let _ = std::fs::remove_file(&output);
            return Err(e);
        }
    };
    if !result.status.success() {
        let _ = std::fs::remove_file(&output);
        let stderr = String::from_utf8_lossy(&result.stderr);
        let detail = stderr.lines().last().unwrap_or("").trim();
        return Err(format!("ffmpeg exited with {}: {detail}", result.status));
    }

    // Trust the new file only if ffprobe reads it with every stream.
    let after = probe(ffprobe, &output, &run).await.inspect_err(|_| {
        let _ = std::fs::remove_file(&output);
    })?;
    // mp4 stores chapters as an extra text track, so more is fine.
    if after.total < pass.kept + pass.added {
        let _ = std::fs::remove_file(&output);
        return Err(format!(
            "the embedded file has {} streams instead of {}",
            after.total,
            pass.kept + pass.added
        ));
    }
    pass.report_lost_tags(request, container, &ext, &after);
    Ok(Some(Output {
        path: output,
        embedded: pass.embedded,
        removed: replaced.removed,
        skipped: pass.skipped,
    }))
}

/// What a pass replaces: a file embedded before (or reprocessed) keeps one
/// cover, one set of subtitles and chapters, and exactly the given tags,
/// instead of piling them up.
struct Replaced {
    /// What's taken out without being replaced.
    removed: Embedded,
    /// What of the file's goes: replaced or removed.
    dropped: Embedded,
    /// Matroska keeps a cover as an attachment, but ffmpeg reads it as a
    /// picture stream and would write it back as a video track: a cover that
    /// stays is copied out and attached again.
    keep_mkv_cover: bool,
}

impl Replaced {
    fn new(request: &Request<'_>, container: Container, before: &Streams) -> Self {
        let new_cover = request.cover.is_some()
            && matches!(
                container,
                Container::Mp4 | Container::M4a | Container::Audio | Container::Matroska
            );
        let new_subtitles = request.subtitles.iter().any(|(path, _)| {
            let sub_ext = path
                .extension()
                .map_or_default(|e| e.to_string_lossy().to_ascii_lowercase());
            container.takes_subtitle(&sub_ext)
        });
        let new_chapters = !request.chapters.is_empty();
        let remove = &request.remove;
        let removed = Embedded {
            cover: remove.cover
                && !new_cover
                && (before.pictures > 0 || before.image_attachments() > 0),
            subtitles: remove.subtitles && !new_subtitles && before.subtitle > 0,
            chapters: remove.chapters && !new_chapters && before.chapters > 0,
            metadata: remove.metadata && request.tags.is_empty() && before.tags > 0,
        };
        let dropped = Embedded {
            cover: new_cover || removed.cover,
            subtitles: new_subtitles || removed.subtitles,
            chapters: new_chapters || removed.chapters,
            metadata: !request.tags.is_empty() || removed.metadata,
        };
        Self {
            keep_mkv_cover: container == Container::Matroska
                && !dropped.cover
                && before.pictures > 0,
            removed,
            dropped,
        }
    }
}

/// The ffmpeg command one pass builds, and what it puts in the file.
#[derive(Default)]
struct Pass {
    /// Each input's arguments (`-i <file>`, with any format options).
    inputs: Vec<Vec<String>>,
    /// Mapping, metadata and attachment arguments.
    args: Vec<String>,
    /// Streams kept from the original file.
    kept: usize,
    /// Streams added to it.
    added: usize,
    /// Attachments in the output so far; new ones are numbered after them.
    attachments: usize,
    embedded: Embedded,
    skipped: Vec<String>,
}

impl Pass {
    /// Start with the streams of `request.media` this pass keeps.
    fn keeping(
        request: &Request<'_>,
        container: Container,
        before: &Streams,
        replaced: &Replaced,
    ) -> Self {
        let mut pass = Self {
            inputs: vec![vec!["-i".into(), path_arg(request.media)]],
            kept: before.real_video + before.audio,
            ..Self::default()
        };
        let args = &mut pass.args;
        let map = |args: &mut Vec<String>, spec: &str| {
            args.extend(["-map".into(), format!("0:{spec}?")]);
        };
        if replaced.dropped.cover || replaced.keep_mkv_cover {
            map(args, "V"); // video without attached pictures
        } else {
            map(args, "v");
            pass.kept += before.pictures;
        }
        map(args, "a");
        if !replaced.dropped.subtitles {
            map(args, "s");
            pass.kept += before.subtitle;
        }
        // mp4 stores chapters as a data track; new chapters bring their own.
        if !(replaced.dropped.chapters && matches!(container, Container::Mp4 | Container::M4a)) {
            map(args, "d");
            pass.kept += before.data;
        }
        if replaced.dropped.cover && before.image_attachments() > 0 {
            // A Matroska cover is an image attachment: keep the others (fonts…).
            for (n, _) in before
                .attachments
                .iter()
                .enumerate()
                .filter(|(_, image)| !**image)
            {
                map(args, &format!("t:{n}"));
                pass.attachments += 1;
            }
        } else {
            map(args, "t");
            pass.attachments = before.attachments.len();
        }
        pass.kept += pass.attachments;
        if replaced.removed.chapters {
            args.extend(["-map_chapters".into(), "-1".into()]);
        }
        if replaced.dropped.metadata {
            // Exactly the given tags: drop the file's global ones (stream and
            // chapter metadata stay).
            args.extend(["-map_metadata:g".into(), "-1".into()]);
            if container == Container::Other {
                // ogg/opus keep tags as Vorbis comments on the audio stream.
                args.extend(["-map_metadata:s:a".into(), "-1".into()]);
            }
        }
        pass
    }

    /// Add the cover: an attached picture, or an attachment in Matroska.
    fn add_cover(
        &mut self,
        request: &Request<'_>,
        container: Container,
        before: &Streams,
        dir: &Path,
        ext: &str,
    ) -> Result<(), String> {
        let Some((cover, format)) = request.cover else {
            return Ok(());
        };
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
                let index = self.inputs.len();
                self.inputs.push(vec!["-i".into(), path_arg(&cover)]);
                self.args.extend(["-map".into(), index.to_string()]);
                self.args.extend([
                    format!("-disposition:v:{}", before.real_video),
                    "attached_pic".into(),
                ]);
                self.added += 1;
                self.embedded.cover = true;
            }
            Container::Matroska => {
                let (mime, name) = match format {
                    CoverFormat::Jpg => ("image/jpeg", "cover.jpg"),
                    CoverFormat::Png => ("image/png", "cover.png"),
                    CoverFormat::Webp => ("image/webp", "cover.webp"),
                };
                self.args
                    .extend(attach(cover, mime, name, self.attachments));
                self.attachments += 1;
                self.added += 1;
                self.embedded.cover = true;
            }
            Container::Webm | Container::Other => self.skipped.push(format!(
                "the cover (a .{ext} file can't hold one; it's still saved as cover_file)"
            )),
        }
        Ok(())
    }

    /// Add the subtitles the container can hold.
    fn add_subtitles(&mut self, request: &Request<'_>, container: Container, ext: &str) {
        let mut subtitle_index = 0;
        for (path, lang) in &request.subtitles {
            let sub_ext = path
                .extension()
                .map_or_default(|e| e.to_string_lossy().to_ascii_lowercase());
            if !container.takes_subtitle(&sub_ext) {
                self.skipped.push(format!(
                    "{lang} subtitles (a .{ext} file can't hold .{sub_ext} subtitles; they're still saved as subtitle_file)"
                ));
                continue;
            }
            let index = self.inputs.len();
            self.inputs.push(vec!["-i".into(), path_arg(path)]);
            self.args.extend(["-map".into(), format!("{index}:s")]);
            self.args.extend([
                format!("-metadata:s:s:{subtitle_index}"),
                format!("language={lang}"),
            ]);
            subtitle_index += 1;
            self.added += 1;
            self.embedded.subtitles = true;
        }
    }

    /// Add the chapters, through an ffmetadata file in `dir`.
    fn add_chapters(&mut self, request: &Request<'_>, dir: &Path) -> Result<(), String> {
        if request.chapters.is_empty() {
            return Ok(());
        }
        let path = dir.join("embed-chapters.txt");
        std::fs::write(&path, ffmetadata(request.chapters)).map_err(|e| e.to_string())?;
        let index = self.inputs.len();
        self.inputs.push(vec![
            "-f".into(),
            "ffmetadata".into(),
            "-i".into(),
            path_arg(&path),
        ]);
        self.args
            .extend(["-map_chapters".into(), index.to_string()]);
        self.embedded.chapters = true;
        Ok(())
    }

    /// Add the tags, and clear the ones to clear wherever the file has them.
    fn add_tags(&mut self, request: &Request<'_>, before: &Streams) {
        for (key, value) in &request.tags {
            self.args
                .extend(["-metadata".into(), format!("{key}={value}")]);
            self.embedded.metadata = true;
        }
        for key in &request.clear {
            let name = key.to_ascii_lowercase();
            let global = before.tag_names.contains(&name);
            let streams = before.stream_tag_names.contains(&name);
            if global {
                self.args.extend(["-metadata".into(), format!("{key}=")]);
            }
            if streams {
                self.args.extend(["-metadata:s".into(), format!("{key}=")]);
            }
            if global || streams {
                self.embedded.metadata = true;
            }
        }
    }

    /// The full ffmpeg command line, writing to `output`.
    fn command(&self, container: Container, ext: &str, output: &Path) -> Vec<String> {
        let mut command: Vec<String> = vec![
            "-hide_banner".into(),
            "-nostdin".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
        ];
        command.extend(self.inputs.iter().flatten().cloned());
        command.extend(self.args.iter().cloned());
        command.extend(["-c".into(), "copy".into()]);
        if container == Container::Mp4 && self.embedded.subtitles {
            command.extend(["-c:s".into(), "mov_text".into()]);
        }
        if ext.eq_ignore_ascii_case("mp3") {
            command.extend(["-id3v2_version".into(), "3".into()]);
        }
        command.push(path_arg(output));
        command
    }

    /// Say which tags didn't make it into the new file (`after`).
    fn report_lost_tags(
        &mut self,
        request: &Request<'_>,
        container: Container,
        ext: &str,
        after: &Streams,
    ) {
        // Containers only keep the tags they know (mp4 drops the rest without
        // a word); say which didn't make it. ogg/opus keep tags on the audio
        // stream, where they're all kept.
        if container != Container::Other {
            for key in request.tags.keys() {
                if !after.tag_names.contains(&key.to_ascii_lowercase()) {
                    self.skipped
                        .push(format!("tag {key} (a .{ext} file can't hold it)"));
                }
            }
        }
        // Some containers write a few stream tags on their own (mp4's
        // handler name): say so when one couldn't be cleared.
        for key in &request.clear {
            let name = key.to_ascii_lowercase();
            if after.tag_names.contains(&name) || after.stream_tag_names.contains(&name) {
                self.skipped
                    .push(format!("removing tag {key} (a .{ext} file always has it)"));
            }
        }
    }
}

/// Copy a Matroska file's cover (stream `index`, in `codec`) out of `media`
/// into `dir`, to attach it again. Returns the copy and its MIME type.
async fn copy_cover_out<F, Fut>(
    ffmpeg: &Path,
    media: &Path,
    dir: &Path,
    index: usize,
    codec: &str,
    run: &F,
) -> Result<(PathBuf, &'static str), String>
where
    F: Fn(&Path, Vec<std::ffi::OsString>) -> Fut,
    Fut: std::future::Future<Output = Result<std::process::Output, String>>,
{
    let (ext, mime) = if codec == "png" {
        ("png", "image/png")
    } else {
        ("jpg", "image/jpeg")
    };
    let kept_cover = dir.join(format!("kept-cover.{ext}"));
    let copied = run(
        ffmpeg,
        ["-hide_banner", "-nostdin", "-loglevel", "error", "-y", "-i"]
            .into_iter()
            .map(Into::into)
            .chain([
                path_arg(media).into(),
                "-map".into(),
                format!("0:{index}").into(),
                "-c".into(),
                "copy".into(),
                "-frames:v".into(),
                "1".into(),
                path_arg(&kept_cover).into(),
            ])
            .collect(),
    )
    .await?;
    if !copied.status.success() {
        return Err(format!(
            "cannot copy the cover out of {}: {}",
            media.display(),
            String::from_utf8_lossy(&copied.stderr).trim()
        ));
    }
    Ok((kept_cover, mime))
}

/// What a file holds, by stream type.
#[derive(Debug, Default)]
struct Streams {
    total: usize,
    /// Video that isn't an attached picture.
    real_video: usize,
    /// Attached pictures (covers), in any container.
    pictures: usize,
    /// The first attached picture: its stream index and codec.
    first_picture: Option<(usize, String)>,
    audio: usize,
    subtitle: usize,
    data: usize,
    /// Each attachment, in order: whether it's an image (a Matroska cover).
    attachments: Vec<bool>,
    chapters: usize,
    /// Global tags, not counting the ones muxers write themselves.
    tags: usize,
    /// Every global tag's name, lowercased.
    tag_names: Vec<String>,
    /// Every stream tag's name, lowercased.
    stream_tag_names: Vec<String>,
}

impl Streams {
    fn image_attachments(&self) -> usize {
        self.attachments.iter().filter(|image| **image).count()
    }
}

/// Global tags ffmpeg and the muxers write on their own.
const MUXER_TAGS: [&str; 5] = [
    "encoder",
    "major_brand",
    "minor_version",
    "compatible_brands",
    "creation_time",
];

async fn probe<F, Fut>(ffprobe: &Path, file: &Path, run: &F) -> Result<Streams, String>
where
    F: Fn(&Path, Vec<std::ffi::OsString>) -> Fut,
    Fut: std::future::Future<Output = Result<std::process::Output, String>>,
{
    let args = [
        "-v",
        "error",
        "-show_entries",
        "stream=index,codec_type,codec_name:stream_disposition=attached_pic:stream_tags:format_tags",
        "-show_chapters",
        "-of",
        "json",
    ]
    .into_iter()
    .map(Into::into)
    .chain(std::iter::once(file.as_os_str().to_owned()))
    .collect();
    let result = run(ffprobe, args).await?;
    if !result.status.success() {
        return Err(format!(
            "ffprobe can't read {}: {}",
            file.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&result.stdout)
        .map_err(|e| format!("ffprobe's output for {} isn't JSON: {e}", file.display()))?;
    let mut streams = Streams::default();
    for stream in json["streams"].as_array().into_iter().flatten() {
        for key in stream["tags"]
            .as_object()
            .into_iter()
            .flat_map(|t| t.keys())
        {
            let key = key.to_ascii_lowercase();
            if !streams.stream_tag_names.contains(&key) {
                streams.stream_tag_names.push(key);
            }
        }
        let picture = stream["disposition"]["attached_pic"] == 1;
        streams.total += 1;
        match stream["codec_type"].as_str().unwrap_or_default() {
            "video" if picture => {
                streams.pictures += 1;
                if streams.first_picture.is_none() {
                    let index = stream["index"]
                        .as_u64()
                        .and_then(|i| usize::try_from(i).ok())
                        .unwrap_or_default();
                    let codec = stream["codec_name"].as_str().unwrap_or_default();
                    streams.first_picture = Some((index, codec.to_string()));
                }
            }
            "video" => streams.real_video += 1,
            "audio" => streams.audio += 1,
            "subtitle" => streams.subtitle += 1,
            "data" => streams.data += 1,
            "attachment" => {
                let mime = stream["tags"]["mimetype"].as_str().unwrap_or_default();
                streams.attachments.push(mime.starts_with("image/"));
            }
            _ => {}
        }
    }
    streams.chapters = json["chapters"].as_array().map_or(0, Vec::len);
    streams.tag_names = json["format"]["tags"]
        .as_object()
        .map_or_default(|tags| tags.keys().map(|k| k.to_ascii_lowercase()).collect());
    streams.tags = streams
        .tag_names
        .iter()
        .filter(|k| !MUXER_TAGS.contains(&k.as_str()))
        .count();
    if streams.total == 0 {
        return Err(format!("ffprobe found no streams in {}", file.display()));
    }
    Ok(streams)
}

/// `-attach` arguments for an image, the `n`th attachment of the output.
fn attach(path: &Path, mime: &str, name: &str, n: usize) -> Vec<String> {
    vec![
        "-attach".into(),
        path_arg(path),
        format!("-metadata:s:t:{n}"),
        format!("mimetype={mime}"),
        format!("-metadata:s:t:{n}"),
        format!("filename={name}"),
    ]
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
        let _ = write!(
            out,
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle={}\n",
            millis(chapter.start_time),
            millis(chapter.end_time),
            escape(&chapter.title)
        );
    }
    out
}

/// Seconds as whole milliseconds, rounded.
#[expect(
    clippy::cast_possible_truncation,
    reason = "std has no checked float-to-int conversion; `as` saturates, far past any chapter time"
)]
fn millis(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
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

    fn tags() -> BTreeMap<String, String> {
        [
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
        .into_iter()
        .map(|(k, v): (&str, String)| (k.to_string(), v))
        .collect()
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
    async fn says_which_tags_a_file_cant_hold() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut tags = tags();
        tags.insert("MUSICBRAINZ_ARTISTID".into(), "abc".into());
        for (name, kept) in [("media.mp4", false), ("media.mkv", true)] {
            let media = sample(dir.path(), name, true);
            let request = Request {
                media: &media,
                tags: tags.clone(),
                cover: None,
                chapters: &[],
                subtitles: Vec::new(),
                remove: Embedded::default(),
                clear: Vec::new(),
            };
            let out = embed(
                &ffmpeg,
                &ffprobe,
                &request,
                &Shutdown::default(),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
            let json = probe_json(&out.path);
            let has = json["format"]["tags"]
                .as_object()
                .unwrap()
                .keys()
                .any(|k| k.eq_ignore_ascii_case("musicbrainz_artistid"));
            assert_eq!(has, kept, "{name}");
            let ext = name.rsplit('.').next().unwrap();
            let warning = format!("tag MUSICBRAINZ_ARTISTID (a .{ext} file can't hold it)");
            assert_eq!(
                out.skipped.contains(&warning),
                !kept,
                "{name}: {:?}",
                out.skipped
            );
            assert_eq!(
                out.skipped.len(),
                usize::from(!kept),
                "{name}: only that one"
            );
            std::fs::remove_file(&out.path).unwrap();
        }
    }

    #[tokio::test]
    async fn clears_tags_from_the_file_and_its_streams() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // Like yt-dlp's merge of YouTube's mp4 parts: a handler name per stream.
        let src = sample(dir.path(), "src.mp4", true);
        let media = dir.path().join("media.mkv");
        assert!(
            Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&src)
                .args([
                    "-c",
                    "copy",
                    "-metadata:s",
                    "handler_name=ISO Media file produced by Google Inc."
                ])
                .arg(&media)
                .status()
                .unwrap()
                .success()
        );
        let handler = |file: &Path| {
            probe_json(file)["streams"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| {
                    s["tags"]
                        .as_object()
                        .is_some_and(|t| t.keys().any(|k| k.eq_ignore_ascii_case("handler_name")))
                })
                .count()
        };
        assert_eq!(handler(&media), 2);
        let request = Request {
            media: &media,
            tags: BTreeMap::new(),
            cover: None,
            chapters: &[],
            subtitles: Vec::new(),
            remove: Embedded::default(),
            clear: vec!["HANDLER_NAME".into()],
        };
        let out = embed(
            &ffmpeg,
            &ffprobe,
            &request,
            &Shutdown::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .expect("a pass, only to clear the tag");
        assert_eq!(handler(&out.path), 0);
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);

        // Already gone: nothing to do.
        let media = dir.path().join("again.mkv");
        std::fs::rename(&out.path, &media).unwrap();
        let request = Request {
            media: &media,
            ..request
        };
        assert!(
            embed(
                &ffmpeg,
                &ffprobe,
                &request,
                &Shutdown::default(),
                Duration::from_secs(5)
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn removes_what_is_switched_off() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::default();
        let grace = Duration::from_secs(5);
        let media = sample(dir.path(), "media.mp4", true);
        let jpg = cover(dir.path(), "jpg", image::ImageFormat::Jpeg);
        let sub = dir.path().join("media.en.vtt");
        std::fs::write(&sub, "WEBVTT\n\n00:00.000 --> 00:00.900\nHello\n").unwrap();
        let chapters = chapters();
        let everything = Request {
            media: &media,
            tags: tags(),
            cover: Some((&jpg, CoverFormat::Jpg)),
            chapters: &chapters,
            subtitles: vec![(sub, "en".into())],
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        let full = embed(&ffmpeg, &ffprobe, &everything, &shutdown, grace)
            .await
            .unwrap()
            .unwrap();
        let all = Embedded {
            metadata: true,
            cover: true,
            chapters: true,
            subtitles: true,
        };
        assert_eq!(full.embedded, all);
        let media = dir.path().join("again.mp4");
        std::fs::rename(&full.path, &media).unwrap();

        // Nothing to add or remove: nothing to do.
        let nothing = Request {
            media: &media,
            tags: BTreeMap::new(),
            cover: None,
            chapters: &[],
            subtitles: Vec::new(),
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        assert!(
            embed(&ffmpeg, &ffprobe, &nothing, &shutdown, grace)
                .await
                .unwrap()
                .is_none()
        );

        let strip = Request {
            remove: all.clone(),
            ..nothing
        };
        let out = embed(&ffmpeg, &ffprobe, &strip, &shutdown, grace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.removed, all);
        assert_eq!(out.embedded, Embedded::default());
        let json = probe_json(&out.path);
        let kinds: Vec<&str> = json["streams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["codec_type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["video", "audio"],
            "no cover, subtitles or chapter track"
        );
        assert!(json["chapters"].as_array().unwrap().is_empty());
        let tags = json["format"]["tags"].as_object().unwrap();
        assert!(
            !tags.contains_key("title") && !tags.contains_key("artist"),
            "{tags:?}"
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
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        let out = embed(
            &ffmpeg,
            &ffprobe,
            &request,
            &Shutdown::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
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
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        let out = embed(
            &ffmpeg,
            &ffprobe,
            &request,
            &Shutdown::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
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
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        let out = embed(
            &ffmpeg,
            &ffprobe,
            &request,
            &Shutdown::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
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
            tags: BTreeMap::new(),
            cover: None,
            chapters: &[],
            subtitles: vec![],
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        assert!(
            embed(
                &ffmpeg,
                &ffprobe,
                &request,
                &Shutdown::default(),
                Duration::from_secs(5)
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn embedding_again_replaces_instead_of_stacking() {
        let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
            eprintln!("skipped: ffmpeg not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let jpg = cover(dir.path(), "jpg", image::ImageFormat::Jpeg);
        let sub = dir.path().join("media.en.vtt");
        std::fs::write(&sub, "WEBVTT\n\n00:00.000 --> 00:00.900\nHello\n").unwrap();
        let chapters = chapters();
        for (name, video) in [
            ("media.mp4", true),
            ("media.mkv", true),
            ("media.opus", false),
        ] {
            let media = sample(dir.path(), name, video);
            let mut counts = Vec::new();
            for title in ["First", "Second"] {
                let mut tags = tags();
                tags.insert("title".into(), title.into());
                if title == "Second" {
                    tags.remove("genre");
                }
                let request = Request {
                    media: &media,
                    tags,
                    cover: Some((&jpg, CoverFormat::Jpg)),
                    chapters: &chapters,
                    subtitles: vec![(sub.clone(), "en".into())],
                    remove: Embedded::default(),
                    clear: Vec::new(),
                };
                let out = embed(
                    &ffmpeg,
                    &ffprobe,
                    &request,
                    &Shutdown::default(),
                    Duration::from_secs(5),
                )
                .await
                .unwrap()
                .unwrap();
                std::fs::rename(&out.path, &media).unwrap();
                let json = probe_json(&media);
                let streams = json["streams"].as_array().unwrap();
                counts.push((
                    streams.len(),
                    streams
                        .iter()
                        .filter(|s| s["disposition"]["attached_pic"] == 1)
                        .count(),
                    streams
                        .iter()
                        .filter(|s| s["codec_type"] == "subtitle")
                        .count(),
                    json["chapters"].as_array().unwrap().len(),
                ));
                let tags = if video {
                    &json["format"]["tags"]
                } else {
                    &streams[0]["tags"]
                };
                let get = |k: &str| tags.get(k).or_else(|| tags.get(k.to_uppercase()));
                assert_eq!(get("title").unwrap(), title, "{name}");
                if title == "Second" {
                    assert!(
                        get("genre").is_none(),
                        "{name}: a dropped tag is gone: {tags}"
                    );
                }
            }
            assert_eq!(
                counts[0], counts[1],
                "{name}: the same streams after embedding twice"
            );
        }
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
            remove: Embedded::default(),
            clear: Vec::new(),
        };
        let err = embed(
            &ffmpeg,
            &ffprobe,
            &request,
            &Shutdown::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.contains("ffprobe"), "{err}");
        assert!(!dir.path().join("embedded.mkv").exists());
    }
}
