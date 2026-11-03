//! Downloading: run yt-dlp into a temp dir, add the cover and metadata.json,
//! and move the result into its media folder in one rename.

pub mod backfill;
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
use tracing::Instrument;

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
    /// Told each time a download is waiting to be processed.
    pub fetched: Option<Arc<tokio::sync::Notify>>,
}

pub struct Options {
    pub limit: Option<usize>,
    /// Downloads at once.
    pub concurrency: usize,
    /// Videos processed at once (hooks, embedding, placing).
    pub process_concurrency: usize,
    /// Download what's queued.
    pub fetch: bool,
    /// Process what's downloaded (including what this run downloads).
    pub process: bool,
    /// Clean up after a crash first: work areas, interrupted videos.
    pub recover: bool,
    /// The run's clock: retry times, cutoffs, timestamps.
    pub now: Timestamp,
}

/// What a run of downloads did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Downloaded into the work area, waiting to be processed.
    pub fetched: usize,
    /// Processed and saved.
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

impl Job {
    /// How the video shows in the log: `<source>: “<title>”`.
    fn label(&self) -> String {
        format!("{}: “{}”", self.source.key, self.queued.item.title)
    }

    /// Place the files in `moves` (metadata last), clear the work area `tmp`
    /// and, when `relink`, point the linked copy at the placed media.
    fn save(
        &self,
        ctx: &Context,
        tmp: &Path,
        moves: &[(PathBuf, PathBuf)],
        relink: bool,
        media_dest: &Path,
    ) -> Result<(), Failure> {
        // Stopping: nothing is placed (an embed may have been cut short); the
        // video goes back to the queue.
        if ctx.shutdown.is_stopping() {
            return Err(fail("stopped"));
        }
        self.step("saving");
        place_all(moves)?;
        let _ = std::fs::remove_dir_all(tmp);
        if relink && let Some(other) = &self.copy_from {
            relink_copy(media_dest, other, &self.label());
        }
        Ok(())
    }

    /// Tell the live panel which processing step the video is on.
    fn step(&self, what: &str) {
        crate::events::emit(crate::events::Event::Finishing {
            source: self.source.key.clone(),
            video_id: self.queued.video_id.clone(),
            title: self.queued.item.title.clone(),
            step: what.to_string(),
        });
    }

    /// The processing stages for this job, logged under `label`.
    fn stage<'a>(&'a self, ctx: &'a Context, label: &'a str) -> stages::Stage<'a> {
        let source = &self.source;
        stages::Stage {
            source,
            hook_names: &source.hooks,
            hooks: &ctx.hooks,
            video_id: &self.queued.video_id,
            source_title: self.queued.source_title.as_deref(),
            position: self.queued.position,
            label,
            home: &ctx.dirs.home,
            hook_logs: &ctx.hook_logs,
            shutdown: &ctx.shutdown,
            grace: ctx.shutdown_timeout,
            ffmpeg: &ctx.ffmpeg,
            ffprobe: &ctx.ffprobe,
            data_dir: &ctx.dirs.data_dir,
        }
    }
}

enum Outcome {
    /// Downloaded into the work area, waiting to be processed.
    Fetched {
        bytes: u64,
    },
    /// Processed and placed.
    Done {
        files: items::VideoFiles,
        /// The metadata.json written, kept as a copy to restore it.
        metadata: String,
        media_bytes: u64,
        linked: bool,
    },
    Skipped(Decision),
    Failed(Failure),
    /// Processing was cut short by Ctrl-C or `kill`; the download stays in
    /// the work area.
    Stopped,
}

/// Which half of a video's way into the library a task did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fetch,
    Process,
}

struct Finished {
    phase: Phase,
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
    /// The source's `finished` hooks, queued once the video is placed.
    pending_hooks: Vec<String>,
}

/// A hook report as stored in `hook_runs`.
#[must_use]
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
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a one-decimal display value; precision past 2^52 bytes doesn't matter"
)]
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

/// A float as a whole number, dropping the fraction. Negative values and
/// NaN become 0, values past `u64::MAX` become `u64::MAX`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "std has no checked float-to-int conversion; `as` saturates, which is what's wanted"
)]
fn whole(value: f64) -> u64 {
    value as u64
}

