//! Recording what indexing found: sources, videos and each video's status in
//! a source.

use std::collections::{BTreeMap, HashMap, HashSet};

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
    /// Downloaded into its work area, waiting to be processed.
    pub const FETCHED: &str = "fetched";
    /// Being processed: hooks, embedding, placing.
    pub const PROCESSING: &str = "processing";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const CLEANED_UP: &str = "cleaned_up";
    /// Couldn't be downloaded, and you said to stop showing it.
    pub const IGNORED: &str = "ignored";

    /// Statuses whose decision is recomputed on every index.
    pub const UNDECIDED: [&str; 4] = [PENDING, PENDING_CHECK, FILTERED, FILTER_ERROR];
    /// Statuses a video can be ignored in: it couldn't be downloaded.
    pub const IGNORABLE: [&str; 2] = [FAILED, UNAVAILABLE];
}

/// The status a filter decision puts an item in.
#[must_use]
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
    /// Lists of the source that couldn't be read (`<url>: <why>`), and
    /// whether that was because yt-dlp may not stand in for the API.
    pub errors: Vec<(String, bool)>,
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
    #[must_use]
    pub fn count(&self, status: &str) -> usize {
        self.by_status.get(status).copied().unwrap_or(0)
    }
}

/// Where a source's index came from.
pub struct SourceRecord<'a> {
    pub key: &'a str,
    pub rules_hash: &'a str,
    pub index_source: &'a str,
    /// The ids of the source's lists (its `urls`), in order.
    pub lists: &'a [String],
    /// Which of those lists each listed video was found in.
    pub found_in: &'a HashMap<String, Vec<String>>,
    /// Lists that couldn't be read this time: what's only known from them
    /// is left as it was.
    pub failed: &'a [String],
}

