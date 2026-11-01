//! The sync pipeline's stages, shared by `index` and `run`.

use std::path::PathBuf;

use jiff::Timestamp;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::config::merge::EffectiveSource;
use crate::filter::{Fields, RuleSet};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::index::RemoteList;
use crate::index::api::{self, Api, ApiError};
use crate::index::ytdlp;
use crate::paths::{Dirs, expand};
use crate::state::State;
use crate::state::items;
use crate::state::items::{IndexSummary, SourceRecord, record_index, status};

/// A hash of a source's effective filters, stored with the source so later
/// stages can tell when the rules changed.
pub fn rules_hash(source: &EffectiveSource) -> String {
    let filters = serde_json::json!({
        "include": source.include,
        "exclude": source.exclude,
        "match_fields": source.match_fields,
        "min_duration": source.min_duration,
        "max_duration": source.max_duration,
        "cutoff": source.cutoff,
    });
    Sha256::digest(filters.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `cookies_file`, expanded; `None` when unset.
pub fn cookies_file(source: &EffectiveSource, dirs: &Dirs) -> Result<Option<PathBuf>, String> {
    if source.cookies_file.is_empty() {
        return Ok(None);
    }
    expand(&source.cookies_file, &dirs.home).map(Some)
}

/// Why a source couldn't be indexed.
#[derive(Debug)]
pub enum IndexFailure {
    /// Ctrl-C.
    Stopped,
    Failed(String),
    /// The API can't be used and `fallback_to_ytdlp = false` (exit 4).
    NoFallback(String),
}

impl std::fmt::Display for IndexFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexFailure::Stopped => f.write_str("stopped"),
            IndexFailure::Failed(e) | IndexFailure::NoFallback(e) => f.write_str(e),
        }
    }
}

/// The API clients of one run, one per key, and the keys that stopped
/// working during it.
#[derive(Default)]
pub struct ApiPool {
    clients: HashMap<String, Arc<Api>>,
    /// Units each key had spent today before this run.
    spent_before: HashMap<String, u32>,
    disabled: HashMap<String, ApiError>,
    warned: HashSet<String>,
}

