//! What the web UI and its JSON API show, read from the state DB.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rusqlite::Connection;
use serde::Serialize;

use crate::config::Config;
use crate::config::merge::effective_sources;
use crate::daemon::service::{Service, local};
use crate::download::human_bytes;
use crate::state::daemon as store;

/// Everything on the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// `general.download`: the default for sources.
    pub download: bool,
    pub services: Vec<ServiceInfo>,
    /// When the syncer syncs next (local time), if it's on a schedule.
    pub next_sync: Option<String>,
    pub last_sync: Option<String>,
    pub sources: Vec<SourceInfo>,
    pub problems: Vec<ProblemInfo>,
    /// Downloads waiting to be finished.
    pub waiting_downloads: i64,
    /// `finished` hooks waiting to run (or to be tried again).
    pub waiting_hooks: i64,
    /// Whether indexing uses the YouTube API, and how much of it today.
    pub youtube_api: YoutubeApi,
}

/// What the sources' YouTube API keys spent today.
fn youtube_api(conn: &Connection, config: &Config) -> rusqlite::Result<YoutubeApi> {
    let mut api = YoutubeApi::default();
    let mut seen = std::collections::BTreeSet::new();
    let day = crate::state::items::quota_day(jiff::Timestamp::now());
    for source in effective_sources(config) {
        match source.youtube.api_key.as_deref() {
            None => api.keyless += 1,
            Some(key) => {
                let id = crate::pipeline::key_id(key);
                if seen.insert(id.clone()) {
                    let used = crate::state::items::quota_used(conn, &id, &day)?;
                    api.keys.push((used, source.youtube.daily_quota));
                }
            }
        }
    }
    Ok(api)
}

/// The YouTube API keys the sources use, and what they spent today.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct YoutubeApi {
    /// Units spent today and the daily quota, per distinct key.
    pub keys: Vec<(u32, u32)>,
    /// Sources without a key: yt-dlp indexes them.
    pub keyless: usize,
}

impl YoutubeApi {
    /// Something spent units today.
    #[must_use]
    pub fn used(&self) -> bool {
        self.keys.iter().any(|(used, _)| *used > 0)
    }