/// Record an index of `source` in one transaction. `decide` is applied to new
/// items and to items that haven't been downloaded yet.
///
/// # Errors
///
/// If a DB read or write fails; nothing is recorded then.
pub fn record_index(
    conn: &mut Connection,
    source: &SourceRecord<'_>,
    list: &RemoteList,
    decide: impl Fn(&RemoteItem) -> Decision,
    now: Timestamp,
) -> rusqlite::Result<IndexSummary> {
    let now = now.to_string();
    let tx = super::State::write_transaction(conn)?;
    upsert_source(&tx, source, list, &now)?;

    let mut summary = IndexSummary {
        total: list.items.len(),
        ..IndexSummary::default()
    };
    let mut seen = HashSet::new();
    for item in &list.items {
        if !seen.insert(item.video_id.as_str()) {
            continue; // the same video twice in one playlist: first position wins
        }
        record_item(&tx, source, item, &decide, &now, &mut summary)?;
    }
    summary.total = seen.len();
    mark_removed(&tx, source, &seen, &mut summary)?;
    // Imported from its backfill folder: in it, though not on YouTube.
    let mut stmt = tx.prepare("SELECT video_id FROM items WHERE source_key = ?1 AND local = 1")?;
    let local: Vec<String> = stmt
        .query_map([source.key], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    summary.total += local
        .iter()
        .filter(|id| !seen.contains(id.as_str()))
        .count();
    summary.by_status = count_by_status(&tx, source.key)?;
    tx.commit()?;
    Ok(summary)
}

fn upsert_source(
    tx: &rusqlite::Transaction<'_>,
    source: &SourceRecord<'_>,
    list: &RemoteList,
    now: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO sources (key, lists, title, rules_hash, last_indexed_at, last_index_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(key) DO UPDATE SET lists = excluded.lists,
           title = COALESCE(excluded.title, sources.title), rules_hash = excluded.rules_hash,
           last_indexed_at = excluded.last_indexed_at, last_index_source = excluded.last_index_source",
        params![
            source.key,
            serde_json::to_string(source.lists).unwrap_or_default(),
            list.title,
            source.rules_hash,
            now,
            source.index_source
        ],
    )?;
    Ok(())
}

/// Where an item was seen before, for lists that failed this time.
fn previous_lists(
    tx: &rusqlite::Transaction<'_>,
    source_key: &str,
    video_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let text: Option<String> = tx
        .query_row(
            "SELECT lists FROM items WHERE source_key = ?1 AND video_id = ?2",
            params![source_key, video_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

/// Record one listed item: its lists and position, and a fresh decision if
/// it's new or still undecided.
fn record_item(
    tx: &rusqlite::Transaction<'_>,
    source: &SourceRecord<'_>,
    item: &RemoteItem,
    decide: &impl Fn(&RemoteItem) -> Decision,
    now: &str,
    summary: &mut IndexSummary,
) -> rusqlite::Result<()> {
    upsert_video(tx, item, now)?;
    let mut lists = source
        .found_in
        .get(&item.video_id)
        .cloned()
        .unwrap_or_default();
    if !source.failed.is_empty() {
        for id in previous_lists(tx, source.key, &item.video_id)? {
            if source.failed.contains(&id) && !lists.contains(&id) {
                lists.push(id);
            }
        }
    }
    let lists = serde_json::to_string(&lists).unwrap_or_default();

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
    if !decide_now {
        tx.execute(
            "UPDATE items SET position = ?3, in_remote = 1, lists = ?4
             WHERE source_key = ?1 AND video_id = ?2",
            params![source.key, item.video_id, item.position, lists],
        )?;
        return Ok(());
    }
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
        "INSERT INTO items (source_key, video_id, position, in_remote, status, decision, lists)
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6)
         ON CONFLICT(source_key, video_id) DO UPDATE SET position = excluded.position,
           in_remote = 1, status = excluded.status, decision = excluded.decision,
           lists = excluded.lists",
        params![
            source.key,
            item.video_id,
            item.position,
            status_for(&decision),
            decision.to_json(),
            lists
        ],
    )?;
    if status_for(&decision) == status::FILTERED {
        *summary
            .filtered
            .entry(decision.label().to_string())
            .or_default() += 1;
    }
    Ok(())
}

/// Whatever wasn't `seen` has left the source, unless it may still be in a
/// list that couldn't be read.
fn mark_removed(
    tx: &rusqlite::Transaction<'_>,
    source: &SourceRecord<'_>,
    seen: &HashSet<&str>,
    summary: &mut IndexSummary,
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(
        "SELECT video_id FROM items WHERE source_key = ?1 AND in_remote = 1 AND local = 0",
    )?;
    let previously: Vec<String> = stmt
        .query_map([source.key], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for video_id in previously.iter().filter(|id| !seen.contains(id.as_str())) {
        if !source.failed.is_empty()
            && previous_lists(tx, source.key, video_id)?
                .iter()
                .any(|id| source.failed.contains(id))
        {
            continue;
        }
        tx.execute(
            "UPDATE items SET in_remote = 0 WHERE source_key = ?1 AND video_id = ?2",
            params![source.key, video_id],
        )?;
        summary.removed += 1;
    }
    Ok(())
}

/// Status counts over a source's videos that are currently in it.
fn count_by_status(
    tx: &rusqlite::Transaction<'_>,
    source_key: &str,
) -> rusqlite::Result<BTreeMap<String, usize>> {
    let mut stmt =
        tx.prepare("SELECT status, COUNT(*) FROM items WHERE source_key = ?1 AND in_remote = 1 GROUP BY status")?;
    stmt.query_map([source_key], |r| {
        Ok((
            r.get::<_, String>(0)?,
            usize::try_from(r.get::<_, i64>(1)?).unwrap_or(usize::MAX),
        ))
    })?
    .collect()
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
            kind: None,
            streamed: false,
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
        record_lists(state, items, &HashMap::new(), &[], decide)
    }

    /// Record a source of two lists, PLa and PLb.
    fn record_lists(
        state: &mut State,
        items: Vec<RemoteItem>,
        found_in: &HashMap<String, Vec<String>>,
        failed: &[String],
        decide: impl Fn(&RemoteItem) -> Decision,
    ) -> IndexSummary {
        let source = SourceRecord {
            key: "lofi",
            rules_hash: "h1",
            index_source: "ytdlp",
            lists: &["PLa".into(), "PLb".into()],
            found_in,
            failed,
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
        let (title, lists, hash): (String, String, String) = state
            .conn
            .query_row(
                "SELECT title, lists, rules_hash FROM sources WHERE key = 'lofi'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (title.as_str(), lists.as_str(), hash.as_str()),
            ("Mix", r#"["PLa","PLb"]"#, "h1")
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
    fn hook_retries_wait_longer_each_time_up_to_an_hour() {
        let waits: Vec<Option<u64>> = (0..9).map(|n| hook_retry_wait(n, 0)).collect();
        assert_eq!(
            waits,
            [
                Some(15),
                Some(16),
                Some(31),
                Some(96),
                Some(271),
                Some(640),
                Some(1311),
                Some(2416),
                None
            ]
        );
        assert_eq!(
            hook_retry_wait(7, 9),
            Some(2416 + 9 * 8),
            "jitter grows too"
        );
        assert_eq!(
            hook_retry_wait(0, 13),
            Some(15 + 3),
            "jitter stays under 10"
        );
    }

    #[test]
    fn a_failed_list_leaves_its_videos_alone() {
        let mut state = State::open_in_memory().unwrap();
        let keep = |_: &RemoteItem| Decision::Keep { include: None };
        let found = |pairs: &[(&str, &[&str])]| -> HashMap<String, Vec<String>> {
            pairs
                .iter()
                .map(|(v, l)| (v.to_string(), l.iter().map(|s| s.to_string()).collect()))
                .collect()
        };
        // a only in PLa, b in both, c only in PLb.
        record_lists(
            &mut state,
            vec![item("a", 1, "A"), item("b", 2, "B"), item("c", 3, "C")],
            &found(&[("a", &["PLa"]), ("b", &["PLa", "PLb"]), ("c", &["PLb"])]),
            &[],
            keep,
        );
        // PLb fails; PLa no longer has a. c is untouched, b keeps PLb.
        let summary = record_lists(
            &mut state,
            vec![item("b", 1, "B")],
            &found(&[("b", &["PLa"])]),
            &["PLb".into()],
            keep,
        );
        assert_eq!(summary.removed, 1, "only a is gone");
        assert_eq!(
            statuses(&state),
            vec![
                ("a".into(), "pending".into(), 0),
                ("b".into(), "pending".into(), 1),
                ("c".into(), "pending".into(), 1)
            ]
        );
        let lists: String = state
            .conn
            .query_row("SELECT lists FROM items WHERE video_id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(lists, r#"["PLa","PLb"]"#);

        // Both read again: c left PLb.
        let summary = record_lists(
            &mut state,
            vec![item("b", 1, "B")],
            &found(&[("b", &["PLa", "PLb"])]),
            &[],
            keep,
        );
        assert_eq!(summary.removed, 1, "c is gone now");
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

/// The version of how tidepool decides a video's kind. Bump it when that
/// changes (e.g. Premieres stopped counting as live streams in 2): kinds
/// stored by an older version count as unknown, and are decided again.
pub const KIND_VERSION: i64 = 2;

/// `videos.kind` when the current kind logic decided it; `NULL` otherwise.
fn trusted_kind(table: &str) -> String {
    let prefix = if table.is_empty() {
        String::new()
    } else {
        format!("{table}.")
    };
    format!("CASE WHEN {prefix}kind_version = {KIND_VERSION} THEN {prefix}kind END")
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
           published_at, availability, thumbs_json, updated_at, kind, kind_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET title = excluded.title,
           channel = COALESCE(excluded.channel, videos.channel),
           channel_id = COALESCE(excluded.channel_id, videos.channel_id),
           description = COALESCE(excluded.description, videos.description),
           tags_json = COALESCE(excluded.tags_json, videos.tags_json),
           duration_s = COALESCE(excluded.duration_s, videos.duration_s),
           published_at = COALESCE(excluded.published_at, videos.published_at),
           availability = excluded.availability,
           thumbs_json = COALESCE(excluded.thumbs_json, videos.thumbs_json),
           updated_at = excluded.updated_at,
           kind_version = CASE WHEN excluded.kind IS NOT NULL THEN excluded.kind_version
             ELSE videos.kind_version END,
           kind = COALESCE(excluded.kind, videos.kind)",
        params![
            item.video_id,
            item.title,
            item.channel,
            item.channel_id,
            item.description,
            tags,
            item.duration.map(|d| i64::try_from(d).unwrap_or(i64::MAX)),
            item.published_at.map(|t| t.to_string()),
            item.availability.as_str(),
            thumbs,
            now,
            item.kind.map(super::super::index::VideoKind::as_str),
            item.kind.map(|_| KIND_VERSION),
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
    /// When its download finished (status `fetched`).
    pub fetched_at: Option<String>,
}

/// Videos to download for these sources, in source order then position:
/// pending, waiting on a field, or failed and due for a retry.
///
/// # Errors
///
/// If the DB read fails.
pub fn download_queue(
    conn: &Connection,
    sources: &[String],
    now: Timestamp,
) -> rusqlite::Result<Vec<QueuedItem>> {
    queue_where(
        conn,
        sources,
        "i.in_remote = 1 AND i.local = 0
           AND (i.status IN ('pending', 'pending_check')
                OR (i.status = 'failed' AND i.attempts < ?3
                    AND (i.next_attempt_at IS NULL OR i.next_attempt_at <= ?2)))",
        "i.position",
        now,
    )
}

/// Videos downloaded into their work area and waiting to be processed, in
/// the order they were fetched.
///
/// # Errors
///
/// If the DB read fails.
pub fn process_queue(conn: &Connection, sources: &[String]) -> rusqlite::Result<Vec<QueuedItem>> {
    let mut queue = queue_where(
        conn,
        sources,
        "i.status = 'fetched'",
        "i.fetched_at, i.position",
        Timestamp::now(),
    )?;
    // Across sources too.
    queue.sort_by(|a, b| a.fetched_at.cmp(&b.fetched_at));
    Ok(queue)
}

/// Queued items of `sources` matching `condition` (`?1` is the source,
/// and it may use `?2`, now, and `?3`, the attempt limit), in `order`
/// within each source.
fn queue_where(
    conn: &Connection,
    sources: &[String],
    condition: &str,
    order: &str,
    now: Timestamp,
) -> rusqlite::Result<Vec<QueuedItem>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT i.video_id, i.position, i.status, i.attempts, s.title, v.title, v.channel, v.channel_id,
                v.description, v.tags_json, v.duration_s, v.published_at, v.availability, v.thumbs_json,
                {}, i.fetched_at
         FROM items i JOIN videos v ON v.id = i.video_id JOIN sources s ON s.key = i.source_key
         WHERE i.source_key = ?1 AND {condition}
         ORDER BY {order}",
        trusted_kind("v")
    ))?;
    let mut queue = Vec::new();
    let (time, limit) = (sql_time(now), MAX_ATTEMPTS);
    let used = stmt.parameter_count();
    for key in sources {
        let all: [&dyn rusqlite::ToSql; 3] = [key, &time, &limit];
        let rows = stmt.query_map(&all[..used], |r| {
            let video_id: String = r.get(0)?;
            let position: u32 = r.get(1)?;
            let tags: Option<String> = r.get(9)?;
            let thumbs: Option<String> = r.get(13)?;
            let kind: Option<String> = r.get(14)?;
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
                    duration: r
                        .get::<_, Option<i64>>(10)?
                        .map(|d| u64::try_from(d).unwrap_or(0)),
                    published_at: published.and_then(|p| p.parse().ok()),
                    availability: crate::index::Availability::parse(
                        availability.as_deref().unwrap_or("public"),
                    ),
                    thumbnails: thumbs
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or_default(),
                    kind: kind.as_deref().and_then(crate::index::VideoKind::parse),
                    streamed: false,
                },
                video_id,
                fetched_at: r.get(15)?,
            })
        })?;
        for row in rows {
            queue.push(row?);
        }
    }
    Ok(queue)
}

/// Put an item in `status`.
///
/// # Errors
///
/// If the DB write fails.
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
///
/// # Errors
///
/// If the DB write fails.
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
///
/// # Errors
///
/// If the DB write fails.
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
    /// Files a `finished` hook removed on purpose: `media`, `cover` or
    /// `subtitle:<lang>`. They don't count as missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtitle {
    pub lang: String,
    pub path: String,
}

impl VideoFiles {
    #[must_use]
    pub fn is_removed(&self, what: &str) -> bool {
        self.removed.iter().any(|r| r == what)
    }

    /// Every file, metadata last.
    #[must_use]
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

/// `value` as JSON, for a column; failing like any value that can't be
/// stored.
fn to_json(value: &(impl Serialize + ?Sized)) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// A finished download, as stored.
#[derive(Clone, Copy)]
pub struct Done<'a> {
    pub files: &'a VideoFiles,
    /// Its metadata.json, kept as a copy to restore it.
    pub metadata: &'a str,
    /// The format string it was downloaded with.
    pub format: &'a str,
    pub media_bytes: u64,
}

/// Record a finished download: its files, metadata copy and size.
///
/// # Errors
///
/// If the DB write fails, or its files can't be serialized to JSON.
pub fn mark_done(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    done: Done<'_>,
    now: Timestamp,
) -> rusqlite::Result<()> {
    let Done {
        files,
        metadata,
        format,
        media_bytes,
    } = done;
    let json = to_json(files)?;
    conn.execute(
        "UPDATE items SET status = 'done', dir = ?3, files = ?4, metadata = ?5, format = ?6, media_bytes = ?7,
           media_present = 1, missing = NULL, downloaded_at = ?8, attempts = 0, next_attempt_at = NULL,
           last_error = NULL
         WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, files.dir, json, metadata, format, i64::try_from(media_bytes).unwrap_or(i64::MAX), now.to_string()],
    )?;
    Ok(())
}

/// What's missing of a downloaded video (empty = nothing).
///
/// # Errors
///
/// If the DB write fails, or the list can't be serialized to JSON.
pub fn set_missing(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    missing: &[String],
) -> rusqlite::Result<()> {
    let json = (!missing.is_empty())
        .then(|| to_json(missing))
        .transpose()?;
    conn.execute(
        "UPDATE items SET missing = ?3, media_present = ?4 WHERE source_key = ?1 AND video_id = ?2",
        params![
            source_key,
            video_id,
            json,
            !missing.iter().any(|m| m == "media")
        ],
    )?;
    Ok(())
}

/// Download a video again whose media is gone (its other files are
/// replaced too).
///
/// # Errors
///
/// If the DB write fails.
pub fn requeue(conn: &Connection, source_key: &str, video_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET status = 'pending', attempts = 0, next_attempt_at = NULL, last_error = NULL
         WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id],
    )?;
    Ok(())
}

