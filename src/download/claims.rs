//! Which video each placed file belongs to, so two videos that render to
//! the same path don't overwrite each other: the second gets ` [<id>]`
//! added to its file names (the titles inside its files stay as they are).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use crate::state::items::VideoFiles;

/// Every recorded file's path and its video's ID.
#[derive(Debug, Default)]
pub struct Claims(Mutex<HashMap<PathBuf, String>>);

impl Claims {
    /// The files recorded for every video.
    ///
    /// # Errors
    ///
    /// If the DB read fails.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut stmt = conn.prepare("SELECT video_id, files FROM items WHERE files IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (video_id, files) = row?;
            if let Ok(files) = serde_json::from_str::<VideoFiles>(&files) {
                for path in files.paths() {
                    map.insert(PathBuf::from(path), video_id.clone());
                }
            }
        }
        Ok(Self(Mutex::new(map)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, String>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Make `files` `video_id`'s: when
    /// any of them is another video's, rename them all with
    /// [`disambiguate`] first. Returns the renames made (none when nothing
    /// was taken). Checked and claimed at once, so two videos placed
    /// together can't both take a path.
    pub fn claim(&self, video_id: &str, files: &mut VideoFiles) -> HashMap<PathBuf, PathBuf> {
        let mut map = self.lock();
        let taken = |p: &Path| map.get(p).is_some_and(|owner| owner != video_id);
        let paths: Vec<PathBuf> = files.paths().into_iter().map(PathBuf::from).collect();
        let renames = if paths.iter().any(|p| taken(p)) {
            disambiguate(video_id, Path::new(&files.media), &paths, &taken)
        } else {
            HashMap::new()
        };
        rename_files(files, &renames);
        for path in files.paths() {
            map.insert(PathBuf::from(path), video_id.to_string());
        }
        renames
    }

    /// Give up `video_id`'s claim on `paths` (it wasn't placed there, or
    /// moved away).
    pub fn release<'a>(&self, video_id: &str, paths: impl IntoIterator<Item = &'a str>) {
        let mut map = self.lock();
        for path in paths {
            if map
                .get(Path::new(path))
                .is_some_and(|owner| owner == video_id)
            {
                map.remove(Path::new(path));
            }
        }
    }
}

/// New names for `paths`, all next to where they were: files named after
/// the media file (`Song.mkv`, `Song-poster.jpg`, `Song.en.vtt`) get
/// ` [<video_id>]` after that name (`Song [id].mkv`, `Song [id]-poster.jpg`,
/// `Song [id].en.vtt`), so players still pair them; any other file that's
/// `taken` gets it before its extension (`metadata [id].json`).
#[must_use]
pub fn disambiguate(
    video_id: &str,
    media: &Path,
    paths: &[PathBuf],
    taken: &dyn Fn(&Path) -> bool,
) -> HashMap<PathBuf, PathBuf> {
    let suffix = format!(" [{video_id}]");
    let stem = media
        .file_stem()
        .map_or_default(|s| s.to_string_lossy().into_owned());
    let mut renames = HashMap::new();
    for path in paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let named_after_media = !stem.is_empty()
            && name
                .strip_prefix(&stem)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-', '_', ' ']));
        let new_name = if named_after_media {
            format!("{stem}{suffix}{}", &name[stem.len()..])
        } else if taken(path) {
            // The first dot after the first character (".hidden" has no extension).
            match name.char_indices().skip(1).find(|(_, c)| *c == '.') {
                Some((dot, _)) => format!("{}{suffix}{}", &name[..dot], &name[dot..]),
                None => format!("{name}{suffix}"),
            }
        } else {
            continue;
        };
        renames.insert(path.clone(), path.with_file_name(new_name));
    }
    renames
}

