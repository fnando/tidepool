//! Recording what indexing found: sources, videos and each video's status in
//! a source.

use std::collections::{BTreeMap, HashSet};

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use serde::{Deserialize, Serialize};

use crate::filter::Decision;
use crate::index::{RemoteItem, RemoteList};

/// Item statuses.
pub mod status {
    pub const PENDING: &str = "pending";
    pub const PENDING_CHECK: &str = "pending_check";
    pub const FILTERED: &str = "filtered";
    pub const FILTER_ERROR: &str = "filter_error";
    pub const DOWNLOADING: &str = "downloading";
    pub const DOWNLOADED: &str = "downloaded";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const CLEANED_UP: &str = "cleaned_up";

    /// Statuses whose decision is recomputed on every index.
    pub const UNDECIDED: [&str; 4] = [PENDING, PENDING_CHECK, FILTERED, FILTER_ERROR];
}

/// The status a filter decision puts an item in.
pub fn status_for(decision: &Decision) -> &'static str {
    match decision {
        Decision::Keep { .. } => status::PENDING,
        Decision::Deferred { .. } => status::PENDING_CHECK,
        Decision::FilterError { .. } => status::FILTER_ERROR,
        Decision::Excluded { .. } | Decision::NotIncluded | Decision::Structural { .. } => {
            status::FILTERED
        }
    }
}

/// What one index of a source changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexSummary {
    /// Videos currently in the source.
    pub total: usize,
    pub new: usize,
    /// Videos that were in the source last time and aren't anymore.
    pub removed: usize,
    /// Status counts over videos currently in the source.
    pub by_status: BTreeMap<String, usize>,
    /// Why videos were filtered, by [`Decision::label`].
    pub filtered: BTreeMap<String, usize>,
    /// Every decision made in this index, for verbose output.
    pub decisions: Vec<DecisionChange>,
}

/// One video's decision in an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionChange {
    pub video_id: String,
    pub title: String,
    /// First time this source saw the video.
    pub new: bool,
    /// The decision differs from the stored one.
    pub changed: bool,
    /// Human-readable decision, e.g. `drop (too short: 30s < 60s)`.
    pub decision: String,
}

impl IndexSummary {
    pub fn count(&self, status: &str) -> usize {
        self.by_status.get(status).copied().unwrap_or(0)
    }
}

/// Where a source's index came from.
pub struct SourceRecord<'a> {
    pub key: &'a str,
    pub rules_hash: &'a str,
    pub index_source: &'a str,
}

