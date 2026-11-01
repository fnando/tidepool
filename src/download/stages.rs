//! The stages after yt-dlp, shared by downloads and `tidepool hooks run`:
//! `downloaded` hooks, `embed` hooks, embedding, `finished` hooks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use super::embed;
use super::info::VideoInfo;
use super::layout::Values;
use super::thumbnail::CoverFormat;
use super::ytdlp::Failure;
use super::{fail, place};
use crate::config::merge::EffectiveSource;
use crate::config::{Duplicates, HookConfig, HookType, OnFailure};
use crate::hooks;
use crate::state::items::VideoFiles;

/// One video going through the stages.
pub struct Stage<'a> {
    pub source: &'a EffectiveSource,
    /// The hooks to run, in order: normally the source's `hooks`.
    pub hook_names: &'a [String],
    pub hooks: &'a BTreeMap<String, HookConfig>,
    pub video_id: &'a str,
    pub source_title: Option<&'a str>,
    pub position: u32,
    pub label: &'a str,
    pub home: &'a Path,
    /// Where hook logs go (`<video_id>.log`).
    pub hook_logs: &'a Path,
    pub cancel: &'a CancellationToken,
    pub ffmpeg: &'a Path,
    pub ffprobe: &'a Path,
}

impl Stage<'_> {
    fn of_type(&self, kind: HookType) -> impl Iterator<Item = (&str, &HookConfig)> {
        hooks::of_type(self.hook_names, self.hooks, kind)
    }

    pub fn has(&self, kind: HookType) -> bool {
        self.of_type(kind).next().is_some()
    }

    async fn run_hook(
        &self,
        name: &str,
        hook: &HookConfig,
        values: &BTreeMap<String, String>,
        work_dir: &Path,
        input: &[u8],
    ) -> hooks::Report {
        tracing::debug!(
            "{}: running hook {name} ({})",
            self.source.key,
            hook.kind.as_str()
        );
        hooks::run(&hooks::Run {
            name,
            hook,
            values,
            default_work_dir: work_dir,
            home: self.home,
            input,
            log: &self.hook_logs.join(format!("{}.log", self.video_id)),
            cancel: self.cancel,
        })
        .await
    }

    /// What hooks get as read-only `context`.
    pub fn context(&self, info: &VideoInfo) -> serde_json::Value {
        serde_json::json!({
            "video": {
                "id": info.id,
                "url": format!("https://www.youtube.com/watch?v={}", info.id),
                "title": info.title,
                "description": info.description,
                "channel": { "id": info.channel_id, "name": info.channel, "url": info.channel_url },
                "upload_date": info.upload_date_iso(),
                "duration_seconds": info.duration_seconds(),
                "tags": info.tags.clone().unwrap_or_default(),
                "categories": info.categories.clone().unwrap_or_default(),
            },
            "source": {
                "name": self.source.key,
                "url": self.source.url,
                "id": self.source.id,
                "title": self.source_title,
                "position": self.position,
                "category": self.source.category,
            },
        })
    }

    /// `downloaded` hooks, on the raw files in the work area `tmp`; they may
    /// replace them.
    pub async fn downloaded_hooks(
        &self,
        raw: &mut hooks::Files,
        values: &Values,
        context: &serde_json::Value,
        tmp: &Path,
        cover_format: CoverFormat,
        hook_runs: &mut Vec<hooks::Report>,
    ) -> Result<(), Failure> {
        for (name, hook) in self.of_type(HookType::Downloaded) {
            let input = serde_json::to_vec(&raw.to_json(context)).expect("JSON");
            let hook_values =
                stage_values(values, tmp, Some(&raw.media), raw.cover.as_deref(), None);
            let report = self.run_hook(name, hook, &hook_values, tmp, &input).await;
            let mut changed = raw.clone();
            let applied = match &report.result {
                Ok(out) => changed
                    .apply(name, out)
                    .and_then(|()| check_in_work_area(name, tmp, &changed))
                    .and_then(|()| check_cover(name, &changed, cover_format)),
                Err(e) => Err(format!("hook {name} failed: {e}")),
            };
            if hook_failure(self.label, hook, report, applied, hook_runs)? {
                if changed.media != raw.media {
                    let _ = std::fs::remove_file(&raw.media);
                    tracing::debug!(
                        "{}: hook {name} replaced the media with {}",
                        self.label,
                        changed.media.display()
                    );
                }
                *raw = changed;
            }
        }
        Ok(())
    }

    /// `embed` hooks: process what gets embedded (skipped when nothing is).
    pub async fn embed_hooks(
        &self,
        data: &mut hooks::EmbedData,
        values: &Values,
        context: &serde_json::Value,
        tmp: &Path,
        raw: &hooks::Files,
        hook_runs: &mut Vec<hooks::Report>,
    ) -> Result<(), Failure> {
        for (name, hook) in self.of_type(HookType::Embed) {
            if !embedding_on(self.source) {
                tracing::debug!(
                    "{}: skipping hook {name}: nothing is embedded (see embed)",
                    self.label
                );
                continue;
            }
            let input = serde_json::to_vec(&data.to_json(context)).expect("JSON");
            let hook_values =
                stage_values(values, tmp, Some(&raw.media), raw.cover.as_deref(), None);
            let report = self.run_hook(name, hook, &hook_values, tmp, &input).await;
            let mut processed = data.clone();
            let applied = match &report.result {
                Ok(out) => processed.apply(name, out),
                Err(e) => Err(format!("hook {name} failed: {e}")),
            };
            if hook_failure(self.label, hook, report, applied, hook_runs)? {
                *data = processed;
            }
        }
        Ok(())
    }

    /// Embed what the source's `embed` settings ask for into `media` (in the
    /// work area), from `data`. Never fails: problems become a warning and
    /// `embed_error` in metadata.json. `shared` is another source's files
    /// when `media` is a linked duplicate of them.
    pub async fn embed(
        &self,
        media: &Path,
        data: &hooks::EmbedData,
        shared: Option<&VideoFiles>,
        duplicates: Duplicates,
    ) -> Embedding {
        let settings = &self.source.embed;
        if !embedding_on(self.source) {
            return Embedding::default();
        }
        let label = self.label;
        let cover = data.cover.as_ref().and_then(|path| {
            let format = path
                .extension()
                .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))?;
            Some((path.as_path(), format))
        });
        let request = embed::Request {
            media,
            tags: if settings.metadata {
                data.tags.clone()
            } else {
                BTreeMap::new()
            },
            cover: cover.filter(|_| settings.cover),
            chapters: if settings.chapters {
                &data.chapters
            } else {
                &[]
            },
            subtitles: if settings.subtitles {
                data.subtitles
                    .iter()
                    .map(|s| (s.path.clone(), s.lang.clone()))
                    .collect()
            } else {
                Vec::new()
            },
        };
        if settings.cover && cover.is_none() {
            tracing::warn!("{label}: no cover to embed");
        }
        match embed::embed(self.ffmpeg, self.ffprobe, &request).await {
            Ok(None) => Embedding::default(),
            Ok(Some(out)) => {
                for what in &out.skipped {
                    tracing::warn!("{label}: not embedded: {what}");
                }
                let symlinked = shared.filter(|_| media.is_symlink());
                let result = match symlinked {
                    // A symlinked duplicate: update the file it points to.
                    Some(other) => place(&out.path, Path::new(&other.media)),
                    None => std::fs::rename(&out.path, media),
                };
                match result {
                    Ok(()) => {
                        tracing::debug!("{label}: embedded {:?}", out.embedded);
                        Embedding {
                            embedded: out.embedded,
                            error: None,
                            relink: shared.is_some()
                                && symlinked.is_none()
                                && duplicates == Duplicates::Hardlink,
                        }
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&out.path);
                        embed_failed(label, e.to_string())
                    }
                }
            }
            Err(e) => embed_failed(label, e),
        }
    }

    /// `finished` hooks, on the placed files. A failure doesn't undo
    /// anything; it's recorded so the hook can be run again.
    pub async fn finished_hooks(
        &self,
        values: &Values,
        files: &VideoFiles,
        hook_runs: &mut Vec<hooks::Report>,
    ) {
        let output_dir = Path::new(&files.dir);
        let media = Path::new(&files.media);
        let work_dir = if output_dir.is_dir() {
            output_dir.to_path_buf()
        } else {
            media.parent().unwrap_or(Path::new("/")).to_path_buf()
        };
        let cover = files.cover.as_ref().map(PathBuf::from);
        for (name, hook) in self.of_type(HookType::Finished) {
            let hook_values = stage_values(
                values,
                output_dir,
                Some(media),
                cover.as_deref(),
                Some(Path::new(&files.metadata)),
            );
            let report = self
                .run_hook(name, hook, &hook_values, &work_dir, b"")
                .await;
            if let Err(e) = &report.result {
                tracing::warn!("{}: hook {name} failed: {e}", self.label);
            }
            hook_runs.push(report);
        }
    }

    /// The tags `embed.metadata` writes.
    pub fn tags(&self, info: &VideoInfo) -> BTreeMap<String, String> {
        let url = info
            .webpage_url
            .clone()
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", info.id));
        let description = info.description.clone().filter(|d| !d.is_empty());
        [
            ("title", Some(info.title.clone())),
            ("artist", info.channel.clone()),
            ("album_artist", info.channel.clone()),
            (
                "genre",
                info.categories.as_ref().and_then(|c| c.first().cloned()),
            ),
            ("date", info.upload_date_iso()),
            ("comment", Some(url)),
            ("description", description.clone()),
            ("synopsis", description),
            ("album", self.source_title.map(str::to_string)),
            ("track", Some(self.position.to_string())),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key.to_string(), v)))
        .collect()
    }
}

