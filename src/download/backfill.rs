//! Videos downloaded by hand (`general.backfill_dir`), for the ones tidepool
//! can't download itself (blocked in your country, age-restricted, …).
//!
//! [`candidates`] lists the media files waiting there; [`stage`] moves the
//! one picked for a video into the video's work area as if yt-dlp had just
//! downloaded it (`media.<ext>`, subtitles, `ytdlp.info.json`), so the
//! processor finishes it like any other download: hooks, cover, embedding,
//! placing.

use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use super::info::VideoInfo;
use super::{INFO_JSON, SUBTITLE_EXTENSIONS};

/// Media files that can be picked.
const MEDIA_EXTENSIONS: [&str; 12] = [
    "mp4", "mkv", "webm", "mov", "m4v", "m4a", "mp3", "opus", "ogg", "flac", "wav", "aac",
];

/// A media file in the backfill folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Its path inside the folder ("a.mp4" or "folder/a.mp4").
    pub name: String,
    pub bytes: u64,
    /// Its name has the video's ID.
    pub matches_id: bool,
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

fn is_media(path: &Path) -> bool {
    extension(path).is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.as_str()))
}

/// The media files in `dir` and its subfolders (one level), the ones whose
/// name has `video_id` first, then by name.
#[must_use]
pub fn candidates(dir: &Path, video_id: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let visit = |folder: &Path, prefix: &str, depth: usize, out: &mut Vec<Candidate>| {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return Vec::new();
        };
        let mut subfolders = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let shown = format!("{prefix}{name}");
            if meta.is_dir() {
                if depth == 0 {
                    subfolders.push((path, format!("{shown}/")));
                }
            } else if is_media(&path) {
                out.push(Candidate {
                    matches_id: name.contains(video_id),
                    name: shown,
                    bytes: meta.len(),
                });
            }
        }
        subfolders
    };
    for (folder, prefix) in visit(dir, "", 0, &mut out) {
        visit(&folder, &prefix, 1, &mut out);
    }
    out.sort_by(|a, b| b.matches_id.cmp(&a.matches_id).then(a.name.cmp(&b.name)));
    out
}

/// `name` inside `dir`, if it's a plain relative path (no `..`, not
/// absolute) to a media file that's there, and not a link out of `dir`.
///
/// # Errors
///
/// A message saying what's wrong with it.
pub fn resolve(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    let plain = !name.is_empty()
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(format!("{name:?} isn't a file in the backfill folder"));
    }
    let path = dir.join(relative);
    let gone = || format!("{name} isn't in the backfill folder anymore");
    let real = path.canonicalize().map_err(|_| gone())?;
    let root = dir.canonicalize().map_err(|_| gone())?;
    if !real.starts_with(&root) {
        return Err(format!("{name:?} isn't a file in the backfill folder"));
    }
    if !real.is_file() || !is_media(&path) {
        return Err(format!("{name} isn't a video or audio file"));
    }
    Ok(path)
}