/// The stored copy of a video's metadata.json.
///
/// # Errors
///
/// If the DB read fails.
pub fn metadata_copy(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
) -> rusqlite::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT metadata FROM items WHERE source_key = ?1 AND video_id = ?2",
            params![source_key, video_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// The thumbnails the indexer found for a video.
///
/// # Errors
///
/// If the DB read fails.
pub fn thumbnails(
    conn: &Connection,
    video_id: &str,
) -> rusqlite::Result<Vec<crate::index::Thumbnail>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT thumbs_json FROM videos WHERE id = ?1",
            [video_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(json
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default())
}

/// A failed download: permanent failures become `unavailable`; others are
/// retried later with exponential backoff (1h, 2h, 4h, … capped at a day).
///
/// # Errors
///
/// If a DB read or write fails.
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
///
/// # Errors
///
/// If the DB read fails.
pub fn find_copy(
    conn: &Connection,
    video_id: &str,
    format: &str,
    except_source: &str,
) -> rusqlite::Result<Option<VideoFiles>> {
    Ok(conn
        .query_row(
            "SELECT files FROM items WHERE video_id = ?1 AND format = ?2 AND source_key != ?3
               AND status = 'done' AND files IS NOT NULL AND media_present = 1
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
    /// An automatic retry of a failed `finished` hook: it keeps counting
    /// retries, where any other run starts over.
    pub retry: bool,
}

/// The longest wait before a failed `finished` hook is tried again; past
/// it, the hook has failed for good.
pub const MAX_HOOK_RETRY_WAIT: u64 = 3600;

/// How long to wait before automatic retry number `retries + 1` of a
/// failed `finished` hook (Sidekiq's curve: `n⁴ + 15 + rand(10)·(n + 1)`
/// seconds, `jitter` in 0..10); `None` once that would pass an hour.
#[must_use]
pub fn hook_retry_wait(retries: u32, jitter: u32) -> Option<u64> {
    let n = u64::from(retries);
    let base = n.pow(4) + 15;
    (base <= MAX_HOOK_RETRY_WAIT).then(|| base + u64::from(jitter % 10) * (n + 1))
}

/// A number in 0..10, different each time.
fn jitter() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    (hasher.finish() % 10) as u32
}

/// Store a run of a hook for a video: its history, and its latest state. A
/// failed `finished` hook is tried again later (see [`hook_retry_wait`]),
/// until it fails for good.
///
/// # Errors
///
/// If a DB read or write fails, or the hook's input values can't be serialized
/// to JSON.
pub fn record_hook_run(conn: &Connection, run: &HookRun) -> rusqlite::Result<()> {
    let (previous, previous_status): (u32, Option<String>) = conn
        .query_row(
            "SELECT retries, status FROM hook_runs
             WHERE source_key = ?1 AND video_id = ?2 AND hook = ?3 AND type = ?4",
            params![run.source_key, run.video_id, run.hook, run.kind],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, None));
    // A queued hook's first run isn't a retry.
    let retry = run.retry && previous_status.as_deref() != Some("pending");
    conn.execute(
        "INSERT INTO hook_attempts (source_key, video_id, hook, type, ok, exit_code, attempts,
           started_at, finished_at, error, retry)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run.source_key,
            run.video_id,
            run.hook,
            run.kind,
            run.ok,
            run.exit_code,
            run.attempts,
            run.started_at,
            run.finished_at,
            run.error,
            retry,
        ],
    )?;
    let finished_at: Option<Timestamp> = run.finished_at.parse().ok();
    let later = |seconds: u64| {
        finished_at
            .and_then(|t| {
                t.checked_add(jiff::SignedDuration::from_secs(
                    i64::try_from(seconds).unwrap_or(i64::MAX),
                ))
                .ok()
            })
            .map(|t| t.to_string())
    };
    let stopped = run.error.as_deref() == Some("stopped");
    let (status, retries, next_attempt_at) = if run.ok {
        ("ok", 0, None)
    } else if run.kind != "finished" {
        // downloaded/embed: the download failed, and is retried as a whole.
        ("failed", 0, None)
    } else if stopped {
        // Not the hook's fault: on the next run, same count (still a first
        // run if it was one).
        (
            if retry { "retrying" } else { "pending" },
            if retry { previous } else { 0 },
            later(0),
        )
    } else {
        let retries = if retry { previous + 1 } else { 0 };
        match hook_retry_wait(retries, jitter()) {
            Some(wait) => ("retrying", retries, later(wait)),
            None => ("failed", retries, None),
        }
    };
    conn.execute(
        "INSERT INTO hook_runs (source_key, video_id, hook, type, status, exit_code, attempts,
           started_at, finished_at, error, input_values, retries, next_attempt_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT (source_key, video_id, hook, type) DO UPDATE SET
           status = excluded.status, exit_code = excluded.exit_code, attempts = excluded.attempts,
           started_at = excluded.started_at, finished_at = excluded.finished_at,
           error = excluded.error, input_values = excluded.input_values,
           retries = excluded.retries, next_attempt_at = excluded.next_attempt_at",
        params![
            run.source_key,
            run.video_id,
            run.hook,
            run.kind,
            status,
            run.exit_code,
            run.attempts,
            run.started_at,
            run.finished_at,
            run.error,
            to_json(&run.values)?,
            retries,
            next_attempt_at,
        ],
    )?;
    Ok(())
}

