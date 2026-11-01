//! The YouTube Data API v3 indexer: every page of `playlistItems.list`
//! (1 unit per 50 videos), then `videos.list` (1 unit per 50 videos) only for
//! videos tidepool doesn't know in full yet.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use jiff::Timestamp;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{Availability, RemoteItem, RemoteList, Thumbnail};
use crate::shutdown::Shutdown;

const API: &str = "https://www.googleapis.com/youtube/v3";
const PLAYLIST_PARTS: &str = "snippet,contentDetails,status";
const VIDEO_PARTS: &str = "snippet,contentDetails,status,liveStreamingDetails";
/// Tries for a request that fails with 429, 5xx or a network error.
const ATTEMPTS: u32 = 3;

/// Why the API couldn't list a source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The key's daily quota is used up (by tidepool's count or YouTube's).
    #[error("the YouTube API quota is used up for today")]
    Quota,
    /// YouTube rejected the key itself.
    #[error("YouTube rejected the API key: {0}")]
    KeyRejected(String),
    /// YouTube kept failing (5xx, 429, network) after retries.
    #[error("the YouTube API keeps failing: {0}")]
    Unavailable(String),
    /// This source can't be read through the API (private, wrong id); yt-dlp
    /// may still read it (e.g. with cookies).
    #[error("{0}")]
    Source(String),
    #[error("stopped")]
    Stopped,
}

impl ApiError {
    /// Whether the key shouldn't be used again in this run.
    pub fn disables_key(&self) -> bool {
        matches!(
            self,
            ApiError::Quota | ApiError::KeyRejected(_) | ApiError::Unavailable(_)
        )
    }
}

/// A client for one API key.
pub struct Api {
    client: reqwest::Client,
    base: String,
    key: String,
    /// Units this client may still spend today (`daily_quota` minus what
    /// was already spent).
    budget: u32,
    spent: AtomicU32,
    min_interval: Duration,
    last_request: Mutex<Option<tokio::time::Instant>>,
    backoff: Duration,
}

impl Api {
    pub fn new(key: &str, budget: u32, requests_per_second: u32) -> Self {
        #[allow(unused_mut)]
        let mut base = API.to_string();
        // Debug builds (tests) can point at a fake API.
        #[cfg(debug_assertions)]
        if let Ok(fake) = std::env::var("TIDEPOOL_TEST_YOUTUBE_API") {
            base = fake;
        }
        Self::with(
            base,
            key,
            budget,
            requests_per_second,
            Duration::from_secs(1),
        )
    }

    pub fn with(
        base: String,
        key: &str,
        budget: u32,
        requests_per_second: u32,
        backoff: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("tidepool/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()
            .expect("HTTP client builds");
        Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            budget,
            spent: AtomicU32::new(0),
            min_interval: Duration::from_secs_f64(1.0 / f64::from(requests_per_second.max(1))),
            last_request: Mutex::new(None),
            backoff,
        }
    }

    /// Units spent by this client so far.
    pub fn spent(&self) -> u32 {
        self.spent.load(Ordering::SeqCst)
    }