/// The details tidepool has for `video_id` from indexing, shaped like
/// yt-dlp's info JSON, for a file that came without one. `None` when the
/// video isn't known.
///
/// # Errors
///
/// If the DB read fails.
pub fn index_info(conn: &Connection, video_id: &str) -> rusqlite::Result<Option<String>> {
    let row = conn
        .query_row(
            "SELECT title, channel, channel_id, description, tags_json, duration_s, published_at
             FROM videos WHERE id = ?1",
            [video_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((title, channel, channel_id, description, tags, duration, published)) = row else {
        return Ok(None);
    };
    let mut thumbnails: Vec<serde_json::Value> = crate::state::items::thumbnails(conn, video_id)?
        .into_iter()
        .map(|t| serde_json::json!({ "url": t.url, "width": t.width, "height": t.height }))
        .collect();
    if thumbnails.is_empty() {
        // YouTube serves these even for videos it won't play.
        thumbnails = ["hqdefault", "maxresdefault"]
            .iter()
            .map(|n| serde_json::json!({ "url": format!("https://i.ytimg.com/vi/{video_id}/{n}.jpg") }))
            .collect();
    }
    let published = published.and_then(|p| p.parse::<jiff::Timestamp>().ok());
    let info = serde_json::json!({
        "id": video_id,
        "title": title.unwrap_or_else(|| video_id.to_string()),
        "description": description,
        "channel": channel,
        "channel_id": channel_id,
        "channel_url": channel_id.as_ref().map(|id| format!("https://www.youtube.com/channel/{id}")),
        "upload_date": published.map(|p| p.strftime("%Y%m%d").to_string()),
        "timestamp": published.map(jiff::Timestamp::as_second),
        "duration": duration,
        "tags": tags.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()),
        "webpage_url": format!("https://www.youtube.com/watch?v={video_id}"),
        "thumbnails": thumbnails,
    });
    Ok(Some(info.to_string()))
}

/// Move a file, across disks too.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// The files that go with `media`: its `<stem>.info.json` and subtitles
/// (`<stem>.<lang>.<ext>`), as `(path, name in the work area)`.
fn sidecars(media: &Path) -> (Option<PathBuf>, Vec<(PathBuf, String)>) {
    let (Some(folder), Some(stem)) = (media.parent(), media.file_stem()) else {
        return (None, Vec::new());
    };
    let stem = stem.to_string_lossy();
    let info = folder.join(format!("{stem}.info.json"));
    let mut subtitles = Vec::new();
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&format!("{stem}.")) else {
            continue;
        };
        let Some((lang, ext)) = rest.rsplit_once('.') else {
            continue;
        };
        if !lang.is_empty() && SUBTITLE_EXTENSIONS.contains(&ext) {
            subtitles.push((entry.path(), format!("media.{lang}.{ext}")));
        }
    }
    subtitles.sort();
    (info.is_file().then_some(info), subtitles)
}

