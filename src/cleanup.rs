//! `tidepool cleanup`: remove downloaded videos a source's `cleanup` limits
//! no longer allow.
//!
//! [`plan`] decides (no I/O): `keep_latest` protects the newest; then
//! `unmatched`, `marker_file` and `max_age` remove on their own; then
//! `max_items` and `max_size` remove from the oldest end (per `order`) until
//! the rest fits. Videos whose media is already gone don't count.
//!
//! [`remove`] takes out only the files tidepool recorded for the video (never
//! a whole folder: folders can be shared), moving them to the trash or
//! deleting them; an emptied folder goes too.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use jiff::Timestamp;

use crate::config::merge::Cleanup;
use crate::config::{CleanupKeep, CleanupOrder, Unmatched};
use crate::state::items::VideoFiles;

/// A downloaded video cleanup may remove.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub video_id: String,
    pub position: i64,
    pub bytes: u64,
    pub downloaded_at: Option<Timestamp>,
    pub published_at: Option<Timestamp>,
    /// Its media file is there.
    pub present: bool,
    /// Why the rules no longer match it, if they don't.
    pub unmatched: Option<String>,
    /// Its folder has the `marker_file`.
    pub marked: bool,
}

/// Why a video goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Unmatched(String),
    Marker(String),
    MaxAge(String),
    MaxItems(u32),
    MaxSize(String),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Unmatched(why) => write!(f, "your rules no longer match it: {why}"),
            Reason::Marker(file) => write!(f, "its folder has {file}"),
            Reason::MaxAge(age) => write!(f, "downloaded more than {age} ago"),
            Reason::MaxItems(n) => write!(f, "beyond max_items = {n}"),
            Reason::MaxSize(size) => write!(f, "over max_size = {size}"),
        }
    }
}

/// Which candidates go, and why, oldest first.
#[must_use]
pub fn plan(cleanup: &Cleanup, candidates: &[Candidate], now: Timestamp) -> Vec<(usize, Reason)> {
    // Oldest first, per `order`; ties: further down the source goes first.
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].present)
        .collect();
    let c = |i: usize| &candidates[i];
    match cleanup.order {
        CleanupOrder::OldestDownloaded => {
            order.sort_by_key(|&i| (c(i).downloaded_at, Reverse(c(i).position)));
        }
        CleanupOrder::OldestPublished => {
            order.sort_by_key(|&i| (c(i).published_at, Reverse(c(i).position)));
        }
        CleanupOrder::SourcePosition => order.sort_by_key(|&i| Reverse(c(i).position)),
    }
    let protected = order.len().saturating_sub(cleanup.keep_latest as usize);
    let (removable, _newest) = order.split_at(protected);

    let mut victims: Vec<(usize, Reason)> = Vec::new();
    let max_age = cleanup.max_age.0.and_then(|age| {
        let age = jiff::SignedDuration::try_from(age).ok()?;
        now.checked_sub(age).ok()
    });
    for &i in removable {
        let reason = if let (Some(why), Unmatched::Remove) = (&c(i).unmatched, cleanup.unmatched) {
            Some(Reason::Unmatched(why.clone()))
        } else if c(i).marked {
            Some(Reason::Marker(cleanup.marker_file.clone()))
        } else if let (Some(limit), Some(at)) = (max_age, c(i).downloaded_at)
            && at < limit
        {
            Some(Reason::MaxAge(crate::config::types::format_duration(
                cleanup.max_age.0.unwrap_or_default(),
            )))
        } else {
            None
        };
        if let Some(reason) = reason {
            victims.push((i, reason));
        }
    }

    // What's left must fit: drop from the oldest end.
    let gone = |i: usize, victims: &[(usize, Reason)]| victims.iter().any(|(v, _)| *v == i);
    let mut count = order.iter().filter(|&&i| !gone(i, &victims)).count();
    let mut size: u64 = order
        .iter()
        .filter(|&&i| !gone(i, &victims))
        .map(|&i| c(i).bytes)
        .sum();
    let max_size = cleanup.max_size.bytes;
    for &i in removable {
        if gone(i, &victims) {
            continue;
        }
        let reason = if cleanup.max_items > 0 && count > cleanup.max_items as usize {
            Reason::MaxItems(cleanup.max_items)
        } else if let Some(max) = max_size
            && size > max
        {
            Reason::MaxSize(cleanup.max_size.text.clone())
        } else {
            break;
        };
        count -= 1;
        size -= c(i).bytes;
        victims.push((i, reason));
    }
    victims
}