    /// One GET; every attempt costs a unit, as YouTube counts them.
    async fn get(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        shutdown: &Shutdown,
    ) -> Result<Value, ApiError> {
        let url = format!("{}/{endpoint}", self.base);
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            if shutdown.is_stopping() {
                return Err(ApiError::Stopped);
            }
            if self.spent() >= self.budget {
                return Err(ApiError::Quota);
            }
            self.pace().await;
            self.spent.fetch_add(1, Ordering::SeqCst);
            let full = reqwest::Url::parse_with_params(
                &url,
                params.iter().copied().chain([("key", self.key.as_str())]),
            )
            .map_err(|e| ApiError::Source(format!("bad API address {url}: {e}")))?;
            let request = self.client.get(full).header("Accept", "application/json");
            let response = tokio::select! {
                response = request.send() => response,
                _ = shutdown.stopping() => return Err(ApiError::Stopped),
            };
            match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let body: Value = response.json().await.unwrap_or(Value::Null);
                    if status == 200 {
                        return Ok(body);
                    }
                    match classify(status, &body) {
                        Some(error) => return Err(error),
                        None => last = format!("HTTP {status}: {}", message(&body)),
                    }
                }
                Err(e) => last = redact(&e.to_string(), &self.key),
            }
            if attempt < ATTEMPTS {
                let wait = self.backoff * 2u32.pow(attempt - 1);
                tracing::debug!("YouTube API: {last}; trying again in {wait:?}");
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = shutdown.stopping() => return Err(ApiError::Stopped),
                }
            }
        }
        Err(ApiError::Unavailable(last))
    }

    /// Keep to `requests_per_second`.
    async fn pace(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let next = previous + self.min_interval;
            if next > tokio::time::Instant::now() {
                tokio::time::sleep_until(next).await;
            }
        }
        *last = Some(tokio::time::Instant::now());
    }

    /// Every entry of a playlist, in order.
    pub async fn playlist(
        &self,
        playlist_id: &str,
        shutdown: &Shutdown,
    ) -> Result<Vec<Entry>, ApiError> {
        let mut entries = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut params = vec![
                ("part", PLAYLIST_PARTS),
                ("maxResults", "50"),
                ("playlistId", playlist_id),
            ];
            if let Some(token) = &token {
                params.push(("pageToken", token));
            }
            let page = self.get("playlistItems", &params, shutdown).await?;
            for item in page["items"].as_array().into_iter().flatten() {
                if let Some(entry) = Entry::parse(item) {
                    entries.push(entry);
                }
            }
            match page["nextPageToken"].as_str() {
                Some(next) => token = Some(next.to_string()),
                None => return Ok(entries),
            }
        }
    }

    /// A channel's id and name, by `@handle` or `UC…` id; `None` if YouTube
    /// has no such channel.
    pub async fn channel(
        &self,
        reference: &str,
        shutdown: &Shutdown,
    ) -> Result<Option<(String, String)>, ApiError> {
        let by = if reference.starts_with('@') {
            "forHandle"
        } else {
            "id"
        };
        let body = self
            .get(
                "channels",
                &[("part", "snippet,contentDetails"), (by, reference)],
                shutdown,
            )
            .await?;
        Ok(body["items"][0]["id"].as_str().map(|id| {
            let title = body["items"][0]["snippet"]["title"]
                .as_str()
                .unwrap_or_default();
            (id.to_string(), title.to_string())
        }))
    }

    /// Details for videos, 50 per request. Videos YouTube doesn't return
    /// (private, deleted) are left out.
    pub async fn videos(
        &self,
        ids: &[String],
        shutdown: &Shutdown,
    ) -> Result<HashMap<String, Video>, ApiError> {
        let mut out = HashMap::new();
        for batch in ids.chunks(50) {
            let joined = batch.join(",");
            let body = self
                .get(
                    "videos",
                    &[("part", VIDEO_PARTS), ("id", &joined)],
                    shutdown,
                )
                .await?;
            for item in body["items"].as_array().into_iter().flatten() {
                if let Some(video) = Video::parse(item) {
                    out.insert(video.id.clone(), video);
                }
            }
        }
        Ok(out)
    }
}

