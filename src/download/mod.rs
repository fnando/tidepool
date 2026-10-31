//! Downloading: run yt-dlp into a temp dir, add the cover and metadata.json,
//! and move the result into its media folder in one rename.

pub mod info;
pub mod layout;
pub mod metadata;
pub mod thumbnail;
pub mod ytdlp;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::Timestamp;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Duplicates;
use crate::config::merge::EffectiveSource;
use crate::filter::{Decision, Fields, RuleSet};
use crate::index::RemoteItem;
use crate::state::State;
use crate::state::items::{self, QueuedItem, status};
use info::VideoInfo;
use layout::Values;
use metadata::{Files, Metadata};
use thumbnail::Cover;
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
    /// For `~` and `{data_dir}` in `output`.
    pub dirs: crate::paths::Dirs,
    pub ytdlp: PathBuf,
    pub ytdlp_version: Option<String>,
    pub duplicates: Duplicates,
    pub thumbnail_base: String,
    pub backtrack_limit: usize,
    pub http: reqwest::Client,
    /// Grace period for yt-dlp after SIGTERM (`general.shutdown_timeout`).
    pub shutdown_timeout: std::time::Duration,
    pub cancel: CancellationToken,
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
    /// Exit code when a signal stopped the run (130 for SIGINT, 143 for SIGTERM).
    pub interrupted: Option<i32>,
}

struct Job {
    source: Arc<EffectiveSource>,
    queued: QueuedItem,
    paths: SourcePaths,
    /// Another source's directory (absolute) holding the same video in the same format.
    copy_from: Option<String>,
    now: Timestamp,
}

enum Outcome {
    Done {
        dir: String,
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

/// Move the finished temp dir into place; an existing directory is moved
/// aside first and removed after the rename succeeds.
fn commit(tmp: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A work area on another filesystem can't be renamed into place: copy it
    // next to the target first, so the target still appears in one rename.
    let staged = staging_path(target);
    match std::fs::rename(tmp, &staged) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let _ = std::fs::remove_dir_all(&staged);
            if let Err(e) = copy_dir(tmp, &staged) {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
            std::fs::remove_dir_all(tmp)?;
        }
        Err(e) => return Err(e),
    }
    let aside = target.exists().then(|| {
        let mut name = target.as_os_str().to_owned();
        name.push(format!(".replaced-{}", std::process::id()));
        PathBuf::from(name)
    });
    if let Some(aside) = &aside {
        std::fs::rename(target, aside)?;
    }
    std::fs::rename(&staged, target)?;
    if let Some(aside) = &aside {
        let _ = std::fs::remove_dir_all(aside);
    }
    Ok(())
}

/// The hidden folder next to `target` a download is staged in before its
/// final rename.
fn staging_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.tidepool-partial"))
}

/// Copy a directory tree (files and folders only).
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
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

