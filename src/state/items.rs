//! Recording what indexing found: sources, videos and each video's status in
//! a source.

use std::collections::{BTreeMap, HashSet};

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

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
        let tags = item
            .tags
            .as_ref()
            .map(|t| serde_json::to_string(t).expect("tags serialize"));
        let thumbs = (!item.thumbnails.is_empty())
            .then(|| serde_json::to_string(&item.thumbnails).expect("thumbnails serialize"));
        tx.execute(
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

        let existing: Option<String> = tx
            .query_row(
                "SELECT status FROM items WHERE source_key = ?1 AND video_id = ?2",
                params![source.key, item.video_id],
                |r| r.get(0),
            )
            .optional()?;
        let decide_now = existing
            .as_deref()
            .is_none_or(|s| status::UNDECIDED.contains(&s));
        if existing.is_none() {
            summary.new += 1;
        }
        if decide_now {
            let decision = decide(item);
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
