//! Downloading: run yt-dlp into a temp dir, add the cover and metadata.json,
//! and move the result into its media folder in one rename.

pub mod embed;
pub mod info;
pub mod layout;
pub mod metadata;
pub mod missing;
pub mod prepare;
pub mod rerun;
pub mod stages;
pub mod thumbnail;
pub mod ytdlp;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::Timestamp;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::merge::EffectiveSource;
use crate::config::{Duplicates, HookConfig, HookType};
use crate::filter::{Decision, Fields, RuleSet};
use crate::hooks;
use crate::index::RemoteItem;
use crate::state::State;
use crate::state::items::{self, QueuedItem, status};
use info::VideoInfo;
use layout::{Layout, Values};
use metadata::Metadata;
use thumbnail::{Cover, CoverFormat};
use ytdlp::Failure;

/// Subtitle files yt-dlp may write next to the media.
const SUBTITLE_EXTENSIONS: [&str; 6] = ["vtt", "srt", "ass", "ssa", "lrc", "ttml"];

/// Where one source's downloads go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePaths {
    /// The source's media folder (`media_dir`), for `{media_dir}` in `output`.
    pub media_dir: PathBuf,
    /// Its work area (`tmp_dir`).
    pub tmp_root: PathBuf,
    pub cookies: Option<PathBuf>,
}

/// What every download shares.
pub struct Context {
    /// Every `[hooks.<name>]`.
    pub hooks: BTreeMap<String, HookConfig>,
    /// Where hook logs go (`<video_id>.log`).
    pub hook_logs: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// For `~` and `{data_dir}` in `output`.
    pub dirs: crate::paths::Dirs,
    pub ytdlp: crate::tools::Ytdlp,
    pub ytdlp_version: Option<String>,
    pub duplicates: Duplicates,
    pub thumbnail_base: String,
    pub backtrack_limit: usize,
    pub http: reqwest::Client,
    /// Grace period for yt-dlp after SIGTERM (`general.shutdown_timeout`).
    pub shutdown_timeout: std::time::Duration,
    /// Ctrl-C and `kill`: stop new work, stop running processes.
    pub shutdown: crate::shutdown::Shutdown,
}

pub struct Options {
    pub limit: Option<usize>,
    pub concurrency: usize,
    /// The run's clock: retry times, cutoffs, timestamps.
    pub now: Timestamp,
}

/// What a run of downloads did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub downloaded: usize,
    pub bytes: u64,
    pub skipped: usize,
    pub failed: usize,
    /// `finished` hooks that failed (the videos are downloaded).
    pub hooks_failed: usize,
    /// Downloaded videos with missing files.
    pub missing: missing::Report,
    /// Exit code when a signal stopped the run (130 for SIGINT, 143 for SIGTERM).
    pub interrupted: Option<i32>,
}

struct Job {
    source: Arc<EffectiveSource>,
    queued: QueuedItem,
    paths: SourcePaths,
    /// Another source's files for the same video in the same format.
    copy_from: Option<items::VideoFiles>,
    now: Timestamp,
}

enum Outcome {
    Done {
        files: items::VideoFiles,
        /// The metadata.json written, kept as a copy to restore it.
        metadata: String,
        media_bytes: u64,
        linked: bool,
    },
    Skipped(Decision),
    Failed(Failure),
}

struct Finished {
    source_key: String,
    /// How often this video failed before this attempt.
    previous_attempts: i64,
    /// The source's format string, recorded for duplicate detection.
    format: String,
    video_id: String,
    title: String,
    outcome: Outcome,
    /// Details learned from yt-dlp, to store on the video.
    details: Option<RemoteItem>,
    /// Every hook that ran.
    hook_runs: Vec<hooks::Report>,
}