/// The files cleanup takes out of a video's folder: everything with `keep =
/// "nothing"`; the media, subtitles and raw info with `keep = "metadata"`.
/// Files a hook removed on purpose, or that are gone already, are skipped.
pub fn files_to_remove(files: &VideoFiles, keep: CleanupKeep) -> Vec<PathBuf> {
    let mut out: Vec<&str> = Vec::new();
    if !files.is_removed("media") {
        out.push(&files.media);
    }
    for sub in &files.subtitles {
        if !files.is_removed(&format!("subtitle:{}", sub.lang)) {
            out.push(&sub.path);
        }
    }
    if keep == CleanupKeep::Nothing {
        out.extend(files.raw_info.as_deref());
        out.extend(files.nfo.as_deref());
        if let Some(cover) = files
            .cover
            .as_deref()
            .filter(|_| !files.is_removed("cover"))
        {
            out.push(cover);
        }
        out.push(&files.metadata);
    }
    out.into_iter()
        .map(PathBuf::from)
        .filter(|p| p.symlink_metadata().is_ok())
        .collect()
}

/// Take `paths` out: into `trash` (a new folder for this video), or deleted
/// with `None`. All or nothing: a failed move puts back what was moved.
/// Returns `(where it was, where it is now)` for each file moved, and the
/// bytes freed; a folder left empty is removed.
///
/// # Errors
///
/// Fails if the trash folder can't be created, a file can't be moved into it
/// (after putting back what was already moved), or a file can't be deleted.
pub fn remove(
    paths: &[PathBuf],
    dir: &Path,
    trash: Option<&Path>,
) -> Result<(Vec<(String, String)>, u64), String> {
    let bytes: u64 = paths
        .iter()
        .filter_map(|p| p.symlink_metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|m| m.len())
        .sum();
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    match trash {
        Some(trash) => {
            std::fs::create_dir_all(trash)
                .map_err(|e| format!("cannot create {}: {e}", trash.display()))?;
            for (n, path) in paths.iter().enumerate() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                // Names can repeat across folders; keep each one apart.
                let dest = if moved.iter().any(|(_, d)| d.file_name() == path.file_name()) {
                    trash.join(format!("{n}-{name}"))
                } else {
                    trash.join(name.as_ref())
                };
                if let Err(e) = crate::download::place(path, &dest) {
                    for (from, to) in moved.iter().rev() {
                        let _ = crate::download::place(to, from);
                    }
                    let _ = std::fs::remove_dir(trash);
                    return Err(format!("cannot move {} to the trash: {e}", path.display()));
                }
                moved.push((path.clone(), dest));
            }
        }
        None => {
            for path in paths {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("cannot delete {}: {e}", path.display())),
                }
            }
        }
    }
    // Only if nothing else is in it (folders can be shared).
    let _ = std::fs::remove_dir(dir);
    let moved = moved
        .into_iter()
        .map(|(from, to)| (from.display().to_string(), to.display().to_string()))
        .collect();
    Ok((moved, bytes))
}