/// Record an index of `source` in one transaction. `decide` is applied to new
/// items and to items that haven't been downloaded yet.
pub fn record_index(
    conn: &mut Connection,
    source: &SourceRecord<'_>,
    list: &RemoteList,
    decide: impl Fn(&RemoteItem) -> Decision,
    now: Timestamp,
) -> rusqlite::Result<IndexSummary> {
    let now = now.to_string();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO sources (key, youtube_id, title, rules_hash, last_indexed_at, last_index_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(key) DO UPDATE SET youtube_id = excluded.youtube_id,
           title = COALESCE(excluded.title, sources.title), rules_hash = excluded.rules_hash,
           last_indexed_at = excluded.last_indexed_at, last_index_source = excluded.last_index_source",
        params![source.key, list.id, list.title, source.rules_hash, now, source.index_source],
    )?;

    let mut summary = IndexSummary {
        total: list.items.len(),
        ..IndexSummary::default()
    };
    let mut seen = HashSet::new();
    for item in &list.items {
        if !seen.insert(item.video_id.as_str()) {
            continue; // the same video twice in one playlist: first position wins
        }
        upsert_video(&tx, item, &now)?;

        let existing: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT status, decision FROM items WHERE source_key = ?1 AND video_id = ?2",
                params![source.key, item.video_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let previous_decision = existing.as_ref().and_then(|(_, d)| d.clone());
        let existing = existing.map(|(status, _)| status);
        let decide_now = existing
            .as_deref()
            .is_none_or(|s| status::UNDECIDED.contains(&s));
        if existing.is_none() {
            summary.new += 1;
        }
        if decide_now {
            let decision = decide(item);
            summary.decisions.push(DecisionChange {
                video_id: item.video_id.clone(),
                title: item.title.clone(),
                new: existing.is_none(),
                changed: existing.is_some()
                    && previous_decision.as_deref() != Some(decision.to_json().as_str()),
                decision: decision.to_string(),
            });
            tx.execute(
                "INSERT INTO items (source_key, video_id, position, in_remote, status, decision)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5)
                 ON CONFLICT(source_key, video_id) DO UPDATE SET position = excluded.position,
                   in_remote = 1, status = excluded.status, decision = excluded.decision",
                params![
                    source.key,
                    item.video_id,
                    item.position,
                    status_for(&decision),
                    decision.to_json()
                ],
            )?;
            if status_for(&decision) == status::FILTERED {
                *summary
                    .filtered
                    .entry(decision.label().to_string())
                    .or_default() += 1;
            }
        } else {
            tx.execute(
                "UPDATE items SET position = ?3, in_remote = 1 WHERE source_key = ?1 AND video_id = ?2",
                params![source.key, item.video_id, item.position],
            )?;
        }
    }
    summary.total = seen.len();

    // Whatever wasn't seen has left the source.
    let mut stmt =
        tx.prepare("SELECT video_id FROM items WHERE source_key = ?1 AND in_remote = 1")?;
    let previously: Vec<String> = stmt
        .query_map([source.key], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for video_id in previously.iter().filter(|id| !seen.contains(id.as_str())) {
        tx.execute(
            "UPDATE items SET in_remote = 0 WHERE source_key = ?1 AND video_id = ?2",
            params![source.key, video_id],
        )?;
        summary.removed += 1;
    }

    let mut stmt =
        tx.prepare("SELECT status, COUNT(*) FROM items WHERE source_key = ?1 AND in_remote = 1 GROUP BY status")?;
    summary.by_status = stmt
        .query_map([source.key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    tx.commit()?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Structural;
    use crate::index::{Availability, Thumbnail};
    use crate::state::State;

    fn item(id: &str, position: u32, title: &str) -> RemoteItem {
        RemoteItem {
            video_id: id.into(),
            position,
            title: title.into(),
            channel: Some("Lofi Girl".into()),
            channel_id: Some("UC1".into()),
            description: None,
            tags: None,
            duration: Some(300),
            published_at: Some("2026-10-01T00:00:00Z".parse().unwrap()),
            availability: Availability::Public,
            thumbnails: vec![Thumbnail {
                url: "https://i.ytimg.com/x.jpg".into(),
                width: Some(480),
                height: Some(360),
            }],
        }
    }

    fn list(items: Vec<RemoteItem>) -> RemoteList {
        RemoteList {
            id: "PLx".into(),
            title: Some("Mix".into()),
            items,
        }
    }

    fn record(
        state: &mut State,
        items: Vec<RemoteItem>,
        decide: impl Fn(&RemoteItem) -> Decision,
    ) -> IndexSummary {
        let source = SourceRecord {
            key: "lofi",
            rules_hash: "h1",
            index_source: "ytdlp",
        };
        record_index(
            &mut state.conn,
            &source,
            &list(items),
            decide,
            "2026-10-31T12:00:00Z".parse().unwrap(),
        )
        .unwrap()
    }

    fn keep_unless_trailer(item: &RemoteItem) -> Decision {
        if item.title.contains("trailer") {
            Decision::Excluded { rule: 0 }
        } else {
            Decision::Keep { include: None }
        }
    }

    fn statuses(state: &State) -> Vec<(String, String, i64)> {
        let mut stmt = state
            .conn
            .prepare("SELECT video_id, status, in_remote FROM items WHERE source_key = 'lofi' ORDER BY video_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn records_new_items_with_their_decisions() {
        let mut state = State::open_in_memory().unwrap();
        let summary = record(
            &mut state,
            vec![item("a", 1, "Live set"), item("b", 2, "A trailer")],
            keep_unless_trailer,
        );
        assert_eq!((summary.total, summary.new, summary.removed), (2, 2, 0));
        assert_eq!(summary.count(status::PENDING), 1);
        assert_eq!(summary.filtered.get("excluded by a rule"), Some(&1));
        assert_eq!(
            statuses(&state),
            vec![
                ("a".into(), "pending".into(), 1),
                ("b".into(), "filtered".into(), 1)
            ]
        );
        let (title, youtube_id, hash): (String, String, String) = state
            .conn
            .query_row(
                "SELECT title, youtube_id, rules_hash FROM sources WHERE key = 'lofi'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (title.as_str(), youtube_id.as_str(), hash.as_str()),
            ("Mix", "PLx", "h1")
        );
        let decision: String = state
            .conn
            .query_row("SELECT decision FROM items WHERE video_id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(decision, r#"{"decision":"excluded","rule":0}"#);
    }

    #[test]
    fn reevaluates_undecided_items_but_never_downloaded_ones() {
        let mut state = State::open_in_memory().unwrap();
        record(
            &mut state,
            vec![item("a", 1, "Live set"), item("b", 2, "A trailer")],
            keep_unless_trailer,
        );
        state
            .conn
            .execute("UPDATE items SET status = 'done' WHERE video_id = 'a'", [])
            .unwrap();

        // New rules: drop everything.
        let summary = record(
            &mut state,
            vec![item("a", 1, "Live set"), item("b", 2, "A trailer")],
            |_| Decision::Structural {
                reason: Structural::TooShort {
                    duration: 1,
                    min: 2,
                },
            },
        );
        assert_eq!(summary.new, 0);
        assert_eq!(
            statuses(&state),
            vec![
                ("a".into(), "done".into(), 1),
                ("b".into(), "filtered".into(), 1)
            ]
        );

        // Rules relaxed again: the filtered one becomes pending.
        record(
            &mut state,
            vec![item("a", 1, "Live set"), item("b", 2, "A trailer")],
            |_| Decision::Keep { include: None },
        );
        assert_eq!(statuses(&state)[1], ("b".into(), "pending".into(), 1));
    }

    #[test]
    fn marks_items_that_left_the_source() {
        let mut state = State::open_in_memory().unwrap();
        record(
            &mut state,
            vec![item("a", 1, "x"), item("b", 2, "y")],
            keep_unless_trailer,
        );
        let summary = record(
            &mut state,
            vec![item("b", 1, "y"), item("c", 2, "z")],
            keep_unless_trailer,
        );
        assert_eq!((summary.total, summary.new, summary.removed), (2, 1, 1));
        assert_eq!(
            statuses(&state),
            vec![
                ("a".into(), "pending".into(), 0),
                ("b".into(), "pending".into(), 1),
                ("c".into(), "pending".into(), 1)
            ]
        );
        let position: i64 = state
            .conn
            .query_row("SELECT position FROM items WHERE video_id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(position, 1);
    }

    #[test]
    fn keeps_richer_video_details() {
        let mut state = State::open_in_memory().unwrap();
        let mut rich = item("a", 1, "x");
        rich.description = Some("full description".into());
        rich.tags = Some(vec!["lofi".into()]);
        record(&mut state, vec![rich], keep_unless_trailer);
        let mut thin = item("a", 1, "x (new title)");
        thin.duration = None;
        record(&mut state, vec![thin], keep_unless_trailer);
        let (title, description, tags, duration): (String, String, String, i64) = state
            .conn
            .query_row(
                "SELECT title, description, tags_json, duration_s FROM videos WHERE id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (
                title.as_str(),
                description.as_str(),
                tags.as_str(),
                duration
            ),
            ("x (new title)", "full description", r#"["lofi"]"#, 300)
        );
    }
}

fn upsert_video(conn: &Connection, item: &RemoteItem, now: &str) -> rusqlite::Result<()> {
    let tags = item
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).expect("tags serialize"));
    let thumbs = (!item.thumbnails.is_empty())
        .then(|| serde_json::to_string(&item.thumbnails).expect("thumbnails serialize"));
    conn.execute(
        "INSERT INTO videos (id, title, channel, channel_id, description, tags_json, duration_s,
           published_at, availability, thumbs_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET title = excluded.title,
           channel = COALESCE(excluded.channel, videos.channel),
           channel_id = COALESCE(excluded.channel_id, videos.channel_id),
           description = COALESCE(excluded.description, videos.description),
           tags_json = COALESCE(excluded.tags_json, videos.tags_json),
           duration_s = COALESCE(excluded.duration_s, videos.duration_s),
           published_at = COALESCE(excluded.published_at, videos.published_at),
           availability = excluded.availability,
           thumbs_json = COALESCE(excluded.thumbs_json, videos.thumbs_json),
           updated_at = excluded.updated_at",
        params![
            item.video_id,
            item.title,
            item.channel,
            item.channel_id,
            item.description,
            tags,
            item.duration.map(|d| d as i64),
            item.published_at.map(|t| t.to_string()),
            item.availability.as_str(),
            thumbs,
            now,
        ],
    )?;
    Ok(())
}

/// Timestamps compared in SQL: whole seconds, UTC, fixed width.
fn sql_time(t: Timestamp) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Give up retrying a video after this many failed attempts.
pub const MAX_ATTEMPTS: i64 = 5;

/// A video waiting to be downloaded, with what the index knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedItem {
    pub source_key: String,
    pub video_id: String,
    pub position: u32,
    pub status: String,
    pub attempts: i64,
    pub source_title: Option<String>,
    pub item: RemoteItem,
}

/// Videos to download for these sources, in source order then position:
/// pending, waiting on a field, or failed and due for a retry.
pub fn download_queue(
    conn: &Connection,
    sources: &[String],
    now: Timestamp,
) -> rusqlite::Result<Vec<QueuedItem>> {
    let mut stmt = conn.prepare(
        "SELECT i.video_id, i.position, i.status, i.attempts, s.title, v.title, v.channel, v.channel_id,
                v.description, v.tags_json, v.duration_s, v.published_at, v.availability, v.thumbs_json
         FROM items i JOIN videos v ON v.id = i.video_id JOIN sources s ON s.key = i.source_key
         WHERE i.source_key = ?1 AND i.in_remote = 1
           AND (i.status IN ('pending', 'pending_check')
                OR (i.status = 'failed' AND i.attempts < ?3
                    AND (i.next_attempt_at IS NULL OR i.next_attempt_at <= ?2)))
         ORDER BY i.position",
    )?;
    let mut queue = Vec::new();
    for key in sources {
        let rows = stmt.query_map(params![key, sql_time(now), MAX_ATTEMPTS], |r| {
            let video_id: String = r.get(0)?;
            let position: u32 = r.get(1)?;
            let tags: Option<String> = r.get(9)?;
            let thumbs: Option<String> = r.get(13)?;
            let published: Option<String> = r.get(11)?;
            let availability: Option<String> = r.get(12)?;
            Ok(QueuedItem {
                source_key: key.clone(),
                position,
                status: r.get(2)?,
                attempts: r.get(3)?,
                source_title: r.get(4)?,
                item: RemoteItem {
                    video_id: video_id.clone(),
                    position,
                    title: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    channel: r.get(6)?,
                    channel_id: r.get(7)?,
                    description: r.get(8)?,
                    tags: tags.and_then(|t| serde_json::from_str(&t).ok()),
                    duration: r.get::<_, Option<i64>>(10)?.map(|d| d.max(0) as u64),
                    published_at: published.and_then(|p| p.parse().ok()),
                    availability: crate::index::Availability::parse(
                        availability.as_deref().unwrap_or("public"),
                    ),
                    thumbnails: thumbs
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or_default(),
                },
                video_id,
            })
        })?;
        for row in rows {
            queue.push(row?);
        }
    }
    Ok(queue)
}

pub fn set_status(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    status: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET status = ?3 WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, status],
    )?;
    Ok(())
}

/// A decision made right before download (for `pending_check` items).
pub fn record_decision(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    decision: &Decision,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET status = ?3, decision = ?4 WHERE source_key = ?1 AND video_id = ?2",
        params![
            source_key,
            video_id,
            status_for(decision),
            decision.to_json()
        ],
    )?;
    Ok(())
}

/// Details learned from a full video lookup.
pub fn update_video_details(
    conn: &Connection,
    item: &RemoteItem,
    now: Timestamp,
) -> rusqlite::Result<()> {
    upsert_video(conn, item, &now.to_string())
}

/// Every file placed for a downloaded video, as absolute paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFiles {
    /// `output_dir`.
    pub dir: String,
    pub media: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<Subtitle>,
    pub metadata: String,
    /// yt-dlp's raw info JSON, with `keep_raw_info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_info: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtitle {
    pub lang: String,
    pub path: String,
}