/// Move `media` (from [`resolve`]) and what goes with it into `work`'s
/// `fetched/`, ready to be processed. Its info JSON is the `.info.json` next
/// to it, if there's one for `video_id`, else `index_info`. On failure the
/// work area goes and the files stay where they were.
///
/// # Errors
///
/// A message when the `.info.json` is for another video or isn't valid, or
/// a file can't be moved.
pub fn stage(media: &Path, work: &Path, video_id: &str, index_info: &str) -> Result<(), String> {
    let (info_file, subtitles) = sidecars(media);
    let info_text = if let Some(path) = &info_file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let info = VideoInfo::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if info.id != video_id {
            return Err(format!(
                "{} is for the video {}, not {video_id}",
                path.display(),
                info.id
            ));
        }
        text
    } else {
        VideoInfo::parse(index_info)?;
        index_info.to_string()
    };
    let fetched = work.join("fetched");
    let _ = std::fs::remove_dir_all(work);
    let result = (|| {
        std::fs::create_dir_all(&fetched)
            .map_err(|e| format!("cannot create {}: {e}", fetched.display()))?;
        std::fs::write(fetched.join(INFO_JSON), &info_text)
            .map_err(|e| format!("cannot write {}: {e}", fetched.join(INFO_JSON).display()))?;
        let ext = extension(media).unwrap_or_default();
        let mut files = vec![(media.to_path_buf(), format!("media.{ext}"))];
        files.extend(subtitles);
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (from, name) in files {
            let to = fetched.join(name);
            if let Err(e) = move_file(&from, &to) {
                // Put back what already moved.
                for (from, to) in moved.iter().rev() {
                    let _ = move_file(to, from);
                }
                return Err(format!("cannot move {}: {e}", from.display()));
            }
            moved.push((from, to));
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Some(path) = info_file {
                let _ = std::fs::remove_file(path);
            }
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(work);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const INFO: &str = r#"{"id": "abc123", "title": "Song"}"#;

    #[test]
    fn lists_media_files_with_the_video_first() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("b.mp4"), "x");
        touch(&dir.path().join("a.mkv"), "x");
        touch(&dir.path().join("Song [abc123].webm"), "xx");
        touch(&dir.path().join("Song [abc123].info.json"), "{}");
        touch(&dir.path().join("Song [abc123].en.vtt"), "");
        touch(&dir.path().join(".hidden.mp4"), "");
        touch(&dir.path().join("album/c.m4a"), "x");
        touch(&dir.path().join("album/deeper/d.mp4"), "x");
        let found = candidates(dir.path(), "abc123");
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["Song [abc123].webm", "a.mkv", "album/c.m4a", "b.mp4"]
        );
        assert!(found[0].matches_id && !found[1].matches_id);
        assert_eq!(found[0].bytes, 2);
        assert!(candidates(&dir.path().join("missing"), "abc123").is_empty());
    }

    #[test]
    fn only_files_inside_the_folder_can_be_picked() {
        let dir = tempfile::tempdir().unwrap();
        let backfill = dir.path().join("backfill");
        touch(&backfill.join("a.mp4"), "x");
        touch(&backfill.join("notes.txt"), "x");
        touch(&dir.path().join("outside.mp4"), "x");
        std::os::unix::fs::symlink(dir.path().join("outside.mp4"), backfill.join("link.mp4"))
            .unwrap();
        assert!(resolve(&backfill, "a.mp4").is_ok());
        for name in [
            "../outside.mp4",
            "/etc/passwd",
            "",
            "link.mp4",
            "notes.txt",
            "nope.mp4",
        ] {
            assert!(resolve(&backfill, name).is_err(), "{name}");
        }
    }

    #[test]
    fn moves_the_file_and_what_goes_with_it_into_the_work_area() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("in/Song [abc123].MP4");
        touch(&media, "video");
        touch(&dir.path().join("in/Song [abc123].pt-BR.vtt"), "sub");
        touch(&dir.path().join("in/Song [abc123].info.json"), INFO);
        touch(&dir.path().join("in/other.mp4"), "other");
        let work = dir.path().join("tmp/abc123.lofi");
        stage(
            &media,
            &work,
            "abc123",
            r#"{"id": "abc123", "title": "From the index"}"#,
        )
        .unwrap();
        let fetched = work.join("fetched");
        assert_eq!(
            std::fs::read_to_string(fetched.join("media.mp4")).unwrap(),
            "video"
        );
        assert_eq!(
            std::fs::read_to_string(fetched.join("media.pt-BR.vtt")).unwrap(),
            "sub"
        );
        assert_eq!(
            std::fs::read_to_string(fetched.join(INFO_JSON)).unwrap(),
            INFO
        );
        let left: Vec<_> = std::fs::read_dir(dir.path().join("in"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["other.mp4"]);
    }

    #[test]
    fn without_an_info_json_the_index_details_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("song.mp3");
        touch(&media, "audio");
        let work = dir.path().join("work");
        stage(&media, &work, "abc123", INFO).unwrap();
        assert_eq!(
            std::fs::read_to_string(work.join("fetched").join(INFO_JSON)).unwrap(),
            INFO
        );
        assert!(work.join("fetched/media.mp3").is_file());
    }

    #[test]
    fn an_info_json_for_another_video_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("song.mp4");
        touch(&media, "video");
        touch(
            &dir.path().join("song.info.json"),
            r#"{"id": "zzz", "title": "Other"}"#,
        );
        let work = dir.path().join("work");
        let err = stage(&media, &work, "abc123", INFO).unwrap_err();
        assert!(err.contains("is for the video zzz"), "{err}");
        assert!(media.is_file(), "left where it was");
        assert!(!work.exists());
    }

    #[test]
    fn index_details_look_like_ytdlp_s() {
        let state = crate::state::State::open_in_memory().unwrap();
        state
            .conn
            .execute(
                "INSERT INTO videos (id, title, channel, channel_id, duration_s, published_at, tags_json)
                 VALUES ('abc123', 'Song', 'Band', 'UCx', 200, '2026-03-04T05:06:07Z', '[\"rock\"]')",
                [],
            )
            .unwrap();
        let text = index_info(&state.conn, "abc123").unwrap().unwrap();
        let info = VideoInfo::parse(&text).unwrap();
        assert_eq!(info.title, "Song");
        assert_eq!(info.channel.as_deref(), Some("Band"));
        assert_eq!(info.upload_date.as_deref(), Some("20260304"));
        assert_eq!(info.duration, Some(200.0));
        assert_eq!(info.tags, Some(vec!["rock".to_string()]));
        assert_eq!(
            info.channel_url.as_deref(),
            Some("https://www.youtube.com/channel/UCx")
        );
        assert_eq!(info.thumbnails().len(), 2);
        assert!(index_info(&state.conn, "nope").unwrap().is_none());
    }
}