/// Put trashed files back where they were. Nothing is moved if any of those
/// places is taken.
///
/// # Errors
///
/// Fails if an original place is taken, a file is gone from the trash, or a
/// move fails (after moving back what was already restored).
pub fn restore(files: &[(String, String)]) -> Result<(), String> {
    for (from, to) in files {
        if Path::new(from).symlink_metadata().is_ok() {
            return Err(format!("{from} exists; move it away first"));
        }
        if Path::new(to).symlink_metadata().is_err() {
            return Err(format!("{to} is no longer in the trash"));
        }
    }
    let mut done: Vec<(&String, &String)> = Vec::new();
    for (from, to) in files {
        if let Err(e) = crate::download::place(Path::new(to), Path::new(from)) {
            for (f, t) in done.iter().rev() {
                let _ = crate::download::place(Path::new(f), Path::new(t));
            }
            return Err(format!("cannot move {to} back: {e}"));
        }
        done.push((from, to));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CleanupDelete, HumanDuration, OptionalDuration, OptionalSize};
    use std::time::Duration;

    fn cleanup() -> Cleanup {
        Cleanup {
            max_items: 0,
            max_age: OptionalDuration(None),
            max_size: OptionalSize::default(),
            order: CleanupOrder::OldestDownloaded,
            unmatched: Unmatched::Keep,
            keep: CleanupKeep::Metadata,
            marker_file: String::new(),
            keep_latest: 0,
            delete: CleanupDelete::Trash,
            trash_for: HumanDuration(Duration::from_secs(7 * 86_400)),
        }
    }

    fn now() -> Timestamp {
        "2026-11-01T12:00:00Z".parse().unwrap()
    }

    /// Video `n`: downloaded `n` days ago, published `10 - n` days ago,
    /// `n` MB, at position `n`.
    fn video(n: i64) -> Candidate {
        let day = |d: i64| now() - jiff::SignedDuration::from_hours(24 * d);
        Candidate {
            video_id: format!("v{n}"),
            position: n,
            bytes: n as u64 * 1_000_000,
            downloaded_at: Some(day(n)),
            published_at: Some(day(10 - n)),
            present: true,
            unmatched: None,
            marked: false,
        }
    }

    fn ids(candidates: &[Candidate], victims: &[(usize, Reason)]) -> Vec<String> {
        victims
            .iter()
            .map(|(i, _)| candidates[*i].video_id.clone())
            .collect()
    }

    #[test]
    fn nothing_set_removes_nothing() {
        let videos: Vec<Candidate> = (1..=5).map(video).collect();
        assert!(plan(&cleanup(), &videos, now()).is_empty());
    }

    #[test]
    fn max_items_removes_the_oldest_per_order() {
        let videos: Vec<Candidate> = (1..=5).map(video).collect();
        let mut c = cleanup();
        c.max_items = 3;
        let victims = plan(&c, &videos, now());
        assert_eq!(
            ids(&videos, &victims),
            ["v5", "v4"],
            "downloaded longest ago"
        );
        assert_eq!(victims[0].1, Reason::MaxItems(3));

        c.order = CleanupOrder::OldestPublished;
        assert_eq!(ids(&videos, &plan(&c, &videos, now())), ["v1", "v2"]);
        c.order = CleanupOrder::SourcePosition;
        assert_eq!(
            ids(&videos, &plan(&c, &videos, now())),
            ["v5", "v4"],
            "furthest down"
        );
    }

    #[test]
    fn max_size_and_keep_latest() {
        let videos: Vec<Candidate> = (1..=5).map(video).collect();
        let mut c = cleanup();
        // 15 MB in all.
        c.max_size = OptionalSize {
            text: "6MB".into(),
            bytes: Some(6_000_000),
        };
        assert_eq!(ids(&videos, &plan(&c, &videos, now())), ["v5", "v4"]);

        // The newest 4 are protected: only v5 may go, and does.
        c.keep_latest = 4;
        assert_eq!(ids(&videos, &plan(&c, &videos, now())), ["v5"]);
    }

    #[test]
    fn age_marker_and_rules_remove_on_their_own() {
        let mut videos: Vec<Candidate> = (1..=5).map(video).collect();
        videos[0].marked = true;
        videos[1].unmatched = Some("excluded by exclude[0]".into());
        let mut c = cleanup();
        c.max_age = OptionalDuration(Some(Duration::from_secs(3 * 86_400)));
        c.marker_file = ".watched".into();
        let victims = plan(&c, &videos, now());
        assert_eq!(ids(&videos, &victims), ["v5", "v4", "v1"], "oldest first");
        assert_eq!(victims[0].1.to_string(), "downloaded more than 3d ago");
        assert_eq!(victims[2].1.to_string(), "its folder has .watched");

        // `unmatched` only with "remove".
        c.unmatched = Unmatched::Remove;
        let victims = plan(&c, &videos, now());
        assert!(
            victims
                .iter()
                .any(|(i, r)| *i == 1 && *r == Reason::Unmatched("excluded by exclude[0]".into()))
        );
    }

    #[test]
    fn missing_media_doesnt_count() {
        let mut videos: Vec<Candidate> = (1..=5).map(video).collect();
        videos[4].present = false;
        let mut c = cleanup();
        c.max_items = 3;
        assert_eq!(ids(&videos, &plan(&c, &videos, now())), ["v4"]);
    }

    fn files(dir: &Path) -> VideoFiles {
        let file = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, name).unwrap();
            path.display().to_string()
        };
        VideoFiles {
            dir: dir.display().to_string(),
            media: file("v.mkv"),
            cover: Some(file("v-poster.jpg")),
            subtitles: vec![crate::state::items::Subtitle {
                lang: "en".into(),
                path: file("v.en.vtt"),
            }],
            metadata: file("metadata.json"),
            nfo: Some(file("v.nfo")),
            raw_info: None,
            removed: Vec::new(),
        }
    }

    #[test]
    fn keeps_metadata_or_nothing_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("video");
        std::fs::create_dir_all(&dir).unwrap();
        let files = files(&dir);
        std::fs::write(dir.join("someone-else.mkv"), "not ours").unwrap();

        let paths = files_to_remove(&files, CleanupKeep::Metadata);
        assert_eq!(paths.len(), 2, "media and subtitles");
        let trash = tmp.path().join("trash/x");
        let (moved, bytes) = remove(&paths, &dir, Some(&trash)).unwrap();
        assert_eq!(bytes, ("v.mkv".len() + "v.en.vtt".len()) as u64);
        assert!(!dir.join("v.mkv").exists() && trash.join("v.mkv").exists());
        assert!(dir.join("metadata.json").exists() && dir.join("v-poster.jpg").exists());
        assert!(dir.join("v.nfo").exists(), "the .nfo is metadata too");

        restore(&moved).unwrap();
        assert!(dir.join("v.mkv").exists() && dir.join("v.en.vtt").exists());

        // Everything ours goes; the folder stays for the file that isn't.
        let paths = files_to_remove(&files, CleanupKeep::Nothing);
        assert_eq!(paths.len(), 5);
        remove(&paths, &dir, None).unwrap();
        assert!(dir.join("someone-else.mkv").exists());
        std::fs::remove_file(dir.join("someone-else.mkv")).unwrap();
        let files = self::files(&dir);
        remove(&files_to_remove(&files, CleanupKeep::Nothing), &dir, None).unwrap();
        assert!(!dir.exists(), "an emptied folder goes");
    }

    #[test]
    fn restore_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("video");
        std::fs::create_dir_all(&dir).unwrap();
        let files = files(&dir);
        let trash = tmp.path().join("trash/x");
        let (moved, _) = remove(
            &files_to_remove(&files, CleanupKeep::Metadata),
            &dir,
            Some(&trash),
        )
        .unwrap();
        std::fs::write(dir.join("v.mkv"), "new").unwrap();
        let e = restore(&moved).unwrap_err();
        assert!(e.contains("exists"), "{e}");
        assert!(trash.join("v.en.vtt").exists(), "nothing moved");
    }
}
