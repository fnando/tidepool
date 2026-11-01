//! `tidepool hooks run`: run hooks again on videos already downloaded.
//!
//! `downloaded` and `embed` hooks work on copies of the placed files in the
//! work area (then the files are embedded again and moved back, the same
//! way a download places them); `finished` hooks run on the placed files.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use super::info::VideoInfo;
use super::layout::{Layout, Values};
use super::stages::Stage;
use super::{metadata, place_all, relink};
use crate::config::merge::EffectiveSource;
use crate::config::{Duplicates, HookConfig, HookType};
use crate::hooks;
use crate::paths::Dirs;
use crate::state::items::{Downloaded, Subtitle, VideoFiles};

/// What every re-run shares.
pub struct Shared<'a> {
    pub hooks: &'a BTreeMap<String, HookConfig>,
    pub hook_logs: &'a Path,
    pub dirs: &'a Dirs,
    pub ffmpeg: &'a Path,
    pub ffprobe: &'a Path,
    pub duplicates: Duplicates,
    pub cancel: &'a CancellationToken,
}

/// One video to run hooks for.
pub struct Video<'a> {
    pub item: &'a Downloaded,
    pub source: &'a EffectiveSource,
    /// The hooks to run, in the source's order.
    pub hook_names: &'a [String],
    pub media_dir: &'a Path,
    pub tmp_root: &'a Path,
    /// Other sources' copies of this video (to keep hardlinks in step).
    pub others: &'a [VideoFiles],
}

/// What a re-run did.
#[derive(Debug, Default)]
pub struct Rerun {
    pub title: String,
    /// New locations (and media size), when the files were reprocessed.
    pub files: Option<(VideoFiles, u64)>,
    /// Every hook that ran, including before a failure.
    pub hook_runs: Vec<hooks::Report>,
    /// Why the files couldn't be reprocessed (they're left as they were).
    pub error: Option<String>,
}

pub async fn rerun(shared: &Shared<'_>, video: &Video<'_>) -> Rerun {
    let (meta, info) = match read_metadata(video) {
        Ok(found) => found,
        Err(e) => {
            return Rerun {
                title: video.item.video_id.clone(),
                error: Some(e),
                ..Rerun::default()
            };
        }
    };
    let files = &video.item.files;
    let source_title = meta["source"]["title"].as_str();
    let position = meta["source"]["position"].as_u64().unwrap_or(0) as u32;
    let source = video.source;
    let label = format!("{}: “{}”", source.key, info.title);
    let values = Values::new(&info, source, source_title, position);
    let stage = Stage {
        source,
        hook_names: video.hook_names,
        hooks: shared.hooks,
        video_id: &video.item.video_id,
        source_title,
        position,
        label: &label,
        home: &shared.dirs.home,
        hook_logs: shared.hook_logs,
        cancel: shared.cancel,
        ffmpeg: shared.ffmpeg,
        ffprobe: shared.ffprobe,
    };
    let mut hook_runs = Vec::new();
    let mut result = Rerun {
        title: info.title.clone(),
        ..Rerun::default()
    };

    if stage.has(HookType::Downloaded) || stage.has(HookType::Embed) {
        let tmp = video
            .tmp_root
            .join(format!("{}.{}", video.item.video_id, source.key));
        let _ = std::fs::remove_dir_all(&tmp);
        let reprocessed = reprocess(
            shared,
            video,
            &stage,
            &info,
            &values,
            &meta,
            &tmp,
            &mut hook_runs,
        )
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
        match reprocessed {
            Ok(new) => result.files = Some(new),
            Err(e) => {
                // Like a failed download: nothing changed, no finished hooks.
                result.hook_runs = hook_runs;
                result.error = Some(e);
                return result;
            }
        }
    }

    let (mut placed, bytes) = match result.files.take() {
        Some(new) => new,
        None => (files.clone(), media_size(files)),
    };
    let context = stage.context(&info);
    let moved = stage
        .finished_hooks(&values, &mut placed, &context, &mut hook_runs)
        .await;
    if moved && let Err(e) = metadata::rewrite_files(&placed) {
        tracing::warn!("{label}: cannot update {}: {e}", placed.metadata);
    }
    if moved || placed != *files {
        result.files = Some((placed, bytes));
    }
    result.hook_runs = hook_runs;
    result
}

/// The media's size now (0 when it's gone).
fn media_size(files: &VideoFiles) -> u64 {
    std::fs::metadata(&files.media)
        .map(|m| m.len())
        .unwrap_or(0)
}

fn read_metadata(video: &Video<'_>) -> Result<(serde_json::Value, VideoInfo), String> {
    let path = Path::new(&video.item.files.metadata);
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read {} ({e}); `tidepool reset {} --delete-files` downloads it again",
            path.display(),
            video.item.video_id
        )
    })?;
    let meta: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} isn't valid JSON: {e}", path.display()))?;
    let info = VideoInfo::from_metadata(&meta)?;
    Ok((meta, info))
}