/// A hook report as stored in `hook_runs`.
pub fn hook_run(
    source_key: &str,
    video_id: &str,
    report: &hooks::Report,
    retry: bool,
) -> items::HookRun {
    items::HookRun {
        source_key: source_key.to_string(),
        video_id: video_id.to_string(),
        hook: report.name.clone(),
        kind: report.kind.as_str().to_string(),
        ok: report.ok(),
        exit_code: report.exit_code,
        attempts: report.attempts,
        started_at: report.started_at.to_string(),
        finished_at: report.finished_at.to_string(),
        error: report.result.as_ref().err().cloned(),
        values: report.values.clone(),
        retry,
    }
}

/// Human-readable size: `48.2 MB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fail(message: impl Into<String>) -> Failure {
    Failure {
        permanent: false,
        message: message.into(),
    }
}

fn io_fail(context: &str, path: &Path, err: std::io::Error) -> Failure {
    fail(format!("{context} {}: {err}", path.display()))
}

/// The index item, refreshed with what yt-dlp knows.
fn merge_details(item: &RemoteItem, info: &VideoInfo) -> RemoteItem {
    RemoteItem {
        title: info.title.clone(),
        channel: info.channel.clone().or_else(|| item.channel.clone()),
        channel_id: info.channel_id.clone().or_else(|| item.channel_id.clone()),
        description: Some(info.description.clone().unwrap_or_default()),
        tags: Some(info.tags.clone().unwrap_or_default()),
        duration: info.duration_seconds().or(item.duration),
        published_at: info.published_at().or(item.published_at),
        thumbnails: if info.thumbnails().is_empty() {
            item.thumbnails.clone()
        } else {
            info.thumbnails()
        },
        // yt-dlp's full details say whether it was streamed.
        kind: item.kind.or(match info.live_status.as_deref() {
            Some("was_live" | "is_live" | "is_upcoming" | "post_live") => {
                Some(crate::index::VideoKind::Live)
            }
            Some("not_live")
                if info
                    .duration_seconds()
                    .is_some_and(|d| d > crate::index::SHORTS_MAX_SECONDS) =>
            {
                Some(crate::index::VideoKind::Video)
            }
            _ => None,
        }),
        ..item.clone()
    }
}

/// Link (or copy) a file from another source's download.
fn link(from: &Path, to: &Path, mode: Duplicates) -> std::io::Result<()> {
    match mode {
        Duplicates::Symlink => std::os::unix::fs::symlink(from, to),
        Duplicates::Copy | Duplicates::Download => std::fs::copy(from, to).map(drop),
        Duplicates::Hardlink => {
            std::fs::hard_link(from, to).or_else(|_| std::fs::copy(from, to).map(drop))
        }
    }
}

/// Move one finished file from the work area to `dest`, replacing what's
/// there. Across filesystems it's copied to a hidden name next to `dest`
/// first, so `dest` still appears in one rename.
pub(crate) fn place(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(src, dest) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
        other => return other,
    }
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staged = dest.with_file_name(format!(".{name}.tidepool-partial"));
    let copied = match std::fs::read_link(src) {
        // A symlinked duplicate stays a symlink.
        Ok(target) => {
            let _ = std::fs::remove_file(&staged);
            std::os::unix::fs::symlink(target, &staged)
        }
        Err(_) => std::fs::copy(src, &staged).map(drop),
    };
    if let Err(e) = copied.and_then(|_| std::fs::rename(&staged, dest)) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    std::fs::remove_file(src)
}

/// Place every file, the metadata file last (a video counts as present
/// once it's there). On failure, files already placed are removed.
fn place_all(moves: &[(PathBuf, PathBuf)]) -> Result<(), Failure> {
    let mut seen = std::collections::HashSet::new();
    for (_, dest) in moves {
        if !seen.insert(dest) {
            return Err(fail(format!(
                "two files would go to {}; make the path settings differ",
                dest.display()
            )));
        }
    }
    for (i, (src, dest)) in moves.iter().enumerate() {
        if let Err(e) = place(src, dest) {
            for (_, placed) in &moves[..i] {
                let _ = std::fs::remove_file(placed);
            }
            return Err(io_fail("cannot move the download to", dest, e));
        }
    }
    Ok(())
}