/// A hook's latest status for a video: `ok`, `retrying` or `failed`.
///
/// # Errors
///
/// If the DB read fails.
pub fn hook_status(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    hook: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT status FROM hook_runs WHERE source_key = ?1 AND video_id = ?2 AND hook = ?3
         ORDER BY type = 'finished' DESC LIMIT 1",
        params![source_key, video_id, hook],
        |r| r.get(0),
    )
    .optional()
}

/// `finished` hooks due to run, per video: queued (`pending`) or waiting
/// for an automatic retry. `(source_key, video_id, hooks)`.
///
/// # Errors
///
/// If the DB read fails.
pub fn due_hooks(
    conn: &Connection,
    now: Timestamp,
) -> rusqlite::Result<Vec<(String, String, Vec<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT source_key, video_id, hook, next_attempt_at FROM hook_runs
         WHERE status IN ('pending', 'retrying') AND type = 'finished'
         ORDER BY source_key, video_id, hook",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut due: Vec<(String, String, Vec<String>)> = Vec::new();
    for row in rows {
        let (source, video, hook, at) = row?;
        let ready = at
            .and_then(|t| t.parse::<Timestamp>().ok())
            .is_none_or(|t| t <= now);
        if !ready {
            continue;
        }
        match due.last_mut() {
            Some((s, v, hooks)) if *s == source && *v == video => hooks.push(hook),
            _ => due.push((source, video, vec![hook])),
        }
    }
    Ok(due)
}