fn fail(message: impl Into<String>) -> Failure {
    Failure {
        permanent: false,
        message: message.into(),
    }
}

fn io_fail(context: &str, path: &Path, err: &std::io::Error) -> Failure {
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
    if let Err(e) = copied.and_then(|()| std::fs::rename(&staged, dest)) {
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
            return Err(io_fail("cannot move the download to", dest, &e));
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

/// A video's work area: `<tmp_root>/<video_id>.<source>`. Fetching leaves
/// the download in `fetched/`; processing works on a copy in `process/`, so
/// it can start over from `fetched/` after a crash or Ctrl-C.
#[must_use]
pub fn work_area(tmp_root: &Path, video_id: &str, source_key: &str) -> PathBuf {
    tmp_root.join(format!("{video_id}.{source_key}"))
}

/// What fetching knows that processing needs, kept in the work area.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct FetchNote {
    /// Another source's files the download was linked from.
    copy_from: Option<items::VideoFiles>,
}

const FETCH_NOTE: &str = "fetch.json";
const INFO_JSON: &str = "ytdlp.info.json";

/// Download a video into its work area (or link another source's copy).
async fn fetch(job: &Job, ctx: &Context) -> (Outcome, Option<RemoteItem>) {
    let source = &job.source;
    let item = &job.queued.item;
    let video_id = &job.queued.video_id;
    let mut details = None;
    let label = job.label();
    tracing::debug!("{}: downloading “{}” [{video_id}]", source.key, item.title);
    crate::events::emit(crate::events::Event::Downloading {
        source: source.key.clone(),
        video_id: video_id.clone(),
        title: item.title.clone(),
        percent: None,
        size: None,
        speed: None,
        eta: None,
    });
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
    let mut info_json = None;
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
            Ok((full, text)) => {
                let mut merged = merge_details(item, &full);
                // Not a stream and short enough: is it a Short?
                if merged.kind.is_none() && full.live_status.as_deref() == Some("not_live") {
                    crate::index::shorts::ShortsCheck::default()
                        .classify(std::slice::from_mut(&mut merged), true, &ctx.shutdown)
                        .await;
                }
                details = Some(merged.clone());
                info_json = Some(text);
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

    let work = work_area(&job.paths.tmp_root, video_id, &source.key);
    let fetched = work.join("fetched");
    let _ = std::fs::remove_dir_all(&work);
    if let Err(err) = std::fs::create_dir_all(&fetched) {
        return (
            Outcome::Failed(io_fail("cannot create", &fetched, &err)),
            details,
        );
    }
    let result = fetch_files(job, ctx, &fetched, info_json, &mut details, &label).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&work);
    }
    match result {
        Ok(bytes) => (Outcome::Fetched { bytes }, details),
        Err(failure) => (Outcome::Failed(failure), details),
    }
}

/// The media and subtitles, linked from another source or downloaded, into
/// `fetched` as `media.<ext>` and `media.<lang>.<ext>`, with yt-dlp's info
/// JSON. Returns the media's size.
async fn fetch_files(
    job: &Job,
    ctx: &Context,
    fetched: &Path,
    info_json: Option<String>,
    details: &mut Option<RemoteItem>,
    label: &str,
) -> Result<u64, Failure> {
    let source = &job.source;
    let video_id = &job.queued.video_id;
    if let (Some(other), Some(text)) = (&job.copy_from, info_json) {
        let ext = extension_of(&other.media);
        let media = PathBuf::from(&other.media);
        link(
            &media,
            &fetched.join(format!("media.{ext}")),
            ctx.duplicates,
        )
        .map_err(|e| io_fail("cannot link", &media, &e))?;
        for sub in &other.subtitles {
            let path = PathBuf::from(&sub.path);
            let name = format!("media.{}.{}", sub.lang, extension_of(&sub.path));
            link(&path, &fetched.join(name), ctx.duplicates)
                .map_err(|e| io_fail("cannot link", &path, &e))?;
        }
        let info_path = fetched.join(INFO_JSON);
        std::fs::write(&info_path, text).map_err(|e| io_fail("cannot write", &info_path, &e))?;
    } else {
        let args = ytdlp::download_args(source, video_id, fetched, job.paths.cookies.as_deref());
        let live = ytdlp::Live {
            source: source.key.clone(),
            video_id: video_id.clone(),
            title: job.queued.item.title.clone(),
        };
        let media = ytdlp::download(
            &ctx.ytdlp,
            &args,
            label,
            Some(&live),
            &ctx.shutdown,
            ctx.shutdown_timeout,
        )
        .await?;
        let info_path = fetched.join(INFO_JSON);
        let text = std::fs::read_to_string(&info_path)
            .map_err(|e| io_fail("cannot read", &info_path, &e))?;
        let info = VideoInfo::parse(&text).map_err(fail)?;
        *details = Some(merge_details(&job.queued.item, &info));
        let name = media
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !fetched.join(&name).is_file() {
            return Err(fail(format!(
                "yt-dlp reported {} but it isn't in the work area",
                media.display()
            )));
        }
    }
    let note = FetchNote {
        copy_from: job.copy_from.clone(),
    };
    let note_path = fetched.join(FETCH_NOTE);
    std::fs::write(&note_path, serde_json::to_vec(&note).expect("JSON"))
        .map_err(|e| io_fail("cannot write", &note_path, &e))?;
    let (media, _) = work_files(fetched)?;
    Ok(std::fs::metadata(&media).map_or(0, |m| m.len()))
}