/// Whether `name` is a folder tidepool creates in a work area
/// (`<video_id>.<source>`).
fn is_work_folder(name: &str) -> bool {
    let Some((id, source)) = name.split_once('.') else {
        return false;
    };
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !source.is_empty()
        && source
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

async fn process(
    job: &Job,
    ctx: &Context,
    hook_runs: &mut Vec<hooks::Report>,
) -> (Outcome, Option<RemoteItem>) {
    let source = &job.source;
    let item = &job.queued.item;
    let video_id = &job.queued.video_id;
    let mut details = None;
    let started = std::time::Instant::now();
    let label = format!("{}: “{}”", source.key, item.title);
    tracing::debug!("{}: downloading “{}” [{video_id}]", source.key, item.title);
    if job.queued.status == status::PENDING_CHECK {
        tracing::debug!("{label}: checking its details before deciding");
    }
    if let Some(files) = &job.copy_from {
        tracing::debug!(
            "{label}: already downloaded as {}; linking its files",
            files.media
        );
    }

    // A decision that needed fields the index didn't have.
    let mut info = None;
    if job.queued.status == status::PENDING_CHECK || job.copy_from.is_some() {
        match ytdlp::video_info(
            &ctx.ytdlp,
            video_id,
            job.paths.cookies.as_deref(),
            &ctx.shutdown,
            ctx.shutdown_timeout,
        )
        .await
        {
            Ok(full) => {
                let mut merged = merge_details(item, &full);
                // Not a stream and short enough: is it a Short?
                if merged.kind.is_none() && full.live_status.as_deref() == Some("not_live") {
                    crate::index::shorts::ShortsCheck::default()
                        .classify(std::slice::from_mut(&mut merged), true, &ctx.shutdown)
                        .await;
                }
                details = Some(merged.clone());
                info = Some(full);
                if job.queued.status == status::PENDING_CHECK {
                    let rules = match RuleSet::compile(source, ctx.backtrack_limit, job.now) {
                        Ok(rules) => rules,
                        Err(err) => return (Outcome::Failed(fail(err)), details),
                    };
                    match rules.decide(&Fields::from_item(&merged)) {
                        // Still unknown after a full lookup: nothing more to learn.
                        Decision::Keep { .. } | Decision::Deferred { .. } => {}
                        decision => return (Outcome::Skipped(decision), details),
                    }
                }
            }
            Err(failure) => return (Outcome::Failed(failure), details),
        }
    }

    let tmp = job
        .paths
        .tmp_root
        .join(format!("{video_id}.{}", source.key));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(err) = std::fs::create_dir_all(&tmp) {
        return (
            Outcome::Failed(io_fail("cannot create", &tmp, err)),
            details,
        );
    }
    let result = assemble(job, ctx, &tmp, info, &mut details, started, hook_runs).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    match result {
        Ok(outcome) => (outcome, details),
        Err(failure) => (Outcome::Failed(failure), details),
    }
}

/// Fill `tmp` with the media, subtitles, cover and metadata, then place
/// each file at its destination.
async fn assemble(
    job: &Job,
    ctx: &Context,
    tmp: &Path,
    info: Option<VideoInfo>,
    details: &mut Option<RemoteItem>,
    started: std::time::Instant,
    hook_runs: &mut Vec<hooks::Report>,
) -> Result<Outcome, Failure> {
    let source = &job.source;
    let label = &format!("{}: “{}”", source.key, job.queued.item.title);
    let video_id = &job.queued.video_id;

    // Media and subtitles: linked from another source, or downloaded. In the
    // work area they're named `media.<ext>` and `media.<lang>.<ext>`.
    let (info, other_cover) = match (&job.copy_from, info) {
        (Some(other), Some(info)) => {
            let ext = extension_of(&other.media);
            let media = PathBuf::from(&other.media);
            link(&media, &tmp.join(format!("media.{ext}")), ctx.duplicates)
                .map_err(|e| io_fail("cannot link", &media, e))?;
            for sub in &other.subtitles {
                let path = PathBuf::from(&sub.path);
                let name = format!("media.{}.{}", sub.lang, extension_of(&sub.path));
                link(&path, &tmp.join(name), ctx.duplicates)
                    .map_err(|e| io_fail("cannot link", &path, e))?;
            }
            (info, other.cover.clone().map(PathBuf::from))
        }
        _ => {
            let args = ytdlp::download_args(source, video_id, tmp, job.paths.cookies.as_deref());
            let media = ytdlp::download(
                &ctx.ytdlp,
                &args,
                label,
                &ctx.shutdown,
                ctx.shutdown_timeout,
            )
            .await?;
            let info_path = tmp.join("ytdlp.info.json");
            let text = std::fs::read_to_string(&info_path)
                .map_err(|e| io_fail("cannot read", &info_path, e))?;
            let info = VideoInfo::parse(&text).map_err(fail)?;
            *details = Some(merge_details(&job.queued.item, &info));
            if !source.keep_raw_info {
                let _ = std::fs::remove_file(&info_path);
            }
            let name = media
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !tmp.join(&name).is_file() {
                return Err(fail(format!(
                    "yt-dlp reported {} but it isn't in the work area",
                    media.display()
                )));
            }
            (info, None)
        }
    };

    // What's in the work area now.
    let mut media = None;
    let mut subtitles: Vec<hooks::Subtitle> = Vec::new();
    for entry in std::fs::read_dir(tmp)
        .map_err(|e| io_fail("cannot read", tmp, e))?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let parts: Vec<&str> = name.split('.').collect();
        match parts.as_slice() {
            ["media", _] => media = Some(entry.path()),
            ["media", lang @ .., ext] if SUBTITLE_EXTENSIONS.contains(ext) && !lang.is_empty() => {
                subtitles.push(hooks::Subtitle {
                    lang: lang.join("."),
                    path: entry.path(),
                });
            }
            _ => {}
        }
    }
    subtitles.sort_by(|a, b| a.lang.cmp(&b.lang));
    let media = media.ok_or_else(|| fail("no media file in the work area"))?;

    let stage = stages::Stage {
        source,
        hook_names: &source.hooks,
        hooks: &ctx.hooks,
        video_id,
        source_title: job.queued.source_title.as_deref(),
        position: job.queued.position,
        label,
        home: &ctx.dirs.home,
        hook_logs: &ctx.hook_logs,
        shutdown: &ctx.shutdown,
        grace: ctx.shutdown_timeout,
        ffmpeg: &ctx.ffmpeg,
        ffprobe: &ctx.ffprobe,
    };

    // `prepare` hooks: the details and paths everything below uses. The
    // details recorded for the video (`details`) stay YouTube's.
    let original = prepare::Details::of(&info);
    let mut info = info;
    let overrides = stage
        .prepare_hooks(
            &mut info,
            &job.paths.media_dir,
            &ctx.dirs,
            tmp,
            &media,
            hook_runs,
        )
        .await?;
    let originals = prepare::Details::of(&info).originals(&original);

    let values = Values::new(
        &info,
        source,
        job.queued.source_title.as_deref(),
        job.queued.position,
    );
    let layout = Layout::new(source, &values, &job.paths.media_dir, &ctx.dirs)
        .map_err(fail)?
        .with_overrides(&overrides);
    let (cover_dest, cover_format) = layout.cover_file().map_err(fail)?;
    let metadata_dest = layout.metadata_file().map_err(fail)?;

    // Cover: reuse the other source's when it's already in the wanted
    // format, or fetch the biggest.
    let cover_tmp = tmp.join(format!("cover.{}", cover_format.extension()));
    let mut cover: Option<Cover> = None;
    let mut cover_file = None;
    let reusable = other_cover.filter(|c| {
        c.is_file()
            && c.extension()
                .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))
                == Some(cover_format)
    });
    if let Some(other_cover) = reusable {
        link(&other_cover, &cover_tmp, ctx.duplicates)
            .map_err(|e| io_fail("cannot link", &other_cover, e))?;
        tracing::debug!("{label}: cover linked from {}", other_cover.display());
        cover_file = Some(cover_tmp);
    } else {
        let thumbs = details
            .as_ref()
            .map(|d| d.thumbnails.clone())
            .unwrap_or_default();
        let urls = thumbnail::candidates(video_id, &thumbs, &ctx.thumbnail_base, cover_format);
        match thumbnail::fetch_largest(&ctx.http, &urls, cover_format).await {
            Some(found) => {
                tracing::debug!(
                    "{label}: cover {}×{} from {}",
                    found.width,
                    found.height,
                    found.source_url
                );
                std::fs::write(&cover_tmp, &found.bytes)
                    .map_err(|e| io_fail("cannot write", &cover_tmp, e))?;
                cover_file = Some(cover_tmp);
                cover = Some(found);
            }
            None => tracing::warn!("{}: no cover image found for {video_id}", source.key),
        }
    }

    // `downloaded` hooks: on the raw files; they may replace them.
    let context = stage.context(&info);
    let mut raw = hooks::Files {
        media,
        cover: cover_file,
        subtitles,
    };
    stage
        .downloaded_hooks(&mut raw, &values, &context, tmp, cover_format, hook_runs)
        .await?;

    let media_ext = raw
        .media
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let media_dest = layout.output_file(&media_ext).map_err(fail)?;
    let mut moves: Vec<(PathBuf, PathBuf)> = vec![(raw.media.clone(), media_dest.clone())];
    let mut files = items::VideoFiles {
        dir: layout.output_dir().display().to_string(),
        media: media_dest.display().to_string(),
        metadata: metadata_dest.display().to_string(),
        ..items::VideoFiles::default()
    };
    for sub in &raw.subtitles {
        let ext = sub
            .path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dest = layout.subtitle_file(&sub.lang, &ext).map_err(fail)?;
        moves.push((sub.path.clone(), dest.clone()));
        files.subtitles.push(items::Subtitle {
            lang: sub.lang.clone(),
            path: dest.display().to_string(),
        });
    }
    if let Some(cover_tmp) = &raw.cover {
        moves.push((cover_tmp.clone(), cover_dest.clone()));
        files.cover = Some(cover_dest.display().to_string());
    } else {
        cover = None;
    }

    let raw_info = tmp.join("ytdlp.info.json");
    if source.keep_raw_info && raw_info.is_file() {
        let mut name = metadata_dest.file_stem().unwrap_or_default().to_owned();
        name.push(".ytdlp.json");
        let dest = metadata_dest.with_file_name(name);
        moves.push((raw_info, dest.clone()));
        files.raw_info = Some(dest.display().to_string());
    }

    // What gets embedded: processed by `embed` hooks, then embedded.
    let mut data = hooks::EmbedData {
        tags: stage.tags(&info, &values),
        cover: raw.cover.clone(),
        chapters: info.chapters.clone().unwrap_or_default(),
        subtitles: raw.subtitles.clone(),
    };
    stage
        .embed_hooks(&mut data, &values, &context, tmp, &raw, hook_runs)
        .await?;
    let embedding = stage
        .embed(
            &raw.media,
            &data,
            job.copy_from.as_ref(),
            ctx.duplicates,
            false,
        )
        .await;

    let media_bytes = std::fs::metadata(&raw.media)
        .map_err(|e| io_fail("cannot read", &raw.media, e))?
        .len();
    let metadata = Metadata {
        info: &info,
        source,
        source_title: job.queued.source_title.as_deref(),
        position: job.queued.position,
        files: &files,
        media_bytes,
        cover: cover.as_ref(),
        embedded: &embedding.embedded,
        embedded_tags: embedding.embedded.metadata.then_some(&data.tags),
        embed_error: embedding.error.as_deref(),
        original: originals.as_ref(),
        downloaded_at: job.now,
        ytdlp_version: ctx.ytdlp_version.as_deref(),
    };
    let json =
        serde_json::to_string_pretty(&metadata.to_json()).expect("metadata serializes") + "\n";
    let metadata_path = tmp.join("metadata.json");
    let mut file = std::fs::File::create(&metadata_path)
        .map_err(|e| io_fail("cannot write", &metadata_path, e))?;
    file.write_all(json.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| io_fail("cannot write", &metadata_path, e))?;
    moves.push((metadata_path, metadata_dest.clone()));

    // Stopping: nothing is placed (an embed may have been cut short); the
    // video goes back to the queue.
    if ctx.shutdown.is_stopping() {
        return Err(fail("stopped"));
    }
    place_all(&moves)?;
    let _ = std::fs::remove_dir_all(tmp);
    if embedding.relink
        && let Some(other) = &job.copy_from
    {
        // Linked duplicates share one file: the latest tags win for both.
        if let Err(e) = relink(&media_dest, Path::new(&other.media)) {
            tracing::warn!(
                "{label}: cannot update the linked copy at {}: {e}",
                other.media
            );
        }
    }
    tracing::debug!(
        "{label}: saved as {} ({:.1}s)",
        media_dest.display(),
        started.elapsed().as_secs_f64()
    );

    stage
        .finished_hooks(&values, &files, &context, hook_runs)
        .await;

    let metadata = std::fs::read_to_string(&files.metadata).unwrap_or(json);
    Ok(Outcome::Done {
        files,
        metadata,
        media_bytes,
        linked: job.copy_from.is_some(),
    })
}