/// When the next queued or retried `finished` hook is due, if any.
///
/// # Errors
///
/// If the DB read fails.
pub fn next_hook_due(conn: &Connection) -> rusqlite::Result<Option<Timestamp>> {
    let next: Option<String> = conn.query_row(
        "SELECT MIN(next_attempt_at) FROM hook_runs
         WHERE type = 'finished' AND status IN ('pending', 'retrying')",
        [],
        |r| r.get(0),
    )?;
    Ok(next.and_then(|t| t.parse().ok()))
}

/// Put every item in status `from` back to `to` (after a service crashed).
///
/// # Errors
///
/// If the DB write fails.
pub fn reset_status(conn: &Connection, from: &str, to: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE items SET status = ?2 WHERE status = ?1",
        params![from, to],
    )
}

/// A video's `finished` hooks that haven't run yet, or will run again.
///
/// # Errors
///
/// If the DB read fails.
pub fn waiting_hooks(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT hook FROM hook_runs
         WHERE source_key = ?1 AND video_id = ?2 AND type = 'finished'
           AND status IN ('pending', 'retrying')
         ORDER BY hook",
    )?;
    stmt.query_map(params![source_key, video_id], |r| r.get(0))?
        .collect()
}

/// The hooks whose last run failed (and may be waiting for a retry) for one
/// video in one source.
///
/// # Errors
///
/// If the DB read fails.
pub fn failed_hooks(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT hook FROM hook_runs
         WHERE source_key = ?1 AND video_id = ?2 AND status IN ('failed', 'retrying')",
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
///
/// # Errors
///
/// If the DB read fails.
pub fn downloaded(conn: &Connection, sources: &[String]) -> rusqlite::Result<Vec<Downloaded>> {
    let mut stmt = conn.prepare(
        "SELECT source_key, video_id, files FROM items
         WHERE status = 'done' AND files IS NOT NULL
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
///
/// # Errors
///
/// If the DB write fails, or its files can't be serialized to JSON.
pub fn update_files(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    files: &VideoFiles,
    media_bytes: u64,
) -> rusqlite::Result<()> {
    let json = to_json(files)?;
    // Keep the metadata copy in step with the file on disk.
    let metadata = std::fs::read_to_string(&files.metadata).ok();
    conn.execute(
        "UPDATE items SET dir = ?3, files = ?4, media_bytes = ?5, metadata = COALESCE(?6, metadata)
         WHERE source_key = ?1 AND video_id = ?2",
        params![
            source_key,
            video_id,
            files.dir,
            json,
            i64::try_from(media_bytes).unwrap_or(i64::MAX),
            metadata
        ],
    )?;
    Ok(())
}

/// Videos tidepool knows in full (description, tags and duration from the
/// API), as index items; the API path skips fetching their details again.
///
/// # Errors
///
/// If the DB read fails.
pub fn known_details(
    conn: &Connection,
    ids: &[String],
) -> rusqlite::Result<std::collections::HashMap<String, RemoteItem>> {
    videos_where(
        conn,
        ids,
        "AND description IS NOT NULL AND tags_json IS NOT NULL AND duration_s IS NOT NULL",
    )
}

/// What's known about these videos, however little.
///
/// # Errors
///
/// If the DB read fails.
pub fn videos(
    conn: &Connection,
    ids: &[String],
) -> rusqlite::Result<std::collections::HashMap<String, RemoteItem>> {
    videos_where(conn, ids, "")
}

fn videos_where(
    conn: &Connection,
    ids: &[String],
    condition: &str,
) -> rusqlite::Result<std::collections::HashMap<String, RemoteItem>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, channel, channel_id, description, tags_json, duration_s, published_at,
           availability, thumbs_json, {}
         FROM videos WHERE id = ?1 {condition}",
        trusted_kind("")
    ))?;
    let mut out = std::collections::HashMap::new();
    for id in ids {
        let found = stmt
            .query_row([id], |r| {
                let tags: Option<String> = r.get(5)?;
                let published: Option<String> = r.get(7)?;
                let availability: Option<String> = r.get(8)?;
                let thumbs: Option<String> = r.get(9)?;
                Ok(RemoteItem {
                    video_id: r.get(0)?,
                    position: 0,
                    title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    channel: r.get(2)?,
                    channel_id: r.get(3)?,
                    description: r.get(4)?,
                    tags: tags.and_then(|t| serde_json::from_str(&t).ok()),
                    duration: r
                        .get::<_, Option<i64>>(6)?
                        .map(|d| u64::try_from(d).unwrap_or(0)),
                    published_at: published.and_then(|p| p.parse().ok()),
                    availability: crate::index::Availability::parse(
                        availability.as_deref().unwrap_or("public"),
                    ),
                    thumbnails: thumbs
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or_default(),
                    kind: r
                        .get::<_, Option<String>>(10)?
                        .as_deref()
                        .and_then(crate::index::VideoKind::parse),
                    streamed: false,
                })
            })
            .optional()?;
        if let Some(item) = found {
            out.insert(id.clone(), item);
        }
    }
    Ok(out)
}