/// What an error answer means; `None` when it's worth trying again.
fn classify(status: u16, body: &Value) -> Option<ApiError> {
    let reasons: Vec<&str> = body["error"]["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["reason"].as_str())
        .collect();
    let has = |r: &str| reasons.contains(&r);
    let text = message(body);
    if has("quotaExceeded") || has("dailyLimitExceeded") {
        return Some(ApiError::Quota);
    }
    // YouTube answers a bad key with 400 badRequest "API key not valid".
    if text.contains("API key")
        || has("keyInvalid")
        || has("keyExpired")
        || has("accessNotConfigured")
        || has("ipRefererBlocked")
        || has("forbidden") && status == 403 && !has("playlistItemsNotAccessible")
    {
        return Some(ApiError::KeyRejected(text));
    }
    match status {
        429 | 500..=599 => None,
        _ => Some(ApiError::Source(format!(
            "YouTube API: {}",
            if text.is_empty() {
                format!("HTTP {status}")
            } else {
                text
            }
        ))),
    }
}

fn message(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn redact(text: &str, key: &str) -> String {
    if key.is_empty() {
        text.to_string()
    } else {
        text.replace(key, "REDACTED")
    }
}

/// One playlist entry (`playlistItems`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub video_id: String,
    pub title: String,
    pub published_at: Option<Timestamp>,
    pub availability: Availability,
    pub channel: Option<String>,
    pub channel_id: Option<String>,
}

impl Entry {
    fn parse(item: &Value) -> Option<Self> {
        let snippet = &item["snippet"];
        let video_id = item["contentDetails"]["videoId"]
            .as_str()
            .or_else(|| snippet["resourceId"]["videoId"].as_str())?
            .to_string();
        let title = snippet["title"].as_str().unwrap_or_default().to_string();
        let availability = match (title.as_str(), item["status"]["privacyStatus"].as_str()) {
            ("Deleted video", _) => Availability::Deleted,
            ("Private video", _) | (_, Some("private")) => Availability::Private,
            (_, Some("unlisted")) => Availability::Unlisted,
            _ => Availability::Public,
        };
        Some(Self {
            video_id,
            title,
            published_at: item["contentDetails"]["videoPublishedAt"]
                .as_str()
                .and_then(|t| t.parse().ok()),
            availability,
            channel: snippet["videoOwnerChannelTitle"].as_str().map(String::from),
            channel_id: snippet["videoOwnerChannelId"].as_str().map(String::from),
        })
    }
}

/// A video's details (`videos`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub description: String,
    pub channel: Option<String>,
    pub channel_id: Option<String>,
    pub tags: Vec<String>,
    pub duration: Option<u64>,
    pub published_at: Option<Timestamp>,
    /// `live` or `upcoming` while it's streaming or scheduled.
    pub broadcast: Option<Availability>,
    /// Streamed live (now, earlier or scheduled).
    pub was_live: bool,
    pub privacy: Option<String>,
    pub thumbnails: Vec<Thumbnail>,
}