/// Point `other` at the same file as `ours` (a hardlink, or a copy across
/// filesystems), replacing it in one rename.
fn relink(ours: &Path, other: &Path) -> std::io::Result<()> {
    let name = other
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staged = other.with_file_name(format!(".{name}.tidepool-partial"));
    let _ = std::fs::remove_file(&staged);
    std::fs::hard_link(ours, &staged).or_else(|_| std::fs::copy(ours, &staged).map(drop))?;
    std::fs::rename(&staged, other).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })
}

/// A file name's extension, `""` if none.
fn extension_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Clear the work areas and put interrupted downloads back in the queue.
/// Only the folders tidepool creates there are removed, so a `tmp_dir`
/// pointing somewhere shared never loses anything else.
pub fn recover(state: &State, tmp_roots: &[PathBuf]) -> Result<usize, String> {
    for tmp_root in tmp_roots.iter().filter(|t| t.is_dir()) {
        let entries = std::fs::read_dir(tmp_root)
            .map_err(|e| format!("cannot clear {}: {e}", tmp_root.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if is_work_folder(&name.to_string_lossy()) && entry.path().is_dir() {
                std::fs::remove_dir_all(entry.path())
                    .map_err(|e| format!("cannot clear {}: {e}", entry.path().display()))?;
            }
        }
    }
    items::reset_interrupted(&state.conn).map_err(|e| e.to_string())
}

/// Download everything queued for `sources`.
pub async fn download_all(
    state: &mut State,
    sources: &[EffectiveSource],
    paths: impl Fn(&EffectiveSource) -> Result<SourcePaths, String>,
    ctx: Context,
    options: Options,
    tracker: &mut crate::problems::Tracker,
    out: &mut dyn Write,
) -> Result<Summary, String> {
    let now = options.now;
    let db = |e: rusqlite::Error| format!("state database: {e}");
    let mut by_source = std::collections::HashMap::new();
    for source in sources {
        by_source.insert(
            source.key.clone(),
            paths(source).map_err(|e| format!("{}: {e}", source.key))?,
        );
    }
    let mut tmp_roots: Vec<PathBuf> = by_source.values().map(|p| p.tmp_root.clone()).collect();
    tmp_roots.sort();
    tmp_roots.dedup();
    recover(state, &tmp_roots)?;
    let mut summary = Summary {
        missing: missing::check(state, sources, &by_source, &ctx, out).await?,
        ..Summary::default()
    };

    let keys: Vec<String> = sources.iter().map(|s| s.key.clone()).collect();
    let mut queue = items::download_queue(&state.conn, &keys, now).map_err(db)?;
    if let Some(limit) = options.limit {
        queue.truncate(limit);
    }
    if queue.is_empty() {
        return Ok(summary);
    }

    let ctx = Arc::new(ctx);
    let semaphore = Arc::new(Semaphore::new(options.concurrency.max(1)));
    let sources: Vec<Arc<EffectiveSource>> = sources.iter().cloned().map(Arc::new).collect();
    let mut tasks = JoinSet::new();
    for queued in queue {
        let source = sources
            .iter()
            .find(|s| s.key == queued.source_key)
            .expect("queued for a known source")
            .clone();
        let copy_from = match ctx.duplicates {
            Duplicates::Download => None,
            _ => items::find_copy(&state.conn, &queued.video_id, &source.format, &source.key)
                .map_err(db)?
                .filter(|files| Path::new(&files.metadata).is_file()),
        };
        items::set_status(
            &state.conn,
            &queued.source_key,
            &queued.video_id,
            status::DOWNLOADING,
        )
        .map_err(db)?;
        let job = Job {
            paths: by_source[&source.key].clone(),
            source,
            queued,
            copy_from,
            now,
        };
        let (ctx, semaphore) = (ctx.clone(), semaphore.clone());
        tasks.spawn(async move {
            let _permit = tokio::select! {
                permit = semaphore.acquire_owned() => permit.expect("semaphore open"),
                _ = ctx.shutdown.stopping() => return None,
            };
            if ctx.shutdown.is_stopping() {
                return None;
            }
            let mut hook_runs = Vec::new();
            let (outcome, details) = process(&job, &ctx, &mut hook_runs).await;
            let title = details
                .as_ref()
                .map(|d| d.title.clone())
                .unwrap_or_else(|| job.queued.item.title.clone());
            Some(Finished {
                previous_attempts: job.queued.attempts,
                source_key: job.queued.source_key,
                format: job.source.format.clone(),
                video_id: job.queued.video_id,
                title,
                outcome,
                details,
                hook_runs,
            })
        });
    }

    loop {
        tokio::select! {
            finished = tasks.join_next() => {
                let Some(finished) = finished else { break };
                let Ok(Some(f)) = finished else { continue };
                record(state, &f, now, &mut summary, tracker, out).map_err(db)?;
            }
            _ = ctx.shutdown.stopping() => {
                // Running processes are being stopped; wait for every task,
                // so nothing writes to the work area while it's cleared.
                // Downloads that finished in the meantime still count.
                while let Some(finished) = tasks.join_next().await {
                    if let Ok(Some(f)) = finished
                        && matches!(f.outcome, Outcome::Done { .. })
                    {
                        record(state, &f, now, &mut summary, tracker, out).map_err(db)?;
                    }
                }
                recover(state, &tmp_roots)?;
                summary.interrupted = ctx.shutdown.code();
                break;
            }
        }
    }
    Ok(summary)
}

fn record(
    state: &State,
    f: &Finished,
    now: Timestamp,
    summary: &mut Summary,
    tracker: &mut crate::problems::Tracker,
    out: &mut dyn Write,
) -> rusqlite::Result<()> {
    if let Some(details) = &f.details {
        items::update_video_details(&state.conn, details, now)?;
    }
    for report in &f.hook_runs {
        items::record_hook_run(
            &state.conn,
            &hook_run(&f.source_key, &f.video_id, report, false),
        )?;
        match &report.result {
            Ok(_) => tracker.hook_succeeded(),
            // Stopped by Ctrl-C: not the hook's fault.
            Err(e) if e == "stopped" => {}
            Err(e) => {
                tracker.hook_failed(&f.source_key, &report.name, e);
                if report.kind == HookType::Finished {
                    summary.hooks_failed += 1;
                }
            }
        }
    }
    match &f.outcome {
        Outcome::Done {
            files,
            metadata,
            media_bytes,
            linked,
        } => {
            items::mark_done(
                &state.conn,
                &f.source_key,
                &f.video_id,
                items::Done {
                    files,
                    metadata,
                    format: &f.format,
                    media_bytes: *media_bytes,
                },
                now,
            )?;
            summary.downloaded += 1;
            tracker.download_succeeded();
            summary.bytes += media_bytes;
            let how = if *linked {
                "linked from another source"
            } else {
                "downloaded"
            };
            let _ = writeln!(
                out,
                "{}: {how} “{}” ({})",
                f.source_key,
                f.title,
                human_bytes(*media_bytes)
            );
        }
        Outcome::Skipped(decision) => {
            items::record_decision(&state.conn, &f.source_key, &f.video_id, decision)?;
            summary.skipped += 1;
            let _ = writeln!(out, "{}: skipped “{}”: {decision}", f.source_key, f.title);
        }
        Outcome::Failed(failure) => {
            items::mark_failed(
                &state.conn,
                &f.source_key,
                &f.video_id,
                &failure.message,
                failure.permanent,
                now,
            )?;
            summary.failed += 1;
            tracker.download_failed(&f.source_key, &failure.message, f.previous_attempts);
            if failure.permanent {
                tracing::warn!(
                    "{}: “{}” is unavailable: {}",
                    f.source_key,
                    f.title,
                    failure.message
                );
            } else {
                tracing::error!(
                    "{}: “{}” failed (will retry): {}",
                    f.source_key,
                    f.title,
                    failure.message
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_work_folders() {
        assert!(is_work_folder("dQw4w9WgXcQ.lofi"));
        assert!(is_work_folder("a-b_c1234XY.my-source_2"));
        for other in [
            "lofi",
            "dQw4w9WgXcQ",
            "short.lofi",
            "dQw4w9WgXcQ.",
            "dQw4w9WgXcQ.Lofi",
            ".git",
        ] {
            assert!(!is_work_folder(other), "{other:?}");
        }
    }

    #[test]
    fn places_files_metadata_last_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("work");
        std::fs::create_dir_all(&tmp).unwrap();
        for name in ["media.mkv", "cover", "metadata.json"] {
            std::fs::write(tmp.join(name), name).unwrap();
        }
        let shared = dir.path().join("media/lofi");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("other video.mkv"), "keep").unwrap();
        std::fs::write(shared.join("x.mkv"), "old").unwrap();
        let moves = vec![
            (tmp.join("media.mkv"), shared.join("x.mkv")),
            (tmp.join("cover"), dir.path().join("covers/x.jpg")),
            (tmp.join("metadata.json"), shared.join("x.json")),
        ];
        place_all(&moves).unwrap();
        assert_eq!(
            std::fs::read_to_string(shared.join("x.mkv")).unwrap(),
            "media.mkv"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("covers/x.jpg")).unwrap(),
            "cover"
        );
        assert_eq!(
            std::fs::read_to_string(shared.join("x.json")).unwrap(),
            "metadata.json"
        );
        assert_eq!(
            std::fs::read_to_string(shared.join("other video.mkv")).unwrap(),
            "keep",
            "a shared folder keeps other videos"
        );

        // A file that can't be placed: the ones already placed are removed.
        std::fs::write(tmp.join("media.mkv"), "again").unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "a file, not a folder").unwrap();
        let moves = vec![
            (tmp.join("media.mkv"), shared.join("y.mkv")),
            (tmp.join("missing"), blocked.join("y.jpg")),
        ];
        assert!(place_all(&moves).is_err());
        assert!(!shared.join("y.mkv").exists());

        // Two files can't share a destination.
        let same = vec![
            (tmp.join("a"), shared.join("z")),
            (tmp.join("b"), shared.join("z")),
        ];
        assert!(
            place_all(&same)
                .unwrap_err()
                .message
                .contains("two files would go to")
        );
    }
}