/// Kinds already found out for these videos.
///
/// # Errors
///
/// If the DB read fails.
pub fn known_kinds(
    conn: &Connection,
    ids: &[String],
) -> rusqlite::Result<std::collections::HashMap<String, crate::index::VideoKind>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT kind FROM videos WHERE id = ?1 AND kind IS NOT NULL AND {} IS NOT NULL",
        trusted_kind("")
    ))?;
    let mut out = std::collections::HashMap::new();
    for id in ids {
        let kind: Option<String> = stmt.query_row([id], |r| r.get(0)).optional()?;
        if let Some(kind) = kind.as_deref().and_then(crate::index::VideoKind::parse) {
            out.insert(id.clone(), kind);
        }
    }
    Ok(out)
}

/// A channel looked up earlier: its id and name.
///
/// # Errors
///
/// If the DB read fails.
pub fn cached_channel(
    conn: &Connection,
    reference: &str,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT channel_id, title FROM channels WHERE ref = ?1",
        [reference],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// Remember what a channel reference points to.
///
/// # Errors
///
/// If the DB write fails.
pub fn save_channel(
    conn: &Connection,
    reference: &str,
    channel_id: &str,
    title: Option<&str>,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO channels (ref, channel_id, title, resolved_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(ref) DO UPDATE SET channel_id = excluded.channel_id, title = excluded.title,
           resolved_at = excluded.resolved_at",
        params![reference, channel_id, title, now.to_string()],
    )?;
    Ok(())
}

/// The Pacific-time date YouTube's quota day runs on.
#[must_use]
pub fn quota_day(now: Timestamp) -> String {
    now.in_tz("America/Los_Angeles").map_or_else(
        |_| now.to_string()[..10].to_string(),
        |z| z.date().to_string(),
    )
}

/// Units an API key spent on `day`.
///
/// # Errors
///
/// If the DB read fails.
pub fn quota_used(conn: &Connection, key_id: &str, day: &str) -> rusqlite::Result<u32> {
    conn.query_row(
        "SELECT COALESCE(SUM(units), 0) FROM quota_usage WHERE key_id = ?1 AND day = ?2",
        params![key_id, day],
        |r| {
            r.get::<_, i64>(0)
                .map(|u| u32::try_from(u.max(0)).unwrap_or(u32::MAX))
        },
    )
}

/// Record units an API key spent indexing a source.
///
/// # Errors
///
/// If the DB write fails.
pub fn add_quota(
    conn: &Connection,
    key_id: &str,
    day: &str,
    source_key: &str,
    units: u32,
) -> rusqlite::Result<()> {
    if units > 0 {
        conn.execute(
            "INSERT INTO quota_usage (day, key_id, source_key, units) VALUES (?1, ?2, ?3, ?4)",
            params![day, key_id, source_key, units],
        )?;
    }
    Ok(())
}

/// After a crash or an interrupt: downloads that didn't finish go back to
/// `pending`. Returns how many.
///
/// # Errors
///
/// If the DB write fails.
pub fn reset_interrupted(conn: &Connection) -> rusqlite::Result<usize> {
    Ok(conn.execute(
        "UPDATE items SET status = 'pending' WHERE status = 'downloading'",
        [],
    )? + conn.execute(
        "UPDATE items SET status = 'fetched' WHERE status = 'processing'",
        [],
    )?)
}

/// A video's download is in its work area, waiting to be processed.
///
/// # Errors
///
/// If the DB write fails.
pub fn mark_fetched(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET status = 'fetched', fetched_at = ?3
         WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, now.to_string()],
    )?;
    Ok(())
}