impl Video {
    fn parse(item: &Value) -> Option<Self> {
        let snippet = &item["snippet"];
        let mut thumbnails: Vec<Thumbnail> = snippet["thumbnails"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(_, t)| {
                Some(Thumbnail {
                    url: t["url"].as_str()?.to_string(),
                    width: t["width"].as_u64().map(|w| w as u32),
                    height: t["height"].as_u64().map(|h| h as u32),
                })
            })
            .collect();
        thumbnails.sort_by_key(|t| t.width.unwrap_or(0) * t.height.unwrap_or(0));
        Some(Self {
            id: item["id"].as_str()?.to_string(),
            title: snippet["title"].as_str().unwrap_or_default().to_string(),
            description: snippet["description"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            channel: snippet["channelTitle"].as_str().map(String::from),
            channel_id: snippet["channelId"].as_str().map(String::from),
            tags: snippet["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(String::from))
                .collect(),
            duration: item["contentDetails"]["duration"]
                .as_str()
                .and_then(parse_duration),
            published_at: snippet["publishedAt"].as_str().and_then(|t| t.parse().ok()),
            broadcast: match snippet["liveBroadcastContent"].as_str() {
                Some("live") => Some(Availability::Live),
                Some("upcoming") => Some(Availability::Upcoming),
                _ => None,
            },
            was_live: item.get("liveStreamingDetails").is_some(),
            privacy: item["status"]["privacyStatus"].as_str().map(String::from),
            thumbnails,
        })
    }
}

/// An ISO 8601 duration (`PT1H2M3S`, `P1DT2H`, `P0D`) in seconds; `None`
/// for zero (upcoming streams report `P0D`).
pub fn parse_duration(text: &str) -> Option<u64> {
    let rest = text.strip_prefix('P')?;
    let mut seconds = 0u64;
    let mut number = String::new();
    let mut in_time = false;
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => number.push(c),
            unit => {
                let n: u64 = number.parse().ok()?;
                number.clear();
                seconds += n * match (unit, in_time) {
                    ('W', false) => 7 * 86_400,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    (seconds > 0).then_some(seconds)
}

/// Combine a playlist entry with its details (fresh from `videos`, or
/// known from an earlier index) into what downstream code uses.
pub fn remote_item(entry: &Entry, position: u32, video: Option<&Video>) -> RemoteItem {
    let Some(video) = video else {
        return RemoteItem {
            video_id: entry.video_id.clone(),
            position,
            title: entry.title.clone(),
            channel: entry.channel.clone(),
            channel_id: entry.channel_id.clone(),
            description: None,
            tags: None,
            duration: None,
            published_at: entry.published_at,
            availability: entry.availability,
            thumbnails: Vec::new(),
        };
    };
    let availability = match (entry.availability, video.broadcast) {
        (Availability::Public | Availability::Unlisted, Some(broadcast)) => broadcast,
        (availability, _) => availability,
    };
    RemoteItem {
        video_id: entry.video_id.clone(),
        position,
        title: video.title.clone(),
        channel: video.channel.clone().or_else(|| entry.channel.clone()),
        channel_id: video
            .channel_id
            .clone()
            .or_else(|| entry.channel_id.clone()),
        description: Some(video.description.clone()),
        tags: Some(video.tags.clone()),
        duration: video.duration,
        published_at: video.published_at.or(entry.published_at),
        availability,
        thumbnails: video.thumbnails.clone(),
    }
}

/// List a playlist through the API: every page, then details for videos
/// `known` doesn't have in full.
pub async fn list(
    api: &Api,
    playlist_id: &str,
    known: impl Fn(&[String]) -> HashMap<String, RemoteItem>,
    skip_details: impl Fn(&Entry) -> bool,
    shutdown: &Shutdown,
) -> Result<RemoteList, ApiError> {
    let entries = api.playlist(playlist_id, shutdown).await?;
    let ids: Vec<String> = entries.iter().map(|e| e.video_id.clone()).collect();
    let known = known(&ids);
    // Private and deleted entries have no details to fetch.
    let wanted: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.availability.downloadable() && !known.contains_key(&e.video_id) && !skip_details(e)
        })
        .map(|e| e.video_id.clone())
        .collect();
    let mut fetched = api.videos(&wanted, shutdown).await?;
    let mut details: BTreeMap<usize, RemoteItem> = BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let position = i as u32 + 1;
        let item = match (fetched.remove(&entry.video_id), known.get(&entry.video_id)) {
            (Some(video), _) => remote_item(entry, position, Some(&video)),
            (None, Some(stored)) => RemoteItem {
                position,
                // The playlist is current about privacy; the rest is stored.
                availability: if entry.availability.downloadable() {
                    stored.availability
                } else {
                    entry.availability
                },
                ..stored.clone()
            },
            // Asked for, but not returned: private or deleted since.
            (None, None) if wanted.contains(&entry.video_id) => RemoteItem {
                availability: Availability::Private,
                ..remote_item(entry, position, None)
            },
            (None, None) => remote_item(entry, position, None),
        };
        details.insert(i, item);
    }
    Ok(RemoteList {
        id: playlist_id.to_string(),
        title: None,
        items: details.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::youtube_fake as fake;

    fn api(server: &wiremock::MockServer, budget: u32) -> Api {
        Api::with(server.uri(), "AIzaTestKey", budget, 1000, Duration::ZERO)
    }

    #[tokio::test]
    async fn lists_every_page_then_details_for_unknown_videos() {
        let server = fake::start().await;
        let api = api(&server, 10_000);
        let shutdown = Shutdown::default();
        let list = list(
            &api,
            fake::UPLOADS,
            |_| HashMap::new(),
            |_| false,
            &shutdown,
        )
        .await
        .unwrap();
        assert_eq!(list.items.len(), 587);
        assert_eq!(api.spent(), 24, "12 pages + 12 detail batches");
        assert!(
            list.items
                .iter()
                .enumerate()
                .all(|(i, item)| item.position == i as u32 + 1)
        );
        let first = &list.items[0];
        assert_eq!(first.duration, Some(37));
        assert!(first.description.is_some() && first.tags.is_some());
        assert_eq!(
            first.thumbnails.last().unwrap().width,
            Some(1280),
            "largest last"
        );
        assert_eq!(
            list.items
                .iter()
                .filter(|i| i.availability == Availability::Upcoming)
                .count(),
            2,
            "scheduled streams"
        );

        // Known videos cost nothing more: only the pages are read.
        let api = self::api(&server, 10_000);
        let known: HashMap<String, RemoteItem> = list
            .items
            .iter()
            .map(|i| (i.video_id.clone(), i.clone()))
            .collect();
        let again = super::list(&api, fake::UPLOADS, |_| known.clone(), |_| false, &shutdown)
            .await
            .unwrap();
        assert_eq!(api.spent(), 12);
        assert_eq!(again.items, list.items);
    }

    #[tokio::test]
    async fn reads_an_unlisted_playlist() {
        let server = fake::start().await;
        let api = api(&server, 10_000);
        let list = list(
            &api,
            fake::PLAYLIST,
            |_| HashMap::new(),
            |_| false,
            &Shutdown::default(),
        )
        .await
        .unwrap();
        assert_eq!(list.items.len(), 10);
        assert_eq!(api.spent(), 2);
        assert_eq!(list.items[0].title, "SCATOLOVE - O TIRO (Lyric Video)");
    }

    #[tokio::test]
    async fn stops_at_the_daily_budget() {
        let server = fake::start().await;
        let api = api(&server, 5);
        let err = list(
            &api,
            fake::UPLOADS,
            |_| HashMap::new(),
            |_| false,
            &Shutdown::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err, ApiError::Quota);
        assert_eq!(api.spent(), 5, "nothing past the budget");
    }

    #[tokio::test]
    async fn retries_server_errors_and_reports_rejections() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/playlistItems"))
            .respond_with(fake::error(503, "backendError", "Backend Error"))
            .mount(&server)
            .await;
        let api = api(&server, 100);
        let err = api.playlist("PLx", &Shutdown::default()).await.unwrap_err();
        assert!(
            matches!(err, ApiError::Unavailable(ref m) if m.contains("503")),
            "{err}"
        );
        assert_eq!(api.spent(), 3, "three tries, each counted");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(fake::key_invalid())
            .mount(&server)
            .await;
        let api = self::api(&server, 100);
        let err = api.playlist("PLx", &Shutdown::default()).await.unwrap_err();
        assert!(matches!(err, ApiError::KeyRejected(ref m) if m.contains("API key not valid")));
        assert_eq!(api.spent(), 1, "not retried");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("PT37S"), Some(37));
        assert_eq!(parse_duration("PT1M1S"), Some(61));
        assert_eq!(parse_duration("PT1H2M3S"), Some(3723));
        assert_eq!(parse_duration("P1DT2H"), Some(93_600));
        assert_eq!(parse_duration("P0D"), None, "upcoming streams");
        assert_eq!(parse_duration("1:00"), None);
    }

    fn body(status: u16, reason: &str, message: &str) -> (u16, Value) {
        (
            status,
            serde_json::json!({ "error": { "code": status, "message": message,
                "errors": [{ "reason": reason, "message": message }] } }),
        )
    }

    #[test]
    fn classifies_errors_as_youtube_sends_them() {
        let (s, b) = body(
            400,
            "badRequest",
            "API key not valid. Please pass a valid API key.",
        );
        assert!(matches!(classify(s, &b), Some(ApiError::KeyRejected(_))));
        let (s, b) = body(
            403,
            "quotaExceeded",
            "The request cannot be completed because you have exceeded your quota.",
        );
        assert_eq!(classify(s, &b), Some(ApiError::Quota));
        let (s, b) = body(
            403,
            "accessNotConfigured",
            "YouTube Data API v3 has not been used in project 1",
        );
        assert!(matches!(classify(s, &b), Some(ApiError::KeyRejected(_))));
        let (s, b) = body(
            404,
            "playlistNotFound",
            "The playlist identified with the request's playlistId parameter cannot be found.",
        );
        assert!(matches!(classify(s, &b), Some(ApiError::Source(_))));
        let (s, b) = body(
            403,
            "playlistItemsNotAccessible",
            "The request is not properly authorized",
        );
        assert!(matches!(classify(s, &b), Some(ApiError::Source(_))));
        let (s, b) = body(503, "backendError", "Backend Error");
        assert_eq!(classify(s, &b), None, "worth another try");
        assert_eq!(classify(429, &Value::Null), None);
    }
}