impl VideoFiles {
    /// Every file, metadata last.
    pub fn paths(&self) -> Vec<&str> {
        let mut out = vec![self.media.as_str()];
        out.extend(self.cover.as_deref());
        out.extend(self.subtitles.iter().map(|s| s.path.as_str()));
        out.extend(self.raw_info.as_deref());
        out.push(&self.metadata);
        out
    }

    fn from_column(text: Option<String>) -> Option<Self> {
        text.and_then(|t| serde_json::from_str(&t).ok())
    }
}

pub fn mark_done(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    files: &VideoFiles,
    format: &str,
    media_bytes: u64,
    now: Timestamp,
) -> rusqlite::Result<()> {
    let json = serde_json::to_string(files).expect("files serialize");
    conn.execute(
        "UPDATE items SET status = 'done', dir = ?3, files = ?4, format = ?5, media_bytes = ?6, media_present = 1,
           downloaded_at = ?7, attempts = 0, next_attempt_at = NULL, last_error = NULL
         WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, files.dir, json, format, media_bytes as i64, now.to_string()],
    )?;
    Ok(())
}

/// A failed download: permanent failures become `unavailable`; others are
/// retried later with exponential backoff (1h, 2h, 4h, … capped at a day).
pub fn mark_failed(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    message: &str,
    permanent: bool,
    now: Timestamp,
) -> rusqlite::Result<()> {
    if permanent {
        conn.execute(
            "UPDATE items SET status = 'unavailable', last_error = ?3, next_attempt_at = NULL
             WHERE source_key = ?1 AND video_id = ?2",
            params![source_key, video_id, message],
        )?;
        return Ok(());
    }
    let attempts: i64 = conn.query_row(
        "SELECT attempts FROM items WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id],
        |r| r.get(0),
    )?;
    let attempts = attempts + 1;
    let hours = (1_i64 << (attempts - 1).clamp(0, 5)).min(24);
    let next = now
        .checked_add(jiff::SignedDuration::from_hours(hours))
        .unwrap_or(now);
    conn.execute(
        "UPDATE items SET status = 'failed', attempts = ?3, next_attempt_at = ?4, last_error = ?5
         WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, attempts, sql_time(next), message],
    )?;
    Ok(())
}

