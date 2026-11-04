//! Videos downloaded by hand (`general.backfill_dir`), for the ones tidepool
//! can't download itself (blocked in your country, age-restricted, …).
//!
//! [`candidates`] lists the media files waiting there; [`stage`] moves the
//! one picked for a video into the video's work area as if yt-dlp had just
//! downloaded it (`media.<ext>`, subtitles, `ytdlp.info.json`), so the
//! processor finishes it like any other download: hooks, cover, embedding,
//! placing.
//!
//! [`import_source`] does it on its own for the files in a source's folder
//! there (`<backfill_dir>/<source>/`), even ones that aren't on YouTube.

use std::io::Write;
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

/// Whether a video in `status` can be given a file from the backfill
/// folder: not while it's being downloaded or processed, nor once it's done.
#[must_use]
pub fn can_backfill(status: &str) -> bool {
    use crate::state::items::status;
    ![
        status::DOWNLOADING,
        status::FETCHED,
        status::PROCESSING,
        status::DONE,
    ]
    .contains(&status)
}

/// The media files in a source's own folder, `<dir>/<source_key>/` (not
/// its subfolders), by name.
#[must_use]
pub fn source_files(dir: &Path, source_key: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir.join(source_key)) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_media(p))
        .collect();
    out.sort();
    out
}

/// The YouTube ID in a name the way yt-dlp writes it ("Song [dQw4w9WgXcQ]").
fn id_in_name(stem: &str) -> Option<&str> {
    let inside = stem.strip_suffix(']')?;
    let (_, id) = inside.rsplit_once('[')?;
    (id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some(id)
}

/// The title in a file's name: without the ID yt-dlp puts at the end.
fn title_in_name(stem: &str) -> String {
    let title = match id_in_name(stem) {
        Some(id) => stem[..stem.len() - id.len() - 2].trim_end(),
        None => stem,
    };
    let title = title.trim();
    if title.is_empty() { stem } else { title }.to_string()
}

/// An ID for a file that isn't a YouTube video: 11 characters, like
/// YouTube's, from its name and size (the same file keeps its ID).
fn local_id(name: &str, bytes: u64) -> String {
    use sha2::Digest;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let digest = sha2::Sha256::digest(format!("{name}\0{bytes}").as_bytes());
    let bits = digest[..9]
        .iter()
        .fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
    (0..11)
        .map(|i| char::from(ALPHABET[((bits >> (66 - 6 * (i + 1))) & 63) as usize]))
        .collect()
}

/// The details of a file that came without any: its title from its name
/// (cleaned up when the `artist` is known, who's also its channel).
fn file_info(video_id: &str, title: &str, artist: Option<&str>, on_youtube: bool) -> String {
    let title = match artist {
        Some(artist) => crate::titles::music_video(title, artist),
        None => title.to_string(),
    };
    let (url, thumbnails) = if on_youtube {
        (
            format!("https://www.youtube.com/watch?v={video_id}"),
            ["hqdefault", "maxresdefault"]
                .iter()
                .map(|n| serde_json::json!({ "url": format!("https://i.ytimg.com/vi/{video_id}/{n}.jpg") }))
                .collect(),
        )
    } else {
        (String::new(), Vec::new())
    };
    serde_json::json!({
        "id": video_id,
        "title": title,
        "channel": artist,
        "webpage_url": url,
        "thumbnails": thumbnails,
    })
    .to_string()
}

/// One file of a source's folder, ready to be imported.
struct Found {
    /// Its file name.
    name: String,
    video_id: String,
    /// Its info JSON: its `.info.json`, or what indexing found.
    info: Option<String>,
    title: String,
    /// Its ID is YouTube's.
    on_youtube: bool,
}

/// The video `media` is for: the one its `.info.json` or name (`[<id>]`)
/// says, else one of its own.
fn identify(conn: &Connection, media: &Path) -> rusqlite::Result<Found> {
    let name = media
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = media
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sidecar = sidecars(media)
        .0
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| VideoInfo::parse(&t).ok().map(|info| (info, t)));
    Ok(if let Some((info, text)) = sidecar {
        Found {
            name,
            video_id: info.id,
            info: Some(text),
            title: info.title,
            on_youtube: true,
        }
    } else if let Some(id) = id_in_name(&stem) {
        Found {
            video_id: id.to_string(),
            info: index_info(conn, id)?,
            title: title_in_name(&stem),
            name,
            on_youtube: true,
        }
    } else {
        let bytes = std::fs::metadata(media).map_or(0, |m| m.len());
        Found {
            video_id: local_id(&name, bytes),
            info: None,
            title: title_in_name(&stem),
            name,
            on_youtube: false,
        }
    })
}