async fn process(job: &Job, ctx: &Context) -> (Outcome, Option<RemoteItem>) {
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
    if let Some(dir) = &job.copy_from {
        tracing::debug!("{label}: already downloaded in {dir}; linking its files");
    }

    // A decision that needed fields the index didn't have.
    let mut info = None;
    if job.queued.status == status::PENDING_CHECK || job.copy_from.is_some() {
        match ytdlp::video_info(&ctx.ytdlp, video_id, job.paths.cookies.as_deref()).await {
            Ok(full) => {
                let merged = merge_details(item, &full);
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
    let result = assemble(job, ctx, &tmp, info, &mut details, &label, started).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    match result {
        Ok(outcome) => (outcome, details),
        Err(failure) => (Outcome::Failed(failure), details),
    }
}

/// Fill `tmp` with the media, cover and metadata.json, then commit it.
async fn assemble(
    job: &Job,
    ctx: &Context,
    tmp: &Path,
    info: Option<VideoInfo>,
    details: &mut Option<RemoteItem>,
    label: &str,
    started: std::time::Instant,
) -> Result<Outcome, Failure> {
    let source = &job.source;
    let video_id = &job.queued.video_id;

    // Media: linked from another source, or downloaded.
    let (info, media_name, other) = match (&job.copy_from, info) {
        (Some(other_dir), Some(info)) => {
            let other = PathBuf::from(other_dir);
            let previous: serde_json::Value = std::fs::read_to_string(other.join("metadata.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .ok_or_else(|| fail(format!("cannot read {}/metadata.json", other.display())))?;
            let media = previous["files"]["media"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let mut names = vec![media.clone()];
            names.extend(
                previous["files"]["subtitles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .map(String::from),
            );
            for name in names.iter().filter(|n| !n.is_empty()) {
                link(&other.join(name), &tmp.join(name), ctx.duplicates)
                    .map_err(|e| io_fail("cannot link", &other.join(name), e))?;
            }
            let cover = previous["files"]["cover"].as_str().map(|c| other.join(c));
            (info, media, cover)
        }
        _ => {
            let args = ytdlp::download_args(source, video_id, tmp, job.paths.cookies.as_deref());
            let media =
                ytdlp::download(&ctx.ytdlp, &args, label, &ctx.cancel, ctx.shutdown_timeout)
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
            (info, name, None)
        }
    };

    let values = Values::new(
        &info,
        source,
        job.queued.source_title.as_deref(),
        job.queued.position,
    );
    let target =
        layout::video_dir(source, &values, &job.paths.media_dir, &ctx.dirs).map_err(fail)?;

    // Cover: reuse the other source's, or fetch the biggest.
    let mut cover: Option<Cover> = None;
    let mut cover_name = None;
    if let Some(other_cover) = other.filter(|c| c.is_file()) {
        let ext = other_cover
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg")
            .to_string();
        let name = layout::cover_file_name(source, &values, &ext).map_err(fail)?;
        link(&other_cover, &tmp.join(&name), ctx.duplicates)
            .map_err(|e| io_fail("cannot link", &other_cover, e))?;
        tracing::debug!("{label}: cover linked from {}", other_cover.display());
        cover_name = Some(name);
    } else {
        let thumbs = details
            .as_ref()
            .map(|d| d.thumbnails.clone())
            .unwrap_or_default();
        let urls = thumbnail::candidates(
            video_id,
            &thumbs,
            &ctx.thumbnail_base,
            source.thumbnail_format,
        );
        match thumbnail::fetch_largest(&ctx.http, &urls, source.thumbnail_format).await {
            Some(found) => {
                tracing::debug!(
                    "{label}: cover {}×{} from {}",
                    found.width,
                    found.height,
                    found.source_url
                );
                let name =
                    layout::cover_file_name(source, &values, found.extension).map_err(fail)?;
                let path = tmp.join(&name);
                std::fs::write(&path, &found.bytes)
                    .map_err(|e| io_fail("cannot write", &path, e))?;
                cover_name = Some(name);
                cover = Some(found);
            }
            None => tracing::warn!("{}: no cover image found for {video_id}", source.key),
        }
    }

    let mut subtitles: Vec<String> = std::fs::read_dir(tmp)
        .map_err(|e| io_fail("cannot read", tmp, e))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("media.")
                && name
                    .rsplit('.')
                    .next()
                    .is_some_and(|ext| SUBTITLE_EXTENSIONS.contains(&ext))
        })
        .collect();
    subtitles.sort();

    let media_path = tmp.join(&media_name);
    let media_bytes = std::fs::metadata(&media_path)
        .map_err(|e| io_fail("cannot read", &media_path, e))?
        .len();
    let files = Files {
        media: media_name,
        cover: cover_name,
        subtitles,
    };
    let metadata = Metadata {
        info: &info,
        source,
        source_title: job.queued.source_title.as_deref(),
        position: job.queued.position,
        files: &files,
        media_bytes,
        cover: cover.as_ref(),
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

    commit(tmp, &target).map_err(|e| io_fail("cannot move the download into", &target, e))?;
    tracing::debug!(
        "{label}: moved into {} ({:.1}s)",
        target.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(Outcome::Done {
        dir: target.to_string_lossy().into_owned(),
        media_bytes,
        linked: job.copy_from.is_some(),
    })
}

/// Wait for SIGINT or SIGTERM; returns the exit code to use.
async fn interrupted() -> i32 {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => 130,
        _ = term.recv() => 143,
    }
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

    let keys: Vec<String> = sources.iter().map(|s| s.key.clone()).collect();
    let mut queue = items::download_queue(&state.conn, &keys, now).map_err(db)?;
    if let Some(limit) = options.limit {
        queue.truncate(limit);
    }
    let mut summary = Summary::default();
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
                .filter(|dir| Path::new(dir).join("metadata.json").is_file()),
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
                _ = ctx.cancel.cancelled() => return None,
            };
            if ctx.cancel.is_cancelled() {
                return None;
            }
            let (outcome, details) = process(&job, &ctx).await;
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
            })
        });
    }

    let signal = interrupted();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            finished = tasks.join_next() => {
                let Some(finished) = finished else { break };
                let Ok(Some(f)) = finished else { continue };
                record(state, &f, now, &mut summary, tracker, out).map_err(db)?;
            }
            code = &mut signal => {
                // Stop yt-dlp gracefully and wait for every task, so nothing
                // writes to the work area while it's cleared.
                ctx.cancel.cancel();
                while tasks.join_next().await.is_some() {}
                recover(state, &tmp_roots)?;
                summary.interrupted = Some(code);
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
    match &f.outcome {
        Outcome::Done {
            dir,
            media_bytes,
            linked,
        } => {
            items::mark_done(
                &state.conn,
                &f.source_key,
                &f.video_id,
                dir,
                &f.format,
                *media_bytes,
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
    fn commit_moves_into_place_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("work/dQw4w9WgXcQ.lofi");
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub/a.txt"), "new").unwrap();
        let target = dir.path().join("media/lofi/video [dQw4w9WgXcQ]");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("old.txt"), "old").unwrap();
        commit(&tmp, &target).unwrap();
        assert!(!tmp.exists());
        assert_eq!(
            std::fs::read_to_string(target.join("sub/a.txt")).unwrap(),
            "new"
        );
        assert!(!target.join("old.txt").exists());
        let names: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "no staging or replaced folders left: {names:?}"
        );

        let copied = dir.path().join("copy");
        copy_dir(&target, &copied).unwrap();
        assert_eq!(
            std::fs::read_to_string(copied.join("sub/a.txt")).unwrap(),
            "new"
        );
    }
}