/// The files of a finished download of the same video and format in
/// another source, if any.
pub fn find_copy(
    conn: &Connection,
    video_id: &str,
    format: &str,
    except_source: &str,
) -> rusqlite::Result<Option<VideoFiles>> {
    Ok(conn
        .query_row(
            "SELECT files FROM items WHERE video_id = ?1 AND format = ?2 AND source_key != ?3
               AND status IN ('done', 'downloaded') AND files IS NOT NULL AND media_present = 1
             ORDER BY downloaded_at LIMIT 1",
            params![video_id, format, except_source],
            |r| r.get(0),
        )
        .optional()?
        .and_then(VideoFiles::from_column))
}

/// One hook run, as stored in `hook_runs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRun {
    pub source_key: String,
    pub video_id: String,
    pub hook: String,
    pub kind: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub attempts: u32,
    pub started_at: String,
    pub finished_at: String,
    pub error: Option<String>,
    pub values: BTreeMap<String, String>,
}

/// Store the latest run of a hook for a video.
pub fn record_hook_run(conn: &Connection, run: &HookRun) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO hook_runs (source_key, video_id, hook, type, status, exit_code, attempts,
           started_at, finished_at, error, input_values)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (source_key, video_id, hook, type) DO UPDATE SET
           status = excluded.status, exit_code = excluded.exit_code, attempts = excluded.attempts,
           started_at = excluded.started_at, finished_at = excluded.finished_at,
           error = excluded.error, input_values = excluded.input_values",
        params![
            run.source_key,
            run.video_id,
            run.hook,
            run.kind,
            if run.ok { "ok" } else { "failed" },
            run.exit_code,
            run.attempts,
            run.started_at,
            run.finished_at,
            run.error,
            serde_json::to_string(&run.values).expect("values serialize"),
        ],
    )?;
    Ok(())
}