/// `files` with the paths in `renames` replaced.
fn rename_files(files: &mut VideoFiles, renames: &HashMap<PathBuf, PathBuf>) {
    if renames.is_empty() {
        return;
    }
    let rename = |path: &mut String| {
        if let Some(new) = renames.get(Path::new(path.as_str())) {
            *path = new.display().to_string();
        }
    };
    rename(&mut files.media);
    rename(&mut files.metadata);
    files.cover.as_mut().map(rename);
    files.nfo.as_mut().map(rename);
    files.raw_info.as_mut().map(rename);
    for sub in &mut files.subtitles {
        rename(&mut sub.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::items::Subtitle;

    fn files(dir: &str, name: &str) -> VideoFiles {
        VideoFiles {
            dir: dir.into(),
            media: format!("{dir}/{name}.mkv"),
            cover: Some(format!("{dir}/{name}-poster.jpg")),
            subtitles: vec![Subtitle {
                lang: "en".into(),
                path: format!("{dir}/{name}.en.vtt"),
            }],
            metadata: format!("{dir}/{name}.json"),
            nfo: Some(format!("{dir}/{name}.nfo")),
            raw_info: None,
            removed: Vec::new(),
        }
    }

    #[test]
    fn a_free_path_is_claimed_as_it_is() {
        let claims = Claims::default();
        let mut f = files("/m/AFI", "Miss Murder");
        assert!(claims.claim("aaaaaaaaaaa", &mut f).is_empty());
        assert_eq!(f.media, "/m/AFI/Miss Murder.mkv");
        // The same video again (a download after a reset): still its own.
        let mut again = files("/m/AFI", "Miss Murder");
        assert!(claims.claim("aaaaaaaaaaa", &mut again).is_empty());
    }

    #[test]
    fn another_videos_path_gets_the_id() {
        let claims = Claims::default();
        claims.claim("aaaaaaaaaaa", &mut files("/m/AFI", "Miss Murder"));
        let mut f = files("/m/AFI", "Miss Murder");
        let renames = claims.claim("bbbbbbbbbbb", &mut f);
        assert_eq!(renames.len(), 5);
        assert_eq!(f.media, "/m/AFI/Miss Murder [bbbbbbbbbbb].mkv");
        assert_eq!(
            f.cover.as_deref(),
            Some("/m/AFI/Miss Murder [bbbbbbbbbbb]-poster.jpg")
        );
        assert_eq!(
            f.subtitles[0].path,
            "/m/AFI/Miss Murder [bbbbbbbbbbb].en.vtt"
        );
        assert_eq!(f.metadata, "/m/AFI/Miss Murder [bbbbbbbbbbb].json");
        assert_eq!(
            f.nfo.as_deref(),
            Some("/m/AFI/Miss Murder [bbbbbbbbbbb].nfo")
        );
        assert_eq!(f.dir, "/m/AFI", "the folder stays");

        // A third one with the same name has its own ID.
        let mut third = files("/m/AFI", "Miss Murder");
        claims.claim("ccccccccccc", &mut third);
        assert_eq!(third.media, "/m/AFI/Miss Murder [ccccccccccc].mkv");
    }

    #[test]
    fn a_shared_file_not_named_after_the_media_gets_the_id_too() {
        let claims = Claims::default();
        let mut a = files("/m/AFI", "One");
        a.metadata = "/m/AFI/metadata.json".into();
        claims.claim("aaaaaaaaaaa", &mut a);
        let mut b = files("/m/AFI", "Two");
        b.metadata = "/m/AFI/metadata.json".into();
        claims.claim("bbbbbbbbbbb", &mut b);
        assert_eq!(b.metadata, "/m/AFI/metadata [bbbbbbbbbbb].json");
        assert_eq!(
            b.media, "/m/AFI/Two [bbbbbbbbbbb].mkv",
            "renamed as a group"
        );
    }

    #[test]
    fn released_paths_are_free_again() {
        let claims = Claims::default();
        let a = files("/m/AFI", "Song");
        claims.claim("aaaaaaaaaaa", &mut a.clone());
        claims.release("bbbbbbbbbbb", a.paths());
        let mut b = files("/m/AFI", "Song");
        assert!(
            !claims.claim("bbbbbbbbbbb", &mut b).is_empty(),
            "not its to release"
        );
        claims.release("aaaaaaaaaaa", a.paths());
        claims.release("bbbbbbbbbbb", b.paths());
        assert!(
            claims
                .claim("ccccccccccc", &mut files("/m/AFI", "Song"))
                .is_empty()
        );
    }
}