/// The media file and subtitles in a work folder.
fn work_files(dir: &Path) -> Result<(PathBuf, Vec<hooks::Subtitle>), Failure> {
    let mut media = None;
    let mut subtitles: Vec<hooks::Subtitle> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| io_fail("cannot read", dir, &e))?
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
    Ok((media, subtitles))
}

/// Process a fetched video: hooks, embedding, placing. It works on a copy of
/// `fetched/` (hardlinks; symlinks stay symlinks), so `fetched/` is still
/// there if it's cut short. Done or failed, the work area goes; stopped, the
/// download stays for the next run.
async fn process(
    job: &mut Job,
    ctx: &Context,
    hook_runs: &mut Vec<hooks::Report>,
) -> (Outcome, Option<RemoteItem>) {
    let video_id = &job.queued.video_id;
    let work = work_area(&job.paths.tmp_root, video_id, &job.source.key);
    let fetched = work.join("fetched");
    let scratch = work.join("process");
    let prepared = (|| {
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).map_err(|e| io_fail("cannot create", &scratch, &e))?;
        let entries = std::fs::read_dir(&fetched).map_err(|_| {
            fail("its download is gone from the work area; it will be downloaded again")
        })?;
        for entry in entries.flatten() {
            stage_copy(&entry.path(), &scratch.join(entry.file_name()))
                .map_err(|e| io_fail("cannot copy", &entry.path(), &e))?;
        }
        let note: FetchNote = std::fs::read(scratch.join(FETCH_NOTE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let info_path = scratch.join(INFO_JSON);
        let text = std::fs::read_to_string(&info_path)
            .map_err(|e| io_fail("cannot read", &info_path, &e))?;
        Ok::<_, Failure>((note, VideoInfo::parse(&text).map_err(fail)?))
    })();
    let (note, info) = match prepared {
        Ok(found) => found,
        Err(failure) => {
            let _ = std::fs::remove_dir_all(&work);
            return (Outcome::Failed(failure), None);
        }
    };
    job.copy_from = note.copy_from;
    let details = Some(merge_details(&job.queued.item, &info));
    let result = assemble(job, ctx, &scratch, info, details.as_ref(), hook_runs).await;
    match result {
        Err(_) if ctx.shutdown.is_stopping() => {
            let _ = std::fs::remove_dir_all(&scratch);
            (Outcome::Stopped, None)
        }
        Ok(outcome) => {
            let _ = std::fs::remove_dir_all(&work);
            (outcome, None)
        }
        Err(failure) => {
            let _ = std::fs::remove_dir_all(&work);
            (Outcome::Failed(failure), None)
        }
    }
}

/// Put `src` at `dest` for processing without copying its data.
fn stage_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    match std::fs::read_link(src) {
        Ok(target) => std::os::unix::fs::symlink(target, dest),
        Err(_) => std::fs::hard_link(src, dest).or_else(|_| std::fs::copy(src, dest).map(drop)),
    }
}

