//! Downloaded videos whose files are gone. Each run checks every recorded
//! file (media, cover, subtitles, metadata), skipping the ones a `finished`
//! hook removed on purpose. With `redownload_missing`, it restores only what's
//! missing: the media by downloading the video again, the cover by fetching
//! it, subtitles with yt-dlp (without the video), the metadata file from
//! tidepool's copy. Otherwise the run reports them.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::thumbnail::{self, CoverFormat};
use super::{Context, SourcePaths, metadata, place};
use crate::config::merge::EffectiveSource;
use crate::state::State;
use crate::state::items::{self, VideoFiles};

/// What the check found and did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Videos with missing files, left alone (`redownload_missing` is off).
    pub reported: usize,
    /// Videos whose media is gone, queued to download again.
    pub requeued: usize,
    /// Files put back without downloading the video.
    pub restored: usize,
    /// Files that couldn't be put back (retried on the next run).
    pub failed: usize,
}

/// What's missing of a video: `media`, `cover`, `subtitle:<lang>`, `metadata`.
#[must_use]
pub fn missing(files: &VideoFiles) -> Vec<String> {
    let gone = |path: &str| !Path::new(path).exists() && !Path::new(path).is_symlink();
    let mut out = Vec::new();
    if !files.is_removed("media") && gone(&files.media) {
        out.push("media".to_string());
    }
    if let Some(cover) = &files.cover
        && !files.is_removed("cover")
        && gone(cover)
    {
        out.push("cover".into());
    }
    for sub in &files.subtitles {
        let what = format!("subtitle:{}", sub.lang);
        if !files.is_removed(&what) && gone(&sub.path) {
            out.push(what);
        }
    }
    if gone(&files.metadata) {
        out.push("metadata".into());
    }
    out
}

/// Check the downloaded videos of `sources`; restore or report. `paths`
/// has every source's paths.
///
/// # Errors
///
/// Returns a message when the state database can't be read or updated, or
/// writing to `out` fails. Files that can't be restored are counted in
/// [`Report::failed`] instead.
pub async fn check(
    state: &State,
    sources: &[EffectiveSource],
    paths: &BTreeMap<String, SourcePaths>,
    ctx: &Context,
    out: &mut dyn Write,
) -> Result<Report, String> {
    let db = |e: rusqlite::Error| format!("state database: {e}");
    let keys: Vec<String> = sources.iter().map(|s| s.key.clone()).collect();
    let mut report = Report::default();
    for item in items::downloaded(&state.conn, &keys).map_err(db)? {
        let missing = missing(&item.files);
        items::set_missing(&state.conn, &item.source_key, &item.video_id, &missing).map_err(db)?;
        if missing.is_empty() {
            continue;
        }
        let Some(source) = sources.iter().find(|s| s.key == item.source_key) else {
            continue;
        };
        let label = format!("{}: {}", item.source_key, item.video_id);
        tracing::debug!("{label}: missing {}", missing.join(", "));
        if !source.redownload_missing {
            report.reported += 1;
            continue;
        }
        // No media: download the video again, which replaces everything.
        if missing.iter().any(|m| m == "media") {
            items::requeue(&state.conn, &item.source_key, &item.video_id).map_err(db)?;
            tracing::info!("{label}: the video file is gone; downloading it again");
            report.requeued += 1;
            continue;
        }
        let tmp = paths[&source.key]
            .tmp_root
            .join(format!("{}.{}", item.video_id, source.key));
        for what in &missing {
            let result = match what.as_str() {
                "cover" => restore_cover(state, ctx, &item.video_id, &item.files, &tmp).await,
                "metadata" => {
                    restore_metadata(state, &item.source_key, &item.video_id, &item.files)
                }
                sub => {
                    let lang = sub.trim_start_matches("subtitle:");
                    restore_subtitle(
                        ctx,
                        source,
                        &paths[&source.key],
                        &item.video_id,
                        lang,
                        &item.files,
                        &tmp,
                    )
                    .await
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!("{label}: restored the {}", describe(what));
                    report.restored += 1;
                }
                Err(e) => {
                    tracing::warn!("{label}: cannot restore the {}: {e}", describe(what));
                    report.failed += 1;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let still = self::missing(&item.files);
        items::set_missing(&state.conn, &item.source_key, &item.video_id, &still).map_err(db)?;
    }

    if report.reported > 0 {
        let videos = if report.reported == 1 {
            "video has"
        } else {
            "videos have"
        };
        writeln!(
            out,
            "{} {videos} missing files; set redownload_missing = true to restore them",
            report.reported
        )
        .map_err(|e| e.to_string())?;
    }
    if report.restored > 0 {
        let files = if report.restored == 1 {
            "file"
        } else {
            "files"
        };
        writeln!(out, "restored {} missing {files}", report.restored).map_err(|e| e.to_string())?;
    }
    Ok(report)
}

fn describe(what: &str) -> String {
    match what {
        "cover" => "poster".into(),
        "metadata" => "metadata file".into(),
        sub => format!("{} subtitles", sub.trim_start_matches("subtitle:")),
    }
}

/// Fetch the biggest cover again, in the format its path asks for.
async fn restore_cover(
    state: &State,
    ctx: &Context,
    video_id: &str,
    files: &VideoFiles,
    tmp: &Path,
) -> Result<(), String> {
    let dest = PathBuf::from(files.cover.as_deref().expect("a cover to restore"));
    let format = dest
        .extension()
        .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))
        .unwrap_or(CoverFormat::Jpg);
    let thumbs = items::thumbnails(&state.conn, video_id).map_err(|e| e.to_string())?;
    let urls = thumbnail::candidates(video_id, &thumbs, &ctx.thumbnail_base, format);
    let found = thumbnail::fetch_largest(&ctx.http, &urls, format)
        .await
        .ok_or("no cover image found")?;
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let file = tmp.join(format!("cover.{}", format.extension()));
    std::fs::write(&file, &found.bytes).map_err(|e| e.to_string())?;
    place(&file, &dest).map_err(|e| e.to_string())
}

/// Fetch one language's subtitles with yt-dlp, without the video.
async fn restore_subtitle(
    ctx: &Context,
    source: &EffectiveSource,
    paths: &SourcePaths,
    video_id: &str,
    lang: &str,
    files: &VideoFiles,
    tmp: &Path,
) -> Result<(), String> {
    let dest = files
        .subtitles
        .iter()
        .find(|s| s.lang == lang)
        .map(|s| PathBuf::from(&s.path))
        .ok_or("no such subtitle")?;
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let mut args: Vec<String> = source.ytdlp_args.clone();
    args.extend([
        "--skip-download".into(),
        "--write-subs".into(),
        "--sub-langs".into(),
        lang.into(),
        "--no-playlist".into(),
        "--no-update".into(),
        "-o".into(),
        format!("{}/media.%(ext)s", tmp.display()),
    ]);
    if let Some(cookies) = &paths.cookies {
        args.extend(["--cookies".into(), cookies.display().to_string()]);
    }
    args.extend([
        "--".into(),
        format!("https://www.youtube.com/watch?v={video_id}"),
    ]);
    let output = ctx
        .ytdlp
        .command()
        .args(&args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("cannot run {}: {e}", ctx.ytdlp.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "yt-dlp failed: {}",
            stderr.lines().last().unwrap_or("").trim()
        ));
    }
    let prefix = format!("media.{lang}.");
    let found = std::fs::read_dir(tmp)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .ok_or_else(|| format!("yt-dlp found no {lang} subtitles"))?;
    place(&found, &dest).map_err(|e| e.to_string())
}

