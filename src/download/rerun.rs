//! `tidepool hooks run` and `tidepool embed`: run hooks (or embedding)
//! again on videos already downloaded.
//!
//! `downloaded` and `embed` hooks work on copies of the placed files in the
//! work area (then the files are embedded again and moved back, the same
//! way a download places them); `finished` hooks run on the placed files.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::info::VideoInfo;
use super::layout::{Layout, Values};
use super::stages::Stage;
use super::{Placement, extension_of, metadata, place_all, relink};
use crate::config::merge::EffectiveSource;
use crate::config::{Duplicates, HookConfig, HookType};
use crate::hooks;
use crate::paths::Dirs;
use crate::state::items::{Downloaded, Subtitle, VideoFiles};

/// What a re-run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `hooks run`: the chosen hooks. The files are reprocessed when
    /// `downloaded` or `embed` hooks are among them.
    Hooks,
    /// `tidepool embed`: the `embed` hooks, then embedding with the current
    /// settings; with `remove_disabled`, what's switched off is taken out.
    /// No `finished` hooks.
    Embed { remove_disabled: bool },
}

/// What every re-run shares.
pub struct Shared<'a> {
    pub mode: Mode,
    pub hooks: &'a BTreeMap<String, HookConfig>,
    pub hook_logs: &'a Path,
    pub dirs: &'a Dirs,
    pub ffmpeg: &'a Path,
    pub ffprobe: &'a Path,
    pub duplicates: Duplicates,
    pub shutdown: &'a crate::shutdown::Shutdown,
    pub grace: std::time::Duration,
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
    /// What embedding did, when the files were reprocessed.
    pub embedding: Option<super::stages::Embedding>,
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
    let position =
        u32::try_from(meta["source"]["position"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
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
        shutdown: shared.shutdown,
        grace: shared.grace,
        ffmpeg: shared.ffmpeg,
        ffprobe: shared.ffprobe,
        data_dir: &shared.dirs.data_dir,
    };
    let mut hook_runs = Vec::new();
    let mut result = Rerun {
        title: info.title.clone(),
        ..Rerun::default()
    };

    let embed_mode = matches!(shared.mode, Mode::Embed { .. });
    if embed_mode || stage.has(HookType::Downloaded) || stage.has(HookType::Embed) {
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
            Ok((files, bytes, embedding)) => {
                result.files = Some((files, bytes));
                result.embedding = Some(embedding);
            }
            Err(e) => {
                // Like a failed download: nothing changed, no finished hooks.
                result.hook_runs = hook_runs;
                result.error = Some(e);
                return result;
            }
        }
    }

    if embed_mode {
        result.hook_runs = hook_runs;
        return result;
    }
    let (placed, bytes) = match result.files.take() {
        Some(new) => new,
        None => (files.clone(), media_size(files)),
    };
    let context = stage.context(&info);
    stage
        .finished_hooks(&values, &placed, &context, &mut hook_runs)
        .await;
    if placed != *files {
        result.files = Some((placed, bytes));
    }
    result.hook_runs = hook_runs;
    result
}

/// The media's size now (0 when it's gone).
fn media_size(files: &VideoFiles) -> u64 {
    std::fs::metadata(&files.media).map_or(0, |m| m.len())
}

fn read_metadata(video: &Video<'_>) -> Result<(serde_json::Value, VideoInfo), String> {
    load_metadata(&video.item.files, &video.item.video_id)
}

/// A downloaded video's metadata.json, and the details in it.
///
/// # Errors
///
/// Returns a message when the file can't be read, isn't valid JSON, or its
/// details don't have the expected types.
pub fn load_metadata(
    files: &VideoFiles,
    video_id: &str,
) -> Result<(serde_json::Value, VideoInfo), String> {
    let path = Path::new(&files.metadata);
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read {} ({e}); `tidepool reset {} --delete-files` downloads it again",
            path.display(),
            video_id
        )
    })?;
    let meta: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} isn't valid JSON: {e}", path.display()))?;
    let info = VideoInfo::from_metadata(&meta)?;
    Ok((meta, info))
}