/// Process the fetched files in `tmp`: hooks, cover, embedding, metadata,
/// then place everything (metadata last) and run `finished` hooks.
async fn assemble(
    job: &Job,
    ctx: &Context,
    tmp: &Path,
    info: VideoInfo,
    details: Option<&RemoteItem>,
    hook_runs: &mut Vec<hooks::Report>,
) -> Result<Outcome, Failure> {
    let started = std::time::Instant::now();
    let source = &job.source;
    let label = &job.label();
    let (media, subtitles) = work_files(tmp)?;
    let stage = job.stage(ctx, label);
    job.step("preparing");

    // `prepare` hooks: the details and paths everything below uses. The
    // details recorded for the video (`details`) stay YouTube's.
    let mut info = info;
    let (overrides, originals) =
        prepare_details(job, ctx, &stage, &mut info, tmp, &media, hook_runs).await?;

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

    job.step("getting the cover");
    let (cover, cover_file) = get_cover(job, ctx, tmp, label, details, cover_format).await?;

    if stage.has(HookType::Downloaded) {
        job.step("running hooks");
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

    let Placement {
        media_dest,
        mut moves,
        files,
    } = plan_moves(source, &layout, &raw, tmp, &cover_dest, &metadata_dest)?;
    // A `downloaded` hook may have removed the cover.
    let cover = cover.filter(|_| raw.cover.is_some());

    if stages::embedding_on(source) {
        job.step("embedding");
    }
    // What gets embedded: processed by `embed` hooks, then embedded.
    let mut data = hooks::EmbedData {
        tags: stage.tags(&info, &values),
        cover: raw.cover.clone(),
        chapters: info.chapters.clone().unwrap_or_default(),
        subtitles: raw.subtitles.clone(),
    };
    let musicbrainz = stage.musicbrainz(&mut data.tags, &info, None).await;
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
        .map_err(|e| io_fail("cannot read", &raw.media, &e))?
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
        musicbrainz: musicbrainz.as_ref(),
        downloaded_at: job.now,
        ytdlp_version: ctx.ytdlp_version.as_deref(),
    };
    let json =
        serde_json::to_string_pretty(&metadata.to_json()).expect("metadata serializes") + "\n";
    let metadata_path = write_metadata(tmp, &json)?;
    moves.push((metadata_path, metadata_dest.clone()));

    job.save(ctx, tmp, &moves, embedding.relink, &media_dest)?;
    tracing::debug!(
        "{label}: saved as {} ({:.1}s)",
        media_dest.display(),
        started.elapsed().as_secs_f64()
    );

    let metadata = std::fs::read_to_string(&files.metadata).unwrap_or(json);
    Ok(Outcome::Done {
        files,
        metadata,
        media_bytes,
        linked: job.copy_from.is_some(),
    })
}

/// Clean up the title and run the `prepare` hooks on `info`. Returns the
/// paths the hooks set and the details they changed, as they were.
async fn prepare_details(
    job: &Job,
    ctx: &Context,
    stage: &stages::Stage<'_>,
    info: &mut VideoInfo,
    tmp: &Path,
    media: &Path,
    hook_runs: &mut Vec<hooks::Report>,
) -> Result<(BTreeMap<String, String>, Option<serde_json::Value>), Failure> {
    let original = prepare::Details::of(info);
    if job.source.clean_title != crate::config::CleanTitle::None {
        job.step("cleaning up the title");
    }
    stage.clean_title(info).await?;
    let overrides = stage
        .prepare_hooks(info, &job.paths.media_dir, &ctx.dirs, tmp, media, hook_runs)
        .await?;
    let originals = prepare::Details::of(info).originals(&original);
    Ok((overrides, originals))
}