/// Fetched videos of `sources`: `(source, video)`.
///
/// # Errors
///
/// If the DB read fails.
pub fn fetched(conn: &Connection, sources: &[String]) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT source_key, video_id FROM items WHERE source_key = ?1 AND status = 'fetched'",
    )?;
    let mut out = Vec::new();
    for key in sources {
        for row in stmt.query_map([key], |r| Ok((r.get(0)?, r.get(1)?)))? {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Queue a placed video's `finished` hooks, to run in this order.
///
/// # Errors
///
/// If the DB write fails.
pub fn queue_hooks(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    hooks: &[String],
    now: Timestamp,
) -> rusqlite::Result<()> {
    for hook in hooks {
        conn.execute(
            "INSERT INTO hook_runs (source_key, video_id, hook, type, status, retries, next_attempt_at)
             VALUES (?1, ?2, ?3, 'finished', 'pending', 0, ?4)
             ON CONFLICT (source_key, video_id, hook, type) DO UPDATE SET
               status = 'pending', retries = 0, next_attempt_at = excluded.next_attempt_at",
            params![source_key, video_id, hook, now.to_string()],
        )?;
    }
    Ok(())
}

/// A video's status in each source it's in: `(source, status)`.
///
/// # Errors
///
/// If the DB read fails.
pub fn item_statuses(conn: &Connection, video_id: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn
        .prepare("SELECT source_key, status FROM items WHERE video_id = ?1 ORDER BY source_key")?;
    stmt.query_map([video_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// Every item's status in a source.
///
/// # Errors
///
/// If the DB read fails.
pub fn statuses(conn: &Connection, source_key: &str) -> rusqlite::Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT video_id, status FROM items WHERE source_key = ?1")?;
    stmt.query_map([source_key], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// A downloaded video, as cleanup sees it.
#[derive(Debug, Clone)]
pub struct Kept {
    pub video_id: String,
    pub position: i64,
    pub files: VideoFiles,
    pub media_bytes: u64,
    pub downloaded_at: Option<Timestamp>,
    /// Imported from the source's backfill folder.
    pub local: bool,
}

/// A source's downloaded videos.
///
/// # Errors
///
/// If the DB read fails.
pub fn kept(conn: &Connection, source_key: &str) -> rusqlite::Result<Vec<Kept>> {
    let mut stmt = conn.prepare(
        "SELECT video_id, position, files, media_bytes, downloaded_at, local FROM items
         WHERE source_key = ?1 AND status = 'done' AND files IS NOT NULL
         ORDER BY position",
    )?;
    let rows = stmt.query_map([source_key], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1)?,
            VideoFiles::from_column(r.get(2)?),
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (video_id, position, files, bytes, downloaded_at, local) = row?;
        if let Some(files) = files {
            out.push(Kept {
                video_id,
                position: position.unwrap_or(0),
                files,
                media_bytes: u64::try_from(bytes.unwrap_or(0)).unwrap_or(0),
                downloaded_at: downloaded_at.and_then(|t| t.parse().ok()),
                local,
            });
        }
    }
    Ok(out)
}

/// A video cleanup removed: it's never downloaded again (`reset` undoes it).
///
/// # Errors
///
/// If the DB write fails.
pub fn mark_cleaned_up(
    conn: &Connection,
    source_key: &str,
    video_id: &str,
    reason: &str,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET status = 'cleaned_up', cleaned_up_at = ?3, cleanup_reason = ?4
         WHERE source_key = ?1 AND video_id = ?2",
        params![source_key, video_id, now.to_string(), reason],
    )?;
    Ok(())
}

/// A video back from the trash: downloaded again, as before.
///
/// # Errors
///
/// If the DB write fails.
pub fn mark_restored(conn: &Connection, source_key: &str, video_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE items SET status = 'done', cleaned_up_at = NULL, cleanup_reason = NULL
         WHERE source_key = ?1 AND video_id = ?2 AND status = 'cleaned_up'",
        params![source_key, video_id],
    )?;
    Ok(())
}

/// One video's files in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    pub id: i64,
    pub source_key: String,
    pub video_id: String,
    pub dir: String,
    /// `(where it was, where it is now)`.
    pub files: Vec<(String, String)>,
    pub bytes: u64,
    pub reason: String,
    pub trashed_at: String,
    pub delete_after: String,
}

/// Put a video's files in the trash.
///
/// # Errors
///
/// If the DB write fails, or its files can't be serialized to JSON.
pub fn add_trash(conn: &Connection, t: &Trashed) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO trash (source_key, video_id, dir, files, bytes, reason, trashed_at, delete_after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            t.source_key,
            t.video_id,
            t.dir,
            to_json(&t.files)?,
            i64::try_from(t.bytes).unwrap_or(i64::MAX),
            t.reason,
            t.trashed_at,
            t.delete_after,
        ],
    )?;
    Ok(())
}

/// Everything in the trash, oldest first.
///
/// # Errors
///
/// If the DB read fails.
pub fn trash(conn: &Connection) -> rusqlite::Result<Vec<Trashed>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_key, video_id, dir, files, bytes, reason, trashed_at, delete_after
         FROM trash ORDER BY trashed_at, id",
    )?;
    stmt.query_map([], |r| {
        let files: String = r.get(4)?;
        Ok(Trashed {
            id: r.get(0)?,
            source_key: r.get(1)?,
            video_id: r.get(2)?,
            dir: r.get(3)?,
            files: serde_json::from_str(&files).unwrap_or_default(),
            bytes: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
            reason: r.get(6)?,
            trashed_at: r.get(7)?,
            delete_after: r.get(8)?,
        })
    })?
    .collect()
}