/// Take `found`'s video for importing (status `downloading`), adding it to
/// the source when it isn't there (`current` is its status). `false` when
/// something else took it first.
fn claim(
    conn: &Connection,
    key: &str,
    found: &Found,
    info: &str,
    current: Option<&str>,
) -> rusqlite::Result<bool> {
    use crate::state::items::status;
    if let Some(current) = current {
        // So a download can't start on it meanwhile.
        let claimed = conn.execute(
            "UPDATE items SET status = ?3 WHERE source_key = ?1 AND video_id = ?2 AND status = ?4",
            rusqlite::params![key, found.video_id, status::DOWNLOADING, current],
        )?;
        return Ok(claimed > 0);
    }
    let now = jiff::Timestamp::now().to_string();
    let parsed = VideoInfo::parse(info).ok();
    conn.execute(
        "INSERT OR IGNORE INTO sources (key, lists) VALUES (?1, '[]')",
        [key],
    )?;
    conn.execute(
        "INSERT INTO videos (id, title, channel, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO NOTHING",
        rusqlite::params![
            found.video_id,
            parsed
                .as_ref()
                .map_or(found.title.as_str(), |i| i.title.as_str()),
            parsed.as_ref().and_then(|i| i.channel.clone()),
            now
        ],
    )?;
    conn.execute(
        "INSERT INTO items (source_key, video_id, position, added_at, in_remote, status, lists, local)
         VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM items WHERE source_key = ?1),
                 ?3, 1, ?4, '[]', 1)",
        rusqlite::params![key, found.video_id, now, status::DOWNLOADING],
    )?;
    Ok(true)
}