/// The cover in `tmp`: the other source's when it's already in the wanted
/// format, or the biggest thumbnail. Returns the thumbnail fetched (none when
/// reused or not found) and the cover file.
async fn get_cover(
    job: &Job,
    ctx: &Context,
    tmp: &Path,
    label: &str,
    details: Option<&RemoteItem>,
    cover_format: CoverFormat,
) -> Result<(Option<Cover>, Option<PathBuf>), Failure> {
    let video_id = &job.queued.video_id;
    let cover_tmp = tmp.join(format!("cover.{}", cover_format.extension()));
    let reusable = job
        .copy_from
        .as_ref()
        .and_then(|other| other.cover.clone())
        .map(PathBuf::from)
        .filter(|c| {
            c.is_file()
                && c.extension()
                    .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))
                    == Some(cover_format)
        });
    if let Some(other_cover) = reusable {
        link(&other_cover, &cover_tmp, ctx.duplicates)
            .map_err(|e| io_fail("cannot link", &other_cover, &e))?;
        tracing::debug!("{label}: cover linked from {}", other_cover.display());
        return Ok((None, Some(cover_tmp)));
    }
    let thumbs = details.map(|d| d.thumbnails.clone()).unwrap_or_default();
    let urls = thumbnail::candidates(video_id, &thumbs, &ctx.thumbnail_base, cover_format);
    let Some(found) = thumbnail::fetch_largest(&ctx.http, &urls, cover_format).await else {
        tracing::warn!("{}: no cover image found for {video_id}", job.source.key);
        return Ok((None, None));
    };
    tracing::debug!(
        "{label}: cover {}×{} from {}",
        found.width,
        found.height,
        found.source_url
    );
    std::fs::write(&cover_tmp, &found.bytes)
        .map_err(|e| io_fail("cannot write", &cover_tmp, &e))?;
    Ok((Some(found), Some(cover_tmp)))
}

/// Where a video's files go.
struct Placement {
    media_dest: PathBuf,
    /// Each file in the work area and where it goes.
    moves: Vec<(PathBuf, PathBuf)>,
    /// The video's files, as recorded.
    files: items::VideoFiles,
}

/// Where each file in `raw` goes (the metadata file isn't written yet).
fn plan_moves(
    source: &EffectiveSource,
    layout: &Layout,
    raw: &hooks::Files,
    tmp: &Path,
    cover_dest: &Path,
    metadata_dest: &Path,
) -> Result<Placement, Failure> {
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
        moves.push((cover_tmp.clone(), cover_dest.to_path_buf()));
        files.cover = Some(cover_dest.display().to_string());
    }

    let raw_info = tmp.join("ytdlp.info.json");
    if source.keep_raw_info && raw_info.is_file() {
        let mut name = metadata_dest.file_stem().unwrap_or_default().to_owned();
        name.push(".ytdlp.json");
        let dest = metadata_dest.with_file_name(name);
        moves.push((raw_info, dest.clone()));
        files.raw_info = Some(dest.display().to_string());
    }
    Ok(Placement {
        media_dest,
        moves,
        files,
    })
}

/// Write `json` to `tmp/metadata.json`, synced to disk.
fn write_metadata(tmp: &Path, json: &str) -> Result<PathBuf, Failure> {
    let metadata_path = tmp.join("metadata.json");
    let mut file = std::fs::File::create(&metadata_path)
        .map_err(|e| io_fail("cannot write", &metadata_path, &e))?;
    file.write_all(json.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| io_fail("cannot write", &metadata_path, &e))?;
    Ok(metadata_path)
}