/// Take an entry out of the trash.
///
/// # Errors
///
/// If the DB write fails.
pub fn remove_trash(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM trash WHERE id = ?1", [id])?;
    Ok(())
}

/// Forget a video's download state in every source so it's downloaded
/// again. Returns `(source, files)` for each item reset.
///
/// # Errors
///
/// If a DB read or write fails.
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

/// Ignore `video_id` in `sources` (every source when empty) where it
/// couldn't be downloaded: it's never tried again, and lists leave it out.
/// Returns the sources it was ignored in.
///
/// # Errors
///
/// If a DB read or write fails.
pub fn ignore(
    conn: &Connection,
    video_id: &str,
    sources: &[String],
) -> rusqlite::Result<Vec<String>> {
    set_ignored(conn, video_id, sources, true)
}

/// Stop ignoring `video_id` in `sources` (every source when empty): it's
/// checked and downloaded again like a new video. Returns the sources it was
/// ignored in.
///
/// # Errors
///
/// If a DB read or write fails.
pub fn unignore(
    conn: &Connection,
    video_id: &str,
    sources: &[String],
) -> rusqlite::Result<Vec<String>> {
    set_ignored(conn, video_id, sources, false)
}

fn set_ignored(
    conn: &Connection,
    video_id: &str,
    sources: &[String],
    ignore: bool,
) -> rusqlite::Result<Vec<String>> {
    let from: &[&str] = if ignore {
        &status::IGNORABLE
    } else {
        &[status::IGNORED]
    };
    let mut stmt = conn
        .prepare("SELECT source_key, status FROM items WHERE video_id = ?1 ORDER BY source_key")?;
    let found: Vec<String> = stmt
        .query_map([video_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(key, status)| {
            from.contains(&status.as_str()) && (sources.is_empty() || sources.contains(key))
        })
        .map(|(key, _)| key)
        .collect();
    for key in &found {
        if ignore {
            // The error stays: it says why.
            conn.execute(
                "UPDATE items SET status = 'ignored', next_attempt_at = NULL
                 WHERE source_key = ?1 AND video_id = ?2",
                [key, video_id],
            )?;
        } else {
            conn.execute(
                "UPDATE items SET status = 'pending', attempts = 0, next_attempt_at = NULL,
                   last_error = NULL
                 WHERE source_key = ?1 AND video_id = ?2",
                [key, video_id],
            )?;
        }
    }
    Ok(found)
}

/// Ignore every video of `source` in `status` (one of
/// [`status::IGNORABLE`]). Returns how many.
///
/// # Errors
///
/// If the DB write fails.
pub fn ignore_all(conn: &Connection, source: &str, status: &str) -> rusqlite::Result<usize> {
    if !status::IGNORABLE.contains(&status) {
        return Ok(0);
    }
    conn.execute(
        "UPDATE items SET status = 'ignored', next_attempt_at = NULL
         WHERE source_key = ?1 AND status = ?2",
        [source, status],
    )
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
            kind: None,
            streamed: false,
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
            lists: &["PLx".into()],
            found_in: &HashMap::new(),
            failed: &[],
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
    fn ignored_videos_stay_ignored_until_brought_back() {
        let mut state = State::open_in_memory().unwrap();
        seed(&mut state, "a", &["x", "y", "z"]);
        seed(&mut state, "b", &["x"]);
        mark_failed(&state.conn, "a", "x", "HTTP Error 429", false, now()).unwrap();
        mark_failed(&state.conn, "b", "x", "Video unavailable", true, now()).unwrap();
        let later: Timestamp = "2026-11-30T00:00:00Z".parse().unwrap();

        // Only where it couldn't be downloaded.
        assert!(ignore(&state.conn, "y", &[]).unwrap().is_empty());
        assert_eq!(ignore(&state.conn, "x", &["a".into()]).unwrap(), ["a"]);
        assert_eq!(row(&state, "b", "x").0, "unavailable", "only in a");
        assert_eq!(ignore(&state.conn, "x", &[]).unwrap(), ["b"]);
        assert_eq!(
            row(&state, "a", "x"),
            ("ignored".into(), 1, None, None),
            "never retried"
        );
        assert_eq!(queued(&state, &["a"], later), vec!["a/y", "a/z"]);
        // Indexing again leaves it alone.
        seed(&mut state, "a", &["x", "y", "z"]);
        assert_eq!(row(&state, "a", "x").0, "ignored");

        // Bulk: every failed video of a source.
        mark_failed(&state.conn, "a", "z", "HTTP Error 429", false, now()).unwrap();
        assert_eq!(ignore_all(&state.conn, "a", status::FAILED).unwrap(), 1);
        assert_eq!(row(&state, "a", "z").0, "ignored");
        assert_eq!(ignore_all(&state.conn, "a", status::PENDING).unwrap(), 0);
        assert_eq!(row(&state, "a", "y").0, "pending");

        // Brought back: downloaded again like a new video.
        assert!(unignore(&state.conn, "y", &[]).unwrap().is_empty());
        assert_eq!(unignore(&state.conn, "x", &[]).unwrap(), ["a", "b"]);
        assert_eq!(row(&state, "a", "x"), ("pending".into(), 0, None, None));
        assert_eq!(queued(&state, &["a"], now()), vec!["a/x", "a/y"]);
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
        let done = Done {
            files: &files,
            metadata: "{}",
            format: "bv*+ba/b",
            media_bytes: 1234,
        };
        mark_done(&state.conn, "a", "x", done, now()).unwrap();
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