/// Write the metadata file again from tidepool's copy, with the files where
/// they are now.
fn restore_metadata(
    state: &State,
    source_key: &str,
    video_id: &str,
    files: &VideoFiles,
) -> Result<(), String> {
    let copy = items::metadata_copy(&state.conn, source_key, video_id)
        .map_err(|e| e.to_string())?
        .ok_or("tidepool has no copy of it (downloaded before copies were kept)")?;
    let mut meta: serde_json::Value = serde_json::from_str(&copy).map_err(|e| e.to_string())?;
    meta["files"] = metadata::files_json(files);
    let dest = Path::new(&files.metadata);
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let name = dest
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned());
    let tmp = dest.with_file_name(format!(".{name}.tidepool-partial"));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&meta).expect("JSON") + "\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::items::Subtitle;

    #[test]
    fn finds_missing_files_but_not_removed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, "x").unwrap();
            path.display().to_string()
        };
        let gone = |name: &str| dir.path().join(name).display().to_string();
        let mut files = VideoFiles {
            dir: dir.path().display().to_string(),
            media: file("v.mkv"),
            cover: Some(gone("v-poster.jpg")),
            subtitles: vec![
                Subtitle {
                    lang: "en".into(),
                    path: file("v.en.vtt"),
                },
                Subtitle {
                    lang: "pt".into(),
                    path: gone("v.pt.vtt"),
                },
            ],
            metadata: gone("metadata.json"),
            ..VideoFiles::default()
        };
        assert_eq!(missing(&files), ["cover", "subtitle:pt", "metadata"]);
        files.removed = vec!["cover".into(), "subtitle:pt".into()];
        assert_eq!(missing(&files), ["metadata"]);
        std::fs::remove_file(&files.media).unwrap();
        assert_eq!(missing(&files), ["media", "metadata"]);
        files.removed.push("media".into());
        assert_eq!(missing(&files), ["metadata"]);
    }
}