/// Linked duplicates share one file: the latest tags win for both.
fn relink_copy(media_dest: &Path, other: &items::VideoFiles, label: &str) {
    if let Err(e) = relink(media_dest, Path::new(&other.media)) {
        tracing::warn!(
            "{label}: cannot update the linked copy at {}: {e}",
            other.media
        );
    }
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

/// After a crash: interrupted videos go back (downloads to the queue,
/// processing to `fetched`), fetched downloads whose work area is gone are
/// downloaded again, and the work areas are cleared except for fetched
/// downloads. Only the folders tidepool creates are touched, so a `tmp_dir`
/// pointing somewhere shared never loses anything else.
///
/// Returns how many videos went back to the queue.
///
/// # Errors
///
/// Returns a message when the state database can't be read or updated, or
/// a work area can't be listed or cleared.
pub fn recover(state: &State, by_source: &BTreeMap<String, SourcePaths>) -> Result<usize, String> {
    let db = |e: rusqlite::Error| format!("state database: {e}");
    let mut reset = items::reset_interrupted(&state.conn).map_err(db)?;
    let keys: Vec<String> = by_source.keys().cloned().collect();
    for (source, video) in items::fetched(&state.conn, &keys).map_err(db)? {
        let work = work_area(&by_source[&source].tmp_root, &video, &source);
        if !work.join("fetched").is_dir() {
            items::set_status(&state.conn, &source, &video, status::PENDING).map_err(db)?;
            reset += 1;
        }
    }
    let mut tmp_roots: Vec<&PathBuf> = by_source.values().map(|p| &p.tmp_root).collect();
    tmp_roots.sort();
    tmp_roots.dedup();
    for tmp_root in tmp_roots.into_iter().filter(|t| t.is_dir()) {
        let entries = std::fs::read_dir(tmp_root)
            .map_err(|e| format!("cannot clear {}: {e}", tmp_root.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_work_folder(&name) || !entry.path().is_dir() {
                continue;
            }
            let Some((video, source)) = name.split_once('.') else {
                continue;
            };
            let fetched = items::item_statuses(&state.conn, video)
                .map_err(db)?
                .iter()
                .any(|(s, st)| s == source && st == status::FETCHED);
            let clear = if fetched {
                entry.path().join("process")
            } else {
                entry.path()
            };
            if clear.exists() {
                std::fs::remove_dir_all(&clear)
                    .map_err(|e| format!("cannot clear {}: {e}", clear.display()))?;
            }
        }
    }
    Ok(reset)
}

/// Download what's queued for `sources` and process what's downloaded,
/// at the same time: each download is processed as soon as it's done.
///
/// # Errors
///
/// Returns a message when a source's paths can't be worked out, recovering
/// or checking for missing files fails, or the state database can't be read
/// or updated.
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
    let mut by_source = BTreeMap::new();
    for source in sources {
        by_source.insert(
            source.key.clone(),
            paths(source).map_err(|e| format!("{}: {e}", source.key))?,
        );
    }
    if options.recover {
        recover(state, &by_source)?;
    }
    let mut summary = Summary::default();
    if options.fetch {
        summary.missing = missing::check(state, sources, &by_source, &ctx, out).await?;
    }

    let (queue, waiting) = queues(state, sources, &options).map_err(db)?;
    if queue.is_empty() && waiting.is_empty() {
        return Ok(summary);
    }

    let mut tasks = Tasks {
        ctx: Arc::new(ctx),
        fetch_slots: Arc::new(Semaphore::new(options.concurrency.max(1))),
        process_slots: Arc::new(Semaphore::new(options.process_concurrency.max(1))),
        fetches: JoinSet::new(),
        processes: JoinSet::new(),
    };
    let sources: Vec<Arc<EffectiveSource>> = sources.iter().cloned().map(Arc::new).collect();
    let source_of = |key: &str| sources.iter().find(|s| s.key == key).cloned();

    for queued in waiting {
        let Some(source) = source_of(&queued.source_key) else {
            continue;
        };
        let job = Job {
            paths: by_source[&queued.source_key].clone(),
            source,
            queued,
            copy_from: None,
            now,
        };
        tasks.spawn_process(state, job).map_err(db)?;
    }
    for queued in queue {
        let Some(source) = source_of(&queued.source_key) else {
            continue;
        };
        let copy_from = match tasks.ctx.duplicates {
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
        tasks.spawn_fetch(job);
    }

    tasks
        .run(state, options.process, now, &mut summary, tracker, out)
        .await
        .map_err(db)?;
    Ok(summary)
}

/// What a run downloads (sources with `download = false` are only indexed)
/// and what it processes; what those sources already downloaded still gets
/// finished.
fn queues(
    state: &State,
    sources: &[EffectiveSource],
    options: &Options,
) -> rusqlite::Result<(Vec<QueuedItem>, Vec<QueuedItem>)> {
    let keys: Vec<String> = sources.iter().map(|s| s.key.clone()).collect();
    let downloading: Vec<String> = sources
        .iter()
        .filter(|s| s.download)
        .map(|s| s.key.clone())
        .collect();
    let mut queue = if options.fetch {
        items::download_queue(&state.conn, &downloading, options.now)?
    } else {
        Vec::new()
    };
    if let Some(limit) = options.limit {
        queue.truncate(limit);
    }
    let waiting = if options.process {
        items::process_queue(&state.conn, &keys)?
    } else {
        Vec::new()
    };
    Ok((queue, waiting))
}

/// A run's downloads and processing, each waiting for a free slot.
struct Tasks {
    ctx: Arc<Context>,
    fetch_slots: Arc<Semaphore>,
    process_slots: Arc<Semaphore>,
    fetches: JoinSet<(Finished, Job)>,
    processes: JoinSet<Finished>,
}

impl Tasks {
    /// Download `job` once a download slot is free.
    fn spawn_fetch(&mut self, job: Job) {
        let (ctx, slots) = (self.ctx.clone(), self.fetch_slots.clone());
        self.fetches.spawn(
            async move {
                let (outcome, details) = tokio::select! {
                    permit = slots.acquire_owned() => match permit {
                        Ok(_permit) if !ctx.shutdown.is_stopping() => fetch(&job, &ctx).await,
                        _ => (Outcome::Stopped, None),
                    },
                    () = ctx.shutdown.stopping() => (Outcome::Stopped, None),
                };
                let f = finished(Phase::Fetch, &job, &ctx, outcome, details, Vec::new());
                (f, job)
            }
            .in_current_span(),
        );
    }

    /// Mark `job` as processing and process it once a slot is free.
    fn spawn_process(&mut self, state: &State, mut job: Job) -> rusqlite::Result<()> {
        items::set_status(
            &state.conn,
            &job.queued.source_key,
            &job.queued.video_id,
            status::PROCESSING,
        )?;
        let (ctx, slots) = (self.ctx.clone(), self.process_slots.clone());
        self.processes.spawn(
            async move {
                let mut hook_runs = Vec::new();
                let outcome = tokio::select! {
                    permit = slots.acquire_owned() => match permit {
                        Ok(_permit) if !ctx.shutdown.is_stopping() => {
                            process(&mut job, &ctx, &mut hook_runs).await.0
                        }
                        _ => Outcome::Stopped,
                    },
                    () = ctx.shutdown.stopping() => Outcome::Stopped,
                };
                finished(Phase::Process, &job, &ctx, outcome, None, hook_runs)
            }
            .in_current_span(),
        );
        Ok(())
    }

    /// Record each task as it ends, processing each download as soon as it's
    /// fetched (when `process`), until every task is done or stopping starts.
    async fn run(
        &mut self,
        state: &State,
        process: bool,
        now: Timestamp,
        summary: &mut Summary,
        tracker: &mut crate::problems::Tracker,
        out: &mut dyn Write,
    ) -> rusqlite::Result<()> {
        loop {
            if self.fetches.is_empty() && self.processes.is_empty() {
                break;
            }
            tokio::select! {
                Some(done) = self.fetches.join_next(), if !self.fetches.is_empty() => {
                    let Ok((f, job)) = done else { continue };
                    let was_fetched = matches!(f.outcome, Outcome::Fetched { .. });
                    record(state, &f, now, summary, tracker, out)?;
                    if was_fetched {
                        if process {
                            self.spawn_process(state, job)?;
                        } else if let Some(waiting) = &self.ctx.fetched {
                            waiting.notify_one();
                        }
                    }
                }
                Some(done) = self.processes.join_next(), if !self.processes.is_empty() => {
                    let Ok(f) = done else { continue };
                    record(state, &f, now, summary, tracker, out)?;
                }
                () = self.ctx.shutdown.stopping() => {
                    // Running processes are being stopped; wait for every task.
                    // What finished in the meantime still counts; the rest goes
                    // back to where it was.
                    while let Some(done) = self.fetches.join_next().await {
                        if let Ok((f, _)) = done {
                            record_stopping(state, f, now, summary, tracker, out)?;
                        }
                    }
                    while let Some(done) = self.processes.join_next().await {
                        if let Ok(f) = done {
                            record_stopping(state, f, now, summary, tracker, out)?;
                        }
                    }
                    summary.interrupted = self.ctx.shutdown.code();
                    break;
                }
            }
        }
        Ok(())
    }
}

/// What a task did, for `record`.
fn finished(
    phase: Phase,
    job: &Job,
    ctx: &Context,
    outcome: Outcome,
    details: Option<RemoteItem>,
    hook_runs: Vec<hooks::Report>,
) -> Finished {
    let title = details
        .as_ref()
        .map_or_else(|| job.queued.item.title.clone(), |d| d.title.clone());
    Finished {
        phase,
        previous_attempts: job.queued.attempts,
        source_key: job.queued.source_key.clone(),
        format: job.source.format.clone(),
        video_id: job.queued.video_id.clone(),
        title,
        outcome,
        details,
        hook_runs,
        pending_hooks: hooks::of_type(&job.source.hooks, &ctx.hooks, HookType::Finished)
            .map(|(name, _)| name.to_string())
            .collect(),
    }
}

/// While stopping: what finished counts; anything cut short goes back to
/// where it was (downloads to the queue, processing to `fetched`).
fn record_stopping(
    state: &State,
    mut f: Finished,
    now: Timestamp,
    summary: &mut Summary,
    tracker: &mut crate::problems::Tracker,
    out: &mut dyn Write,
) -> rusqlite::Result<()> {
    if let Outcome::Failed(_) | Outcome::Skipped(_) = f.outcome {
        f.outcome = Outcome::Stopped;
        f.hook_runs.clear();
    }
    record(state, &f, now, summary, tracker, out)
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
            Err(e) => tracker.hook_failed(&f.source_key, &report.name, e),
        }
    }
    match &f.outcome {
        Outcome::Fetched { bytes } => {
            items::mark_fetched(&state.conn, &f.source_key, &f.video_id, now)?;
            crate::events::emit(crate::events::Event::Fetched {
                source: f.source_key.clone(),
                video_id: f.video_id.clone(),
                title: f.title.clone(),
            });
            summary.fetched += 1;
            tracker.download_succeeded();
            tracing::debug!(
                "{}: fetched “{}” ({})",
                f.source_key,
                f.title,
                human_bytes(*bytes)
            );
        }
        Outcome::Done {
            files,
            metadata,
            media_bytes,
            linked,
        } => {
            let done = items::Done {
                files,
                metadata,
                format: &f.format,
                media_bytes: *media_bytes,
            };
            items::mark_done(&state.conn, &f.source_key, &f.video_id, done, now)?;
            items::queue_hooks(
                &state.conn,
                &f.source_key,
                &f.video_id,
                &f.pending_hooks,
                now,
            )?;
            summary.downloaded += 1;
            tracker.download_succeeded();
            summary.bytes += media_bytes;
            report_done(f, *media_bytes, *linked, out);
        }
        Outcome::Skipped(decision) => {
            items::record_decision(&state.conn, &f.source_key, &f.video_id, decision)?;
            crate::events::emit(crate::events::Event::Stopped {
                source: f.source_key.clone(),
                video_id: f.video_id.clone(),
            });
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
            report_failure(f, failure);
        }
        Outcome::Stopped => {
            crate::events::emit(crate::events::Event::Stopped {
                source: f.source_key.clone(),
                video_id: f.video_id.clone(),
            });
            let back = match f.phase {
                Phase::Fetch => status::PENDING,
                Phase::Process => status::FETCHED,
            };
            items::set_status(&state.conn, &f.source_key, &f.video_id, back)?;
        }
    }
    Ok(())
}

/// Tell the live panel and `out` that a video was saved.
fn report_done(f: &Finished, media_bytes: u64, linked: bool, out: &mut dyn Write) {
    crate::events::emit(crate::events::Event::Saved {
        source: f.source_key.clone(),
        video_id: f.video_id.clone(),
        title: f.title.clone(),
    });
    let how = if linked {
        "linked from another source"
    } else {
        "downloaded"
    };
    let _ = writeln!(
        out,
        "{}: {how} “{}” ({})",
        f.source_key,
        f.title,
        human_bytes(media_bytes)
    );
}

/// Tell the live panel and the log that a video failed.
fn report_failure(f: &Finished, failure: &Failure) {
    crate::events::emit(crate::events::Event::Failed {
        source: f.source_key.clone(),
        video_id: f.video_id.clone(),
        title: f.title.clone(),
        error: failure.message.clone(),
    });
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