/// Copy the placed files to the work area, run `downloaded` and `embed`
/// hooks, embed again and move everything back. Returns the new locations
/// and what embedding did.
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
) -> Result<(VideoFiles, u64, super::stages::Embedding), String> {
    let files = &video.item.files;
    let layout = Layout::new(video.source, values, video.media_dir, shared.dirs)?;
    let (cover_dest, cover_format) = layout.cover_file()?;
    let mut raw = copy_to_work(files, tmp)?;
    let context = stage.context(info);
    stage
        .downloaded_hooks(&mut raw, values, &context, tmp, cover_format, hook_runs)
        .await
        .map_err(|f| f.message)?;

    let mut data = hooks::EmbedData {
        tags: stage
            .embed_tags(info, values)
            .await
            .map_err(|f| f.message)?,
        cover: raw.cover.clone(),
        chapters: info.chapters.clone().unwrap_or_default(),
        subtitles: raw.subtitles.clone(),
    };
    let musicbrainz = stage
        .musicbrainz(&mut data.tags, info, meta.get("musicbrainz"))
        .await;
    stage
        .embed_hooks(&mut data, values, &context, tmp, &raw, hook_runs)
        .await
        .map_err(|f| f.message)?;
    let remove_disabled = matches!(
        shared.mode,
        Mode::Embed {
            remove_disabled: true
        }
    );
    let embedding = stage
        .embed(&raw.media, &data, None, shared.duplicates, remove_disabled)
        .await;

    let old_media = PathBuf::from(&files.media);
    let Placement {
        media_dest,
        mut moves,
        files: new,
    } = moves_back(files, &raw, &layout, &cover_dest)?;
    let linked = hardlinked(video.others, &old_media);

    let media_bytes = std::fs::metadata(&raw.media)
        .map_err(|e| format!("cannot read {}: {e}", raw.media.display()))?
        .len();
    let meta = updated_metadata(meta, &new, media_bytes, &embedding, &data, musicbrainz);
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
    Ok((new, media_bytes, embedding))
}

/// Copies of the placed files that still exist in the work area `tmp`, named
/// the way a download leaves them.
fn copy_to_work(files: &VideoFiles, tmp: &Path) -> Result<hooks::Files, String> {
    std::fs::create_dir_all(tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let copy = |from: &str, name: String| -> Result<PathBuf, String> {
        let to = tmp.join(name);
        // A copy, never a link: hooks may change it in place.
        std::fs::copy(from, &to).map_err(|e| format!("cannot copy {from}: {e}"))?;
        Ok(to)
    };
    Ok(hooks::Files {
        media: copy(
            &files.media,
            format!("media.{}", extension_of(&files.media)),
        )?,
        cover: match &files.cover {
            Some(cover) if Path::new(cover).is_file() => {
                Some(copy(cover, format!("cover.{}", extension_of(cover)))?)
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
                    path: copy(
                        &s.path,
                        format!("media.{}.{}", s.lang, extension_of(&s.path)),
                    )?,
                })
            })
            .collect::<Result<_, String>>()?,
    })
}

/// Where the reprocessed files go: back where they were, except that a new
/// media type gets the new extension (the metadata file isn't written yet).
fn moves_back(
    files: &VideoFiles,
    raw: &hooks::Files,
    layout: &Layout,
    cover_dest: &Path,
) -> Result<Placement, String> {
    let old_media = PathBuf::from(&files.media);
    let new_ext = raw
        .media
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let media_dest = if new_ext == extension_of(&files.media) {
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
            .map_or_else(|| cover_dest.to_path_buf(), PathBuf::from);
        moves.push((cover.clone(), dest.clone()));
        new.cover = Some(dest.display().to_string());
    }
    for sub in &raw.subtitles {
        let dest = match files.subtitles.iter().find(|s| s.lang == sub.lang) {
            Some(existing) => PathBuf::from(&existing.path),
            None => {
                layout.subtitle_file(&sub.lang, &extension_of(&sub.path.display().to_string()))?
            }
        };
        moves.push((sub.path.clone(), dest.clone()));
        new.subtitles.push(Subtitle {
            lang: sub.lang.clone(),
            path: dest.display().to_string(),
        });
    }
    Ok(Placement {
        media_dest,
        moves,
        files: new,
    })
}

/// Other sources' copies hardlinked to `media`: they get the new file too.
fn hardlinked<'a>(others: &'a [VideoFiles], media: &Path) -> Vec<&'a VideoFiles> {
    let inode = std::fs::metadata(media).ok().map(|m| (m.dev(), m.ino()));
    others
        .iter()
        .filter(|o| {
            inode.is_some() && std::fs::metadata(&o.media).ok().map(|m| (m.dev(), m.ino())) == inode
        })
        .collect()
}

/// `meta` with the new files, size, embedding and MusicBrainz match.
fn updated_metadata(
    meta: &serde_json::Value,
    new: &VideoFiles,
    media_bytes: u64,
    embedding: &super::stages::Embedding,
    data: &hooks::EmbedData,
    musicbrainz: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut meta = meta.clone();
    meta["files"] = metadata::files_json(new);
    meta["media"]["size_bytes"] = media_bytes.into();
    meta["embedded"] = serde_json::to_value(&embedding.embedded).expect("JSON");
    meta["embedded_tags"] = if embedding.embedded.metadata {
        serde_json::to_value(&data.tags).expect("JSON")
    } else {
        serde_json::Value::Null
    };
    meta["embed_error"] = embedding.error.clone().into();
    if let Some(musicbrainz) = musicbrainz {
        meta["musicbrainz"] = musicbrainz;
    }
    meta
}