/// An API key's id: the start of its SHA-256, so the key itself is never
/// stored anywhere but the config.
pub fn key_id(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// List a source through the API, if it has a key that still works. `None`
/// means "use yt-dlp".
#[allow(clippy::too_many_arguments)]
async fn list_with_api(
    state: &State,
    source: &EffectiveSource,
    playlist_id: &str,
    rules: &RuleSet,
    pool: &mut ApiPool,
    tracker: &mut crate::problems::Tracker,
    shutdown: &crate::shutdown::Shutdown,
    now: Timestamp,
) -> Result<Option<RemoteList>, IndexFailure> {
    let Some((id, api)) = pool.client(state, source, now)? else {
        return Ok(None);
    };
    let before = api.spent();
    let cutoff = rules.cutoff;
    let result = api::list(
        &api,
        playlist_id,
        |ids| items::known_details(&state.conn, ids).unwrap_or_default(),
        |entry| cutoff.zip(entry.published_at).is_some_and(|(c, p)| p < c),
        shutdown,
    )
    .await;
    pool.spent(state, source, &id, api.spent() - before, now)?;
    match result {
        Ok(list) => {
            tracker.api_succeeded();
            Ok(Some(list))
        }
        Err(e) => pool.failed(source, id, e, tracker).map(|()| None),
    }
}

impl ApiPool {
    /// The client for a source's key; `None` without a key, or when the key
    /// stopped working earlier in this run and yt-dlp may be used instead.
    fn client(
        &mut self,
        state: &State,
        source: &EffectiveSource,
        now: Timestamp,
    ) -> Result<Option<(String, Arc<Api>)>, IndexFailure> {
        let youtube = &source.youtube;
        let Some(key) = youtube.api_key.as_deref() else {
            return Ok(None);
        };
        let id = key_id(key);
        if let Some(e) = self.disabled.get(&id) {
            let masked = crate::config::diff::mask_secret(key);
            tracing::debug!(
                "{}: API key {masked} unavailable this run ({e}); using yt-dlp",
                source.key
            );
            return fallback(source, e).map(|()| None);
        }
        if !self.clients.contains_key(&id) {
            let used = items::quota_used(&state.conn, &id, &items::quota_day(now))
                .map_err(|e| IndexFailure::Failed(format!("state database: {e}")))?;
            let budget = youtube.daily_quota.saturating_sub(used);
            self.spent_before.insert(id.clone(), used);
            self.clients.insert(
                id.clone(),
                Arc::new(Api::new(key, budget, youtube.requests_per_second)),
            );
        }
        Ok(Some((id.clone(), self.clients[&id].clone())))
    }

    /// Record units a source spent; warn once a key nears its quota.
    fn spent(
        &mut self,
        state: &State,
        source: &EffectiveSource,
        id: &str,
        units: u32,
        now: Timestamp,
    ) -> Result<(), IndexFailure> {
        let youtube = &source.youtube;
        let masked = crate::config::diff::mask_secret(youtube.api_key.as_deref().unwrap_or(""));
        items::add_quota(&state.conn, id, &items::quota_day(now), &source.key, units)
            .map_err(|e| IndexFailure::Failed(format!("state database: {e}")))?;
        let total = self.spent_before[id] + self.clients[id].spent();
        tracing::debug!(
            "{}: {units} API units (key {masked}: {total} of {} today)",
            source.key,
            youtube.daily_quota
        );
        if f64::from(total) >= youtube.quota_warn_at * f64::from(youtube.daily_quota)
            && self.warned.insert(id.to_string())
        {
            tracing::warn!(
                "YouTube API key {masked} has used {total} of its {} units today",
                youtube.daily_quota
            );
        }
        Ok(())
    }

    /// The API failed for a source: stop using a key that's out of quota,
    /// rejected or unavailable, and say whether yt-dlp may take over.
    fn failed(
        &mut self,
        source: &EffectiveSource,
        id: String,
        e: ApiError,
        tracker: &mut crate::problems::Tracker,
    ) -> Result<(), IndexFailure> {
        if e == ApiError::Stopped {
            return Err(IndexFailure::Stopped);
        }
        let masked =
            crate::config::diff::mask_secret(source.youtube.api_key.as_deref().unwrap_or(""));
        if e.disables_key() {
            tracing::warn!("{}: {e}; using yt-dlp for the rest of this run", source.key);
            let kind = match &e {
                ApiError::Quota => Some(crate::problems::Kind::ApiQuota),
                ApiError::KeyRejected(_) => Some(crate::problems::Kind::ApiKeyRejected),
                _ => None,
            };
            if let Some(kind) = kind {
                tracker.api_failed(kind, &source.key, &format!("key {masked}: {e}"));
            }
            self.disabled.insert(id, e.clone());
        } else {
            tracing::info!("{}: {e}; trying yt-dlp", source.key);
        }
        fallback(source, &e)
    }
}

/// Whether yt-dlp may stand in for the API.
fn fallback(source: &EffectiveSource, e: &ApiError) -> Result<(), IndexFailure> {
    if source.youtube.fallback_to_ytdlp {
        Ok(())
    } else {
        Err(IndexFailure::NoFallback(format!(
            "{e}; not using yt-dlp instead (fallback_to_ytdlp = false)"
        )))
    }
}

/// A channel source's id and name: remembered from an earlier lookup, or
/// looked up through the API (handles and ids) or yt-dlp.
#[allow(clippy::too_many_arguments)]
async fn resolve_channel(
    state: &State,
    source: &EffectiveSource,
    ytdlp: &crate::tools::Ytdlp,
    cookies: Option<&std::path::Path>,
    pool: &mut ApiPool,
    tracker: &mut crate::problems::Tracker,
    shutdown: &crate::shutdown::Shutdown,
    grace: std::time::Duration,
    now: Timestamp,
) -> Result<(String, Option<String>), IndexFailure> {
    let db = |e: rusqlite::Error| IndexFailure::Failed(format!("state database: {e}"));
    let reference = source.id.as_str();
    if let Some(found) = items::cached_channel(&state.conn, reference).map_err(db)? {
        return Ok(found);
    }
    let by_api = reference.starts_with('@') || reference.starts_with("UC");
    if by_api && let Some((id, api)) = pool.client(state, source, now)? {
        let before = api.spent();
        let result = api.channel(reference, shutdown).await;
        pool.spent(state, source, &id, api.spent() - before, now)?;
        match result {
            Ok(Some((channel_id, title))) => {
                tracker.api_succeeded();
                items::save_channel(&state.conn, reference, &channel_id, Some(&title), now)
                    .map_err(db)?;
                return Ok((channel_id, Some(title)));
            }
            Ok(None) => {
                return Err(IndexFailure::Failed(format!(
                    "YouTube has no channel {reference}"
                )));
            }
            Err(e) => pool.failed(source, id, e, tracker)?,
        }
    }
    let (channel_id, title) = ytdlp::channel(ytdlp, &source.url, cookies, shutdown, grace)
        .await
        .map_err(|e| match e {
            crate::index::IndexError::Stopped => IndexFailure::Stopped,
            e => IndexFailure::Failed(e.to_string()),
        })?;
    items::save_channel(&state.conn, reference, &channel_id, title.as_deref(), now).map_err(db)?;
    Ok((channel_id, title))
}

/// List one source (through the API when it has a key, else with yt-dlp),
/// apply its filters and record the result.
#[allow(clippy::too_many_arguments)]
pub async fn index_source(
    state: &mut State,
    config: &Config,
    source: &EffectiveSource,
    dirs: &Dirs,
    ytdlp_path: &crate::tools::Ytdlp,
    apis: &mut ApiPool,
    tracker: &mut crate::problems::Tracker,
    shutdown: &crate::shutdown::Shutdown,
    now: Timestamp,
) -> Result<IndexSummary, IndexFailure> {
    let failed = IndexFailure::Failed;
    let limit = usize::try_from(config.general.regex_backtrack_limit).unwrap_or(usize::MAX);
    let rules = RuleSet::compile(source, limit, now).map_err(failed)?;
    let cookies = cookies_file(source, dirs).map_err(failed)?;
    let grace = config.general.shutdown_timeout.0;
    // A channel is listed through its uploads playlist (UC… → UU…).
    let (playlist_id, url, channel_title) = if source.channel {
        let (channel_id, title) = resolve_channel(
            state,
            source,
            ytdlp_path,
            cookies.as_deref(),
            apis,
            tracker,
            shutdown,
            grace,
            now,
        )
        .await?;
        let uploads = format!("UU{}", &channel_id[2..]);
        tracing::debug!(
            "{}: channel {} is {channel_id}; listing its uploads ({uploads})",
            source.key,
            source.id
        );
        let url = format!("https://www.youtube.com/playlist?list={uploads}");
        (uploads, url, title)
    } else {
        (source.id.clone(), source.url.clone(), None)
    };
    tracing::debug!("{}: listing {url}", source.key);
    let started = std::time::Instant::now();
    let api_list = list_with_api(
        state,
        source,
        &playlist_id,
        &rules,
        apis,
        tracker,
        shutdown,
        now,
    )
    .await?;
    let (mut list, how) = match api_list {
        Some(list) => (list, "api"),
        None => {
            let list = ytdlp::list(ytdlp_path, &url, cookies.as_deref(), shutdown, grace)
                .await
                .map_err(|e| match e {
                    crate::index::IndexError::Stopped => IndexFailure::Stopped,
                    e => IndexFailure::Failed(e.to_string()),
                })?;
            (list, "ytdlp")
        }
    };
    if channel_title.is_some() {
        list.title = channel_title;
    }
    // Rules on `kind`: settle what can be settled now. The API marks live
    // streams, so through it "not a Short" means a regular video; through
    // yt-dlp a past stream may not be marked, so that waits for the check
    // before download.
    if rules.uses_kind() {
        let ids: Vec<String> = list.items.iter().map(|i| i.video_id.clone()).collect();
        let known = items::known_kinds(&state.conn, &ids).unwrap_or_default();
        for item in list.items.iter_mut().filter(|i| i.kind.is_none()) {
            item.kind = known.get(&item.video_id).copied();
        }
        for item in list.items.iter_mut().filter(|i| i.kind.is_none()) {
            if matches!(
                item.availability,
                crate::index::Availability::Live | crate::index::Availability::Upcoming
            ) {
                item.kind = Some(crate::index::VideoKind::Live);
            }
        }
        crate::index::shorts::ShortsCheck::default()
            .classify(&mut list.items, how == "api", shutdown)
            .await;
    }
    tracing::debug!(
        "{}: listed {} in {:.1}s ({})",
        source.key,
        plural(list.items.len(), "video", "videos"),
        started.elapsed().as_secs_f64(),
        if how == "api" {
            "YouTube API"
        } else {
            "yt-dlp"
        }
    );
    let hash = rules_hash(source);
    let record = SourceRecord {
        key: &source.key,
        rules_hash: &hash,
        index_source: how,
    };
    let summary = record_index(
        &mut state.conn,
        &record,
        &list,
        |item| rules.decide(&Fields::from_item(item)),
        now,
    )
    .map_err(|e| IndexFailure::Failed(format!("cannot record the index: {e}")))?;
    for d in &summary.decisions {
        let line = format!("“{}” [{}]: {}", d.title, d.video_id, d.decision);
        if d.new {
            tracing::debug!("{}: new: {line}", source.key);
        } else if d.changed {
            tracing::debug!("{}: changed: {line}", source.key);
        } else {
            tracing::trace!("{}: {line}", source.key);
        }
    }
    Ok(summary)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// One line per source: `lofi: 12 videos (3 new): 2 to download, 9 skipped (…)`.
pub fn describe_index(key: &str, summary: &IndexSummary) -> String {
    let mut head = format!("{key}: {}", plural(summary.total, "video", "videos"));
    let mut notes = Vec::new();
    if summary.new > 0 {
        notes.push(format!("{} new", summary.new));
    }
    if summary.removed > 0 {
        notes.push(format!("{} gone from the source", summary.removed));
    }
    if !notes.is_empty() {
        head.push_str(&format!(" ({})", notes.join(", ")));
    }

    let mut parts = Vec::new();
    let pending = summary.count(status::PENDING);
    if pending > 0 {
        parts.push(format!("{pending} to download"));
    }
    let check = summary.count(status::PENDING_CHECK);
    if check > 0 {
        parts.push(format!("{check} to check before download"));
    }
    let downloaded = summary.count(status::DONE) + summary.count(status::DOWNLOADED);
    if downloaded > 0 {
        parts.push(format!("{downloaded} already downloaded"));
    }
    let skipped = summary.count(status::FILTERED);
    if skipped > 0 {
        let mut reasons: Vec<(&String, &usize)> = summary.filtered.iter().collect();
        reasons.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.cmp(b)));
        let reasons: Vec<String> = reasons
            .iter()
            .map(|(label, n)| format!("{n} {label}"))
            .collect();
        parts.push(format!("{skipped} skipped ({})", reasons.join(", ")));
    }
    let errors = summary.count(status::FILTER_ERROR);
    if errors > 0 {
        parts.push(format!("{errors} held by a rule error"));
    }
    if parts.is_empty() {
        return head;
    }
    format!("{head}: {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_sources;

    fn source(extra: &str) -> EffectiveSource {
        let config: Config = toml::from_str(&format!(
            "[sources.x]\nurl = 'https://www.youtube.com/playlist?list=PLx'\n{extra}"
        ))
        .unwrap();
        effective_sources(&config).remove(0)
    }

    #[test]
    fn rules_hash_changes_with_filters_only() {
        let base = rules_hash(&source(""));
        assert_eq!(base, rules_hash(&source("format = 'ba'")));
        assert_ne!(base, rules_hash(&source("exclude = ['x']")));
        assert_ne!(base, rules_hash(&source("cutoff = '30d'")));
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn describes_summaries() {
        let mut summary = IndexSummary {
            total: 1,
            ..Default::default()
        };
        assert_eq!(describe_index("a", &summary), "a: 1 video");
        summary.total = 3;
        summary.new = 2;
        summary.removed = 1;
        summary.by_status.insert("pending".into(), 1);
        summary.by_status.insert("done".into(), 1);
        summary.by_status.insert("filtered".into(), 1);
        summary.filtered.insert("too short".into(), 1);
        assert_eq!(
            describe_index("a", &summary),
            "a: 3 videos (2 new, 1 gone from the source): 1 to download, 1 already downloaded, 1 skipped (1 too short)"
        );
    }
}