/// The hooks whose last run failed for one video in one source.
pub fn failed_hooks(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT hook FROM hook_runs WHERE source_key = ?1 AND video_id = ?2 AND status = 'failed'",
    )?;
    stmt.query_map(params![source_key, video_id], |r| r.get(0))?
        .collect()
}

/// A downloaded video and where its files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub source_key: String,
    pub video_id: String,
    pub files: VideoFiles,
}

/// Every downloaded video of `sources`, in source order then position.
pub fn downloaded(conn: &Connection, sources: &[String]) -> rusqlite::Result<Vec<Downloaded>> {
    let mut stmt = conn.prepare(
        "SELECT source_key, video_id, files FROM items
         WHERE status IN ('done', 'downloaded') AND files IS NOT NULL
         ORDER BY source_key, position",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            VideoFiles::from_column(r.get(2)?),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (source_key, video_id, files) = row?;
        if let Some(files) = files
            && sources.contains(&source_key)
        {
            out.push(Downloaded {
                source_key,
                video_id,
                files,
            });
        }
    }
    Ok(out)
}

/// New file locations (and size) after a downloaded video was reprocessed.
pub fn update_files(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    files: &VideoFiles,
    media_bytes: u64,
) -> rusqlite::Result<()> {
    let json = serde_json::to_string(files).expect("files serialize");
    conn.execute(
        "UPDATE items SET dir = ?3, files = ?4, media_bytes = ?5 WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, files.dir, json, media_bytes as i64],
    )?;
    Ok(())
}