/// Copy the placed files to the work area, run `downloaded` and `embed`
/// hooks, embed again and move everything back. Returns the new locations.
#[allow(clippy::too_many_arguments)]
async fn reprocess(
    shared: &Shared<'_>,
    video: &Video<'_>,
    stage: &Stage<'_>,
    info: &VideoInfo,
    values: &Values,
    meta: &serde_json::Value,
    tmp: &Path,
    hook_runs: &mut Vec<hooks::Report>,
) -> Result<(VideoFiles, u64), String> {
    let files = &video.item.files;
    let source = video.source;
    std::fs::create_dir_all(tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let copy = |from: &str, name: String| -> Result<PathBuf, String> {
        let to = tmp.join(name);
        // A copy, never a link: hooks may change it in place.
        std::fs::copy(from, &to).map_err(|e| format!("cannot copy {from}: {e}"))?;
        Ok(to)
    };
    let ext = |path: &str| {
        Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let layout = Layout::new(source, values, video.media_dir, shared.dirs)?;
    let (cover_dest, cover_format) = layout.cover_file()?;
    let mut raw = hooks::Files {
        media: copy(&files.media, format!("media.{}", ext(&files.media)))?,
        cover: match &files.cover {
            Some(cover) if Path::new(cover).is_file() => {
                Some(copy(cover, format!("cover.{}", ext(cover)))?)
            }
            _ => None,
        },
        subtitles: files
            .subtitles
            .iter()
            .filter(|s| Path::new(&s.path).is_file())
            .map(|s| {
                Ok(hooks::Subtitle {
                    lang: s.lang.clone(),
                    path: copy(&s.path, format!("media.{}.{}", s.lang, ext(&s.path)))?,
                })
            })
            .collect::<Result<_, String>>()?,
    };
    let context = stage.context(info);
    stage
        .downloaded_hooks(&mut raw, values, &context, tmp, cover_format, hook_runs)
        .await
        .map_err(|f| f.message)?;

    let mut data = hooks::EmbedData {
        tags: stage.tags(info),
        cover: raw.cover.clone(),
        chapters: info.chapters.clone().unwrap_or_default(),
        subtitles: raw.subtitles.clone(),
    };
    stage
        .embed_hooks(&mut data, values, &context, tmp, &raw, hook_runs)
        .await
        .map_err(|f| f.message)?;
    let embedding = stage
        .embed(&raw.media, &data, None, shared.duplicates)
        .await;

    // Back where they were; a new media type gets the new extension.
    let old_media = PathBuf::from(&files.media);
    let new_ext = raw
        .media
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let media_dest = if new_ext == ext(&files.media) {
        old_media.clone()
    } else {
        old_media.with_extension(&new_ext)
    };
    let mut new = VideoFiles {
        media: media_dest.display().to_string(),
        cover: None,
        subtitles: Vec::new(),
        ..files.clone()
    };
    let mut moves = vec![(raw.media.clone(), media_dest.clone())];
    if let Some(cover) = &raw.cover {
        let dest = files
            .cover
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cover_dest.clone());
        moves.push((cover.clone(), dest.clone()));
        new.cover = Some(dest.display().to_string());
    }
    for sub in &raw.subtitles {
        let dest = match files.subtitles.iter().find(|s| s.lang == sub.lang) {
            Some(existing) => PathBuf::from(&existing.path),
            None => layout.subtitle_file(&sub.lang, &ext(&sub.path.display().to_string()))?,
        };
        moves.push((sub.path.clone(), dest.clone()));
        new.subtitles.push(Subtitle {
            lang: sub.lang.clone(),
            path: dest.display().to_string(),
        });
    }

    // Other sources hardlinked to the old media get the new file too.
    let old_inode = std::fs::metadata(&old_media)
        .ok()
        .map(|m| (m.dev(), m.ino()));
    let linked: Vec<&VideoFiles> = video
        .others
        .iter()
        .filter(|o| {
            old_inode.is_some()
                && std::fs::metadata(&o.media).ok().map(|m| (m.dev(), m.ino())) == old_inode
        })
        .collect();

    let media_bytes = std::fs::metadata(&raw.media)
        .map_err(|e| format!("cannot read {}: {e}", raw.media.display()))?
        .len();
    let mut meta = meta.clone();
    meta["files"] = metadata::files_json(&new);
    meta["media"]["size_bytes"] = media_bytes.into();
    meta["embedded"] = serde_json::to_value(&embedding.embedded).expect("JSON");
    meta["embedded_tags"] = if embedding.embedded.metadata {
        serde_json::to_value(&data.tags).expect("JSON")
    } else {
        serde_json::Value::Null
    };
    meta["embed_error"] = embedding.error.clone().into();
    let meta_tmp = tmp.join("metadata.json");
    std::fs::write(
        &meta_tmp,
        serde_json::to_string_pretty(&meta).expect("JSON") + "\n",
    )
    .map_err(|e| format!("cannot write {}: {e}", meta_tmp.display()))?;
    moves.push((meta_tmp, PathBuf::from(&files.metadata)));

    place_all(&moves).map_err(|f| f.message)?;
    if media_dest != old_media {
        let _ = std::fs::remove_file(&old_media);
    }
    for other in linked.iter().filter(|o| Path::new(&o.media) != old_media) {
        if let Err(e) = relink(&media_dest, Path::new(&other.media)) {
            tracing::warn!(
                "{}: cannot update the linked copy at {}: {e}",
                stage.label,
                other.media
            );
        }
    }
    Ok((new, media_bytes))
}