/// Whether any `embed` option is on.
pub fn embedding_on(source: &EffectiveSource) -> bool {
    let e = &source.embed;
    e.metadata || e.cover || e.chapters || e.subtitles
}

/// What embedding did.
#[derive(Debug, Default)]
pub struct Embedding {
    pub embedded: embed::Embedded,
    pub error: Option<String>,
    /// The media was a hardlinked duplicate and got new tags: point the
    /// other source's path at the new file once it's placed.
    pub relink: bool,
}

fn embed_failed(label: &str, error: String) -> Embedding {
    tracing::warn!("{label}: embedding failed, keeping the file as downloaded: {error}");
    Embedding {
        error: Some(error),
        ..Embedding::default()
    }
}

/// Record a `downloaded`/`embed` hook run and decide what its failure means
/// (`applied` covers both the run and its output). `Ok(true)`: use its
/// output; `Ok(false)`: continue without it (`warn`); `Err`: fail.
fn hook_failure(
    label: &str,
    hook: &HookConfig,
    mut report: hooks::Report,
    applied: Result<(), String>,
    hook_runs: &mut Vec<hooks::Report>,
) -> Result<bool, Failure> {
    if let Err(e) = &applied
        && report.result.is_ok()
    {
        report.result = Err(e.clone());
    }
    hook_runs.push(report);
    match applied {
        Ok(()) => Ok(true),
        Err(e) if hook.on_failure == OnFailure::Warn => {
            tracing::warn!("{label}: {e}; continuing without it");
            Ok(false)
        }
        Err(e) => Err(fail(e)),
    }
}