/// After a crash or an interrupt: downloads that didn't finish go back to
/// `pending`. Returns how many.
pub fn reset_interrupted(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE items SET status = 'pending' WHERE status = 'downloading'",
        [],
    )
}

/// Forget a video's download state in every source so it's downloaded
/// again. Returns `(source, files)` for each item reset.
pub fn reset_video(
    conn: &Connection,
    video_id: &str,
) -> rusqlite::Result<Vec<(String, Option<VideoFiles>)>> {
    let mut stmt = conn
        .prepare("SELECT source_key, files FROM items WHERE video_id = ?1 ORDER BY source_key")?;
    let items: Vec<(String, Option<VideoFiles>)> = stmt
        .query_map([video_id], |r| {
            Ok((r.get(0)?, VideoFiles::from_column(r.get(1)?)))
        })?
        .collect::<Result<_, _>>()?;
    conn.execute(
        "UPDATE items SET status = 'pending', dir = NULL, files = NULL, format = NULL, media_bytes = NULL, downloaded_at = NULL,
           attempts = 0, next_attempt_at = NULL, last_error = NULL, cleaned_up_at = NULL, cleanup_reason = NULL
         WHERE video_id = ?1",
        [video_id],
    )?;
    Ok(items)
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use crate::index::{Availability, Thumbnail};
    use crate::state::State;

    fn item(id: &str, position: u32) -> RemoteItem {
        RemoteItem {
            video_id: id.into(),
            position,
            title: format!("Video {id}"),
            channel: Some("Lofi Girl".into()),
            channel_id: None,
            description: None,
            tags: None,
            duration: Some(300),
            published_at: None,
            availability: Availability::Public,
            thumbnails: vec![Thumbnail {
                url: "https://i.ytimg.com/x.jpg".into(),
                width: None,
                height: None,
            }],
        }
    }

    fn seed(state: &mut State, key: &str, ids: &[&str]) {
        let items = ids
            .iter()
            .enumerate()
            .map(|(i, id)| item(id, i as u32 + 1))
            .collect();
        let list = RemoteList {
            id: format!("PL{key}"),
            title: Some(format!("{key} title")),
            items,
        };
        let source = SourceRecord {
            key,
            rules_hash: "h",
            index_source: "ytdlp",
        };
        record_index(
            &mut state.conn,
            &source,
            &list,
            |_| Decision::Keep { include: None },
            now(),
        )
        .unwrap();
    }

    fn now() -> Timestamp {
        "2026-10-31T12:00:00Z".parse().unwrap()
    }

    fn row(state: &State, key: &str, id: &str) -> (String, i64, Option<String>, Option<String>) {
        state
            .conn
            .query_row(
                "SELECT status, attempts, next_attempt_at, dir FROM items WHERE source_key = ?1 AND video_id = ?2",
                params![key, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    fn queued(state: &State, keys: &[&str], at: Timestamp) -> Vec<String> {
        let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        download_queue(&state.conn, &keys, at)
            .unwrap()
            .into_iter()
            .map(|q| format!("{}/{}", q.source_key, q.video_id))
            .collect()
    }

    #[test]
    fn queues_pending_items_in_order_with_index_details() {
        let mut state = State::open_in_memory().unwrap();
        seed(&mut state, "b", &["b1", "b2"]);
        seed(&mut state, "a", &["a1"]);
        set_status(&state.conn, "b", "b2", status::PENDING_CHECK).unwrap();
        state
            .conn
            .execute(
                "UPDATE items SET status = 'filtered' WHERE video_id = 'b1'",
                [],
            )
            .unwrap();
        assert_eq!(queued(&state, &["b", "a"], now()), vec!["b/b2", "a/a1"]);
        assert_eq!(queued(&state, &["a"], now()), vec!["a/a1"]);
        let q = download_queue(&state.conn, &["b".into()], now())
            .unwrap()
            .remove(0);
        assert_eq!(
            (q.position, q.status.as_str(), q.source_title.as_deref()),
            (2, "pending_check", Some("b title"))
        );
        assert_eq!(q.item.title, "Video b2");
        assert_eq!(q.item.thumbnails.len(), 1);
        // Gone from the source: not downloaded.
        seed(&mut state, "a", &[]);
        assert!(queued(&state, &["a"], now()).is_empty());
    }

    #[test]
    fn retries_with_backoff_and_gives_up() {
        let mut state = State::open_in_memory().unwrap();
        seed(&mut state, "a", &["x", "y"]);
        mark_failed(&state.conn, "a", "x", "HTTP Error 429", false, now()).unwrap();
        assert_eq!(
            row(&state, "a", "x"),
            (
                "failed".into(),
                1,
                Some("2026-10-31T13:00:00Z".into()),
                None
            )
        );
        assert_eq!(queued(&state, &["a"], now()), vec!["a/y"]);
        assert_eq!(
            queued(&state, &["a"], "2026-10-31T13:00:00Z".parse().unwrap()),
            vec!["a/x", "a/y"]
        );
        mark_failed(&state.conn, "a", "x", "again", false, now()).unwrap();
        assert_eq!(row(&state, "a", "x").2, Some("2026-10-31T14:00:00Z".into()));
        for _ in 0..3 {
            mark_failed(&state.conn, "a", "x", "again", false, now()).unwrap();
        }
        assert_eq!(row(&state, "a", "x").1, MAX_ATTEMPTS);
        assert_eq!(
            queued(&state, &["a"], "2027-01-01T00:00:00Z".parse().unwrap()),
            vec!["a/y"],
            "gave up"
        );

        mark_failed(&state.conn, "a", "y", "Private video", true, now()).unwrap();
        assert_eq!(row(&state, "a", "y").0, "unavailable");
        let error: String = state
            .conn
            .query_row(
                "SELECT last_error FROM items WHERE video_id = 'y'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(error, "Private video");
    }

    #[test]
    fn done_copies_interrupts_and_resets() {
        let mut state = State::open_in_memory().unwrap();
        seed(&mut state, "a", &["x"]);
        seed(&mut state, "b", &["x"]);
        set_status(&state.conn, "b", "x", status::DOWNLOADING).unwrap();
        assert_eq!(reset_interrupted(&state.conn).unwrap(), 1);
        assert_eq!(row(&state, "b", "x").0, "pending");

        let files = VideoFiles {
            dir: "/media/a/x".into(),
            media: "/media/a/x/x.mkv".into(),
            metadata: "/media/a/x/metadata.json".into(),
            ..VideoFiles::default()
        };
        mark_done(&state.conn, "a", "x", &files, "bv*+ba/b", 1234, now()).unwrap();
        assert_eq!(
            row(&state, "a", "x"),
            ("done".into(), 0, None, Some("/media/a/x".into()))
        );
        assert_eq!(
            find_copy(&state.conn, "x", "bv*+ba/b", "b").unwrap(),
            Some(files.clone())
        );
        assert_eq!(
            find_copy(&state.conn, "x", "ba/b", "b").unwrap(),
            None,
            "other format"
        );
        assert_eq!(
            find_copy(&state.conn, "x", "bv*+ba/b", "a").unwrap(),
            None,
            "same source"
        );

        let reset = reset_video(&state.conn, "x").unwrap();
        assert_eq!(reset, vec![("a".into(), Some(files)), ("b".into(), None)]);
        assert_eq!(row(&state, "a", "x"), ("pending".into(), 0, None, None));
        assert!(reset_video(&state.conn, "nope").unwrap().is_empty());
    }

    #[test]
    fn records_late_decisions_and_details() {
        let mut state = State::open_in_memory().unwrap();
        seed(&mut state, "a", &["x"]);
        let mut richer = item("x", 1);
        richer.description = Some("full".into());
        richer.published_at = Some(now());
        update_video_details(&state.conn, &richer, now()).unwrap();
        let (description, published): (String, String) = state
            .conn
            .query_row(
                "SELECT description, published_at FROM videos WHERE id = 'x'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (description.as_str(), published.as_str()),
            ("full", "2026-10-31T12:00:00Z")
        );
        record_decision(&state.conn, "a", "x", &Decision::NotIncluded).unwrap();
        assert_eq!(row(&state, "a", "x").0, "filtered");
    }
}