/// Import the files in `source`'s own folder in `dir`: each becomes one of
/// its videos (a new one when the source doesn't have it), staged in its
/// work area under `tmp_root` for the processor, as if just downloaded.
/// The video is the one its `.info.json` or name (`[<id>]`) says; other
/// files get an ID of their own and their name as the title, cleaned up
/// when the source names its artist (`MUSICBRAINZ_ARTISTID`), whose name
/// comes from MusicBrainz. A file that can't be imported now stays.
/// Returns how many were.
///
/// # Errors
///
/// If a DB read or write fails.
pub async fn import_source(
    conn: &Connection,
    source: &crate::config::merge::EffectiveSource,
    dir: &Path,
    tmp_root: &Path,
    data_dir: &Path,
    out: &mut dyn Write,
) -> rusqlite::Result<usize> {
    use crate::state::items;
    let key = &source.key;
    let mut artist: Option<Option<String>> = None;
    let mut imported = 0;
    for media in source_files(dir, key) {
        let found = identify(conn, &media)?;
        let name = &found.name;
        let current: Option<String> = conn
            .query_row(
                "SELECT status FROM items WHERE source_key = ?1 AND video_id = ?2",
                [key, &found.video_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(current) = &current
            && !can_backfill(current)
        {
            tracing::info!(
                "{key}: {name} stays in the backfill folder: its video ({}) is {current}",
                found.video_id
            );
            continue;
        }
        let info = match &found.info {
            Some(info) => info.clone(),
            None if current.is_some() => index_info(conn, &found.video_id)?.unwrap_or_default(),
            None => {
                if artist.is_none() {
                    match artist_name(source, data_dir).await {
                        Ok(name) => artist = Some(name),
                        Err(e) => {
                            tracing::warn!(
                                "{key}: {name} stays in the backfill folder for now: {e}"
                            );
                            continue;
                        }
                    }
                }
                file_info(
                    &found.video_id,
                    &found.title,
                    artist.clone().flatten().as_deref(),
                    found.on_youtube,
                )
            }
        };
        if !claim(conn, key, &found, &info, current.as_deref())? {
            continue;
        }
        let video_id = &found.video_id;
        let work = super::work_area(tmp_root, video_id, key);
        match stage(&media, &work, video_id, &info) {
            Ok(()) => {
                items::mark_fetched(conn, key, video_id, jiff::Timestamp::now())?;
                tracing::info!("{key}: {video_id} given {name} from its backfill folder");
                let _ = writeln!(out, "{key}: imported {name} from the backfill folder");
                imported += 1;
            }
            Err(err) => {
                if let Some(current) = &current {
                    items::set_status(conn, key, video_id, current)?;
                } else {
                    conn.execute(
                        "DELETE FROM items WHERE source_key = ?1 AND video_id = ?2",
                        [key, video_id],
                    )?;
                }
                tracing::warn!("{key}: {name} stays in the backfill folder: {err}");
            }
        }
    }
    Ok(imported)
}

/// The name of the artist `source` names (`MUSICBRAINZ_ARTISTID`), from
/// MusicBrainz; `None` when it doesn't name one.
async fn artist_name(
    source: &crate::config::merge::EffectiveSource,
    data_dir: &Path,
) -> Result<Option<String>, String> {
    let values = super::layout::Values::default();
    let Some(id) = super::stages::artist_id(source, &values) else {
        return Ok(None);
    };
    crate::musicbrainz::artist_name(&source.embed.musicbrainz_server, data_dir, &id)
        .await
        .map(Some)
        .map_err(|e| format!("cannot look up the artist's name: {e}"))
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
    fn a_files_name_gives_its_video_and_title() {
        assert_eq!(id_in_name("Song [dQw4w9WgXcQ]"), Some("dQw4w9WgXcQ"));
        assert_eq!(id_in_name("Song [Official Video]"), None);
        assert_eq!(id_in_name("Song [Live]"), None);
        assert_eq!(id_in_name("Song"), None);
        assert_eq!(title_in_name("Song [dQw4w9WgXcQ]"), "Song");
        assert_eq!(title_in_name("[dQw4w9WgXcQ]"), "[dQw4w9WgXcQ]");
        assert_eq!(title_in_name("  Song (Live) "), "Song (Live)");
    }

    #[test]
    fn files_without_a_video_get_an_id_like_youtubes() {
        let id = local_id("Song.mp4", 100);
        assert_eq!(id.len(), 11);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(id, local_id("Song.mp4", 100), "the same file, the same ID");
        assert_ne!(id, local_id("Song.mp4", 101));
        assert_ne!(id, local_id("Other.mp4", 100));
        assert!(super::super::is_work_folder(&format!("{id}.music")));
    }

    #[test]
    fn a_sources_folder_has_its_own_media_files() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("lofi/b.mp4"), "x");
        touch(&dir.path().join("lofi/a.MKV"), "x");
        touch(&dir.path().join("lofi/a.info.json"), "{}");
        touch(&dir.path().join("lofi/.hidden.mp4"), "x");
        touch(&dir.path().join("lofi/deeper/c.mp4"), "x");
        touch(&dir.path().join("other.mp4"), "x");
        let names: Vec<String> = source_files(dir.path(), "lofi")
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.MKV", "b.mp4"]);
        assert!(source_files(dir.path(), "none").is_empty());
    }

    #[test]
    fn file_details_name_the_artist_and_clean_the_title_up() {
        let info = VideoInfo::parse(&file_info(
            "abcdefghijk",
            "Rick Astley - Song (Official Video)",
            Some("Rick Astley"),
            false,
        ))
        .unwrap();
        assert_eq!(info.title, "Song");
        assert_eq!(info.channel.as_deref(), Some("Rick Astley"));
        let info = VideoInfo::parse(&file_info(
            "abcdefghijk",
            "Racionais MC's - Negro Drama (Clipe Oficial)",
            Some("Racionais MC's"),
            false,
        ))
        .unwrap();
        assert_eq!(info.title, "Negro Drama");
        assert_eq!(info.webpage_url.as_deref(), Some(""));
        assert!(info.thumbnails().is_empty());
        let info = VideoInfo::parse(&file_info("dQw4w9WgXcQ", "Song", None, true)).unwrap();
        assert_eq!(info.title, "Song");
        assert_eq!(info.channel, None);
        assert_eq!(info.thumbnails().len(), 2);
    }

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