/// Files a `downloaded` hook returns must stay in the video's work area.
fn check_in_work_area(name: &str, tmp: &Path, files: &hooks::Files) -> Result<(), String> {
    let paths = std::iter::once(&files.media)
        .chain(files.cover.iter())
        .chain(files.subtitles.iter().map(|s| &s.path));
    for path in paths {
        if !path.starts_with(tmp) {
            return Err(format!(
                "hook {name} returned {}, which isn't in the work area {}",
                path.display(),
                tmp.display()
            ));
        }
    }
    Ok(())
}

/// A replaced cover must be in the format `cover_file` asks for.
fn check_cover(name: &str, files: &hooks::Files, format: CoverFormat) -> Result<(), String> {
    match &files.cover {
        Some(cover)
            if cover
                .extension()
                .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))
                != Some(format) =>
        {
            Err(format!(
                "hook {name} returned a cover that isn't a .{} like cover_file",
                format.extension()
            ))
        }
        _ => Ok(()),
    }
}

/// Placeholder values for a hook: the video's, plus where its files are at
/// this stage.
fn stage_values(
    values: &Values,
    output_dir: &Path,
    output_file: Option<&Path>,
    cover_file: Option<&Path>,
    metadata_file: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = values
        .0
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    out.insert("output_dir".into(), output_dir.display().to_string());
    for (key, path) in [
        ("output_file", output_file),
        ("cover_file", cover_file),
        ("metadata_file", metadata_file),
    ] {
        if let Some(path) = path {
            out.insert(key.into(), path.display().to_string());
        }
    }
    out
}