    /// e.g. "120 of 10000 units used today", "key set, not used today".
    #[must_use]
    pub fn summary(&self) -> String {
        let usage = match self.keys.as_slice() {
            [] => return "no key, so yt-dlp indexes".into(),
            [(0, _)] => "key set, not used today".to_string(),
            [(used, quota)] => format!("{used} of {quota} units used today"),
            keys => {
                let used: u32 = keys.iter().map(|(used, _)| used).sum();
                format!("{} keys, {used} units used today", keys.len())
            }
        };
        match self.keyless {
            0 => usage,
            1 => format!("{usage}; 1 source has no key"),
            n => format!("{usage}; {n} sources have no key"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceInfo {
    pub name: String,
    pub status: String,
    pub activity: Option<String>,
    pub paused: bool,
    pub restarts: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub name: String,
    pub title: Option<String>,
    pub category: String,
    pub enabled: bool,
    /// Its videos are downloaded (else it's only checked).
    pub download: bool,
    pub last_indexed: Option<String>,
    /// Videos per group: downloaded, waiting, skipped, failed, unavailable,
    /// `cleaned_up`.
    pub counts: BTreeMap<String, i64>,
    pub disk_bytes: u64,
    pub disk: String,
    /// `cleanup.max_size`, if set.
    pub max_size: Option<String>,
}

impl SourceInfo {
    #[must_use]
    pub fn count(&self, group: &str) -> i64 {
        self.counts.get(group).copied().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProblemInfo {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub text: String,
}

/// The groups a video's status falls in, as the dashboard counts them.
#[must_use]
pub fn group(status: &str) -> &'static str {
    match status {
        "done" => "downloaded",
        "pending" | "pending_check" | "downloading" | "fetched" | "processing" => "waiting",
        "filtered" | "filter_error" => "skipped",
        "failed" => "failed",
        "unavailable" => "unavailable",
        "cleaned_up" => "cleaned_up",
        _ => "other",
    }
}

/// What the status page shows: each service, and each source's counts.
///
/// # Errors
///
/// Fails if a database query fails.
pub fn status(conn: &Connection, config: &Config) -> rusqlite::Result<Status> {
    let services = Service::ALL
        .into_iter()
        .map(|service| {
            let row = store::service(conn, service.name())?;
            Ok(ServiceInfo {
                name: service.name().to_string(),
                status: if row.paused {
                    "paused".into()
                } else {
                    row.status.unwrap_or_else(|| "not running".into())
                },
                activity: row.activity,
                paused: row.paused,
                restarts: row.restarts,
                last_error: row.last_error,
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next_sync = config
        .schedule
        .interval
        .0
        .and_then(|_| store::get_time(conn, store::NEXT_SYNC_AT).ok().flatten())
        .map(local);
    let last_sync = store::get_time(conn, store::LAST_SYNC_FINISHED_AT)?.map(local);

    let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut disk: BTreeMap<String, u64> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT source_key, status, COUNT(*), SUM(COALESCE(media_bytes, 0)) FROM items
             GROUP BY source_key, status",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (source, status, n, bytes) = row?;
            *counts
                .entry(source.clone())
                .or_default()
                .entry(group(&status).to_string())
                .or_default() += n;
            if status == "done" {
                *disk.entry(source).or_default() += u64::try_from(bytes).unwrap_or(0);
            }
        }
    }
    let mut sources = Vec::new();
    for source in effective_sources(config) {
        let (title, last_indexed): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, last_indexed_at FROM sources WHERE key = ?1",
                [&source.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None));
        let bytes = disk.get(&source.key).copied().unwrap_or(0);
        sources.push(SourceInfo {
            title,
            category: source.category.clone(),
            enabled: source.enabled,
            download: source.download,
            last_indexed: last_indexed.and_then(|t| t.parse().ok()).map(local),
            counts: counts.remove(&source.key).unwrap_or_default(),
            disk_bytes: bytes,
            disk: human_bytes(bytes),
            max_size: source.cleanup.max_size.bytes.map(human_bytes),
            name: source.key,
        });
    }

    let problems = crate::problems::open(conn)?
        .into_iter()
        .map(|p| ProblemInfo {
            id: p.id,
            kind: p.kind.as_str().to_string(),
            title: p.kind.title().to_string(),
            text: crate::problems::describe(&p),
        })
        .collect();
    let count = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
    Ok(Status {
        download: config.general.download,
        services,
        next_sync,
        last_sync,
        sources,
        problems,
        waiting_downloads: count(
            "SELECT COUNT(*) FROM items WHERE status IN ('fetched', 'processing')",
        )?,
        waiting_hooks: count(
            "SELECT COUNT(*) FROM hook_runs WHERE type = 'finished' AND status IN ('pending', 'retrying')",
        )?,
        youtube_api: youtube_api(conn, config)?,
    })
}

/// Statuses in each group the source page filters by.
pub const GROUPS: [(&str, &str); 6] = [
    ("downloaded", "Downloaded"),
    ("waiting", "Waiting"),
    ("skipped", "Skipped"),
    ("failed", "Failed"),
    ("unavailable", "Unavailable"),
    ("cleaned_up", "Cleaned up"),
];

fn statuses_of(group: &str) -> &'static [&'static str] {
    match group {
        "downloaded" => &["done"],
        "waiting" => &[
            "pending",
            "pending_check",
            "downloading",
            "fetched",
            "processing",
        ],
        "skipped" => &["filtered", "filter_error"],
        "failed" => &["failed"],
        "unavailable" => &["unavailable"],
        "cleaned_up" => &["cleaned_up"],
        _ => &[],
    }
}

/// One video in a source's list.
#[derive(Debug, Clone, Serialize)]
pub struct VideoRow {
    pub video_id: String,
    pub title: String,
    pub status: String,
    /// `status` as the badge says it, and its CSS class.
    pub label: String,
    pub badge: String,
    /// Why it's in that status: the rule that dropped it, the error, …
    pub why: Option<String>,
    /// Published (`YYYY-MM-DD`).
    pub date: Option<String>,
    pub size: Option<String>,
    /// The title `clean_title` gives it, when that differs.
    pub clean_title: Option<String>,
}

/// Why a video is where it is, in words.
fn why(
    status: &str,
    decision: Option<&str>,
    error: Option<String>,
    cleanup: Option<String>,
) -> Option<String> {
    match status {
        "filtered" | "filter_error" | "pending_check" => decision
            .and_then(crate::filter::Decision::from_json)
            .map(|d| d.to_string()),
        "failed" | "unavailable" => error,
        "cleaned_up" => cleanup,
        "fetched" | "processing" => Some("downloaded, being finished".into()),
        _ => None,
    }
}

/// A page of `source`'s videos: in `group` (all when `None`), with
/// `query` in the title, in source order. Returns the rows and how many
/// match in all.
///
/// # Errors
///
/// Fails if a database query fails.
pub fn videos(
    conn: &Connection,
    source: &str,
    group: Option<&str>,
    query: Option<&str>,
    page: usize,
    per_page: usize,
) -> rusqlite::Result<(Vec<VideoRow>, usize)> {
    let mut filter = String::from("i.source_key = ?1");
    if let Some(group) = group {
        let list: Vec<String> = statuses_of(group)
            .iter()
            .map(|s| format!("'{s}'"))
            .collect();
        let list = if list.is_empty() {
            "''".to_string()
        } else {
            list.join(", ")
        };
        let _ = write!(filter, " AND i.status IN ({list})");
    }
    let like = query.map(str::trim).filter(|q| !q.is_empty()).map(|q| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });
    if like.is_some() {
        filter.push_str(" AND (v.title LIKE ?2 ESCAPE '\\' OR i.video_id LIKE ?2 ESCAPE '\\')");
    }
    let from = format!("FROM items i JOIN videos v ON v.id = i.video_id WHERE {filter}");
    let params: Vec<&dyn rusqlite::ToSql> = match &like {
        Some(like) => vec![&source, like],
        None => vec![&source],
    };
    let total: i64 = conn.query_row(&format!("SELECT COUNT(*) {from}"), &params[..], |r| {
        r.get(0)
    })?;
    let mut stmt = conn.prepare(&format!(
        "SELECT i.video_id, v.title, i.status, i.decision, i.last_error, i.cleanup_reason,
                v.published_at, i.media_bytes
         {from} ORDER BY i.position LIMIT {per_page} OFFSET {}",
        page.saturating_sub(1) * per_page
    ))?;
    let rows = stmt
        .query_map(&params[..], |r| {
            let status: String = r.get(2)?;
            let decision: Option<String> = r.get(3)?;
            let published: Option<String> = r.get(6)?;
            let bytes: Option<i64> = r.get(7)?;
            Ok(VideoRow {
                video_id: r.get(0)?,
                title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                why: why(&status, decision.as_deref(), r.get(4)?, r.get(5)?),
                date: published.map(|p| p.chars().take(10).collect()),
                size: bytes
                    .filter(|_| status == "done")
                    .map(|b| human_bytes(u64::try_from(b).unwrap_or(0))),
                clean_title: None,
                label: status_label(&status),
                badge: status_badge(&status),
                status,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((rows, usize::try_from(total).unwrap_or(0)))
}

/// A `finished`/other hook's latest run for a video.
#[derive(Debug, Clone, Serialize)]
pub struct HookRunRow {
    pub hook: String,
    pub kind: String,
    pub status: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub next_attempt_at: Option<String>,
}

/// Everything the video page shows.
#[derive(Debug, Clone, Serialize)]
pub struct VideoDetail {
    pub source: String,
    pub video_id: String,
    pub title: String,
    pub status: String,
    /// `status` as the badge says it, and its CSS class.
    pub label: String,
    pub badge: String,
    pub why: Option<String>,
    /// Each rule's verdict, as `tidepool filter test` shows it.
    pub explanation: String,
    pub files: Vec<String>,
    pub hook_runs: Vec<HookRunRow>,
    pub in_trash: bool,
    /// The title `clean_title` gives it, when that differs.
    pub clean_title: Option<String>,
}

/// `None` when `source` doesn't have the video.
///
/// # Errors
///
/// Fails if a database query fails or a source's filters don't compile.
pub fn video(
    conn: &Connection,
    config: &Config,
    source: &str,
    video_id: &str,
) -> anyhow::Result<Option<VideoDetail>> {
    use rusqlite::OptionalExtension;
    let Some((title, status, decision, error, cleanup, files)) = conn
        .query_row(
            "SELECT v.title, i.status, i.decision, i.last_error, i.cleanup_reason, i.files
             FROM items i JOIN videos v ON v.id = i.video_id
             WHERE i.source_key = ?1 AND i.video_id = ?2",
            [source, video_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut explanation = Vec::new();
    crate::app::explain_video(conn, config, video_id, Some(source), &mut explanation)?;
    let files = files
        .and_then(|f| serde_json::from_str::<crate::state::items::VideoFiles>(&f).ok())
        .map(|f| f.paths().into_iter().map(str::to_string).collect())
        .unwrap_or_default();
    let mut stmt = conn.prepare(
        "SELECT hook, type, status, finished_at, error, next_attempt_at FROM hook_runs
         WHERE source_key = ?1 AND video_id = ?2 ORDER BY type, hook",
    )?;
    let hook_runs = stmt
        .query_map([source, video_id], |r| {
            Ok(HookRunRow {
                hook: r.get(0)?,
                kind: r.get(1)?,
                status: r.get(2)?,
                finished_at: r.get(3)?,
                error: r.get(4)?,
                next_attempt_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let in_trash: i64 = conn.query_row(
        "SELECT COUNT(*) FROM trash WHERE source_key = ?1 AND video_id = ?2",
        [source, video_id],
        |r| r.get(0),
    )?;
    Ok(Some(VideoDetail {
        source: source.to_string(),
        video_id: video_id.to_string(),
        title: title.unwrap_or_default(),
        why: why(&status, decision.as_deref(), error, cleanup),
        label: status_label(&status),
        badge: status_badge(&status),
        status,
        explanation: String::from_utf8_lossy(&explanation).trim().to_string(),
        files,
        hook_runs,
        in_trash: in_trash > 0,
        clean_title: None,
    }))
}

/// The last `lines` lines of a video's hook log, if it has one.
#[must_use]
pub fn log_tail(path: &std::path::Path, lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let all: Vec<&str> = text.lines().collect();
    Some(all[all.len().saturating_sub(lines)..].join("\n"))
}

/// The title `clean_title = "music_video"` gives `title`, when it differs.
#[must_use]
pub fn clean_title(title: &str, artist: &str) -> Option<String> {
    let cleaned = crate::titles::music_video(title, artist);
    (cleaned != title).then_some(cleaned)
}

/// How a video's status reads on a badge.
#[must_use]
pub fn status_label(status: &str) -> String {
    match status {
        "filtered" => "excluded".into(),
        other => other.replace('_', " "),
    }
}

/// The badge's CSS class (`st-<this>`).
#[must_use]
pub fn status_badge(status: &str) -> String {
    match status {
        "filtered" => "excluded".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::YoutubeApi;

    #[test]
    fn says_whether_the_youtube_api_is_used() {
        let api = |keys: Vec<(u32, u32)>, keyless| YoutubeApi { keys, keyless };
        let cases = [
            (api(vec![], 2), "no key, so yt-dlp indexes", false),
            (api(vec![(0, 10_000)], 0), "key set, not used today", false),
            (
                api(vec![(120, 10_000)], 1),
                "120 of 10000 units used today; 1 source has no key",
                true,
            ),
            (
                api(vec![(5, 10_000), (0, 500)], 3),
                "2 keys, 5 units used today; 3 sources have no key",
                true,
            ),
        ];
        for (api, summary, used) in cases {
            assert_eq!((api.summary().as_str(), api.used()), (summary, used));
        }
    }
}
