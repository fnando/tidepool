//! MusicBrainz ids for music videos (`embed.musicbrainz`). Given the
//! artist's id (the `MUSICBRAINZ_ARTISTID` tag) and the video's title, find
//! the track on one of the artist's releases, using only browse requests
//! (`/ws/2/release?artist=…`), which work on mirrors without a search server
//! too. Each artist's releases are cached for 30 days.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_SERVER: &str = "https://musicbrainz.org";
const CACHE_FOR: Duration = Duration::from_secs(30 * 86_400);
/// musicbrainz.org allows one request per second.
const OFFICIAL_SPACING: Duration = Duration::from_millis(1100);

/// One of an artist's releases, with what matching needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    pub title: String,
    /// `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
    pub date: Option<String>,
    pub group_id: Option<String>,
    /// Album, Single, EP, …
    pub primary_type: Option<String>,
    /// Compilation, Live, …
    pub secondary_types: Vec<String>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub recording_id: String,
    pub title: String,
}

/// What a lookup found.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    Match {
        release: Box<Release>,
        track: Track,
    },
    /// Nothing matched; why.
    None(String),
}

impl Found {
    /// The tags to add (MusicBrainz Picard's names).
    pub fn tags(&self) -> Vec<(&'static str, String)> {
        match self {
            Found::Match { release, track } => {
                let mut tags = vec![
                    ("MUSICBRAINZ_ALBUMID", release.id.clone()),
                    ("MUSICBRAINZ_TRACKID", track.recording_id.clone()),
                    ("MUSICBRAINZ_RELEASETRACKID", track.id.clone()),
                ];
                if let Some(group) = &release.group_id {
                    tags.push(("MUSICBRAINZ_RELEASEGROUPID", group.clone()));
                }
                tags
            }
            Found::None(_) => Vec::new(),
        }
    }

    /// What metadata.json records (`musicbrainz`).
    pub fn record(&self, artist_id: &str) -> Value {
        match self {
            Found::Match { release, track } => serde_json::json!({
                "artist_id": artist_id,
                "release_id": release.id,
                "release_group_id": release.group_id,
                "recording_id": track.recording_id,
                "track_id": track.id,
                "release": release.title,
                "release_date": release.date,
                "track": track.title,
            }),
            Found::None(why) => serde_json::json!({ "artist_id": artist_id, "no_match": why }),
        }
    }

    /// Tags from what metadata.json recorded for an earlier match.
    pub fn tags_from_record(record: &Value) -> Option<Vec<(&'static str, String)>> {
        let get = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
        let mut tags = vec![
            ("MUSICBRAINZ_ALBUMID", get("release_id")?),
            ("MUSICBRAINZ_TRACKID", get("recording_id")?),
            ("MUSICBRAINZ_RELEASETRACKID", get("track_id")?),
        ];
        if let Some(group) = get("release_group_id") {
            tags.push(("MUSICBRAINZ_RELEASEGROUPID", group));
        }
        Some(tags)
    }
}

/// Find `title` among the artist's releases.
pub async fn lookup(
    server: &str,
    data_dir: &Path,
    artist_id: &str,
    title: &str,
    artist_name: Option<&str>,
    published: Option<jiff::civil::Date>,
) -> Result<Found, String> {
    let releases = artist_releases(server, data_dir, artist_id).await?;
    Ok(pick(&releases, title, artist_name, published))
}

/// Match a video title against the releases, and pick one release.
pub fn pick(
    releases: &[Release],
    title: &str,
    artist_name: Option<&str>,
    published: Option<jiff::civil::Date>,
) -> Found {
    let wanted = normalize(&clean_title(title, artist_name));
    if wanted.is_empty() {
        return Found::None("the title is empty once cleaned".into());
    }
    let with_track: Vec<(&Release, &Track)> = releases
        .iter()
        .filter_map(|r| {
            r.tracks
                .iter()
                .find(|t| normalize(&t.title) == wanted)
                .map(|t| (r, t))
        })
        .collect();
    if with_track.is_empty() {
        return Found::None(format!(
            "no track called “{}” on the artist's {} releases",
            clean_title(title, artist_name),
            releases.len()
        ));
    }
    // Compilations, live albums and the like only when nothing else has it.
    let regular: Vec<(&Release, &Track)> = with_track
        .iter()
        .copied()
        .filter(|(r, _)| r.secondary_types.is_empty())
        .collect();
    let pool = if regular.is_empty() {
        with_track
    } else {
        regular
    };
    let rank = |r: &Release| match r.primary_type.as_deref() {
        Some("Album") => 0,
        Some("EP") => 1,
        Some("Single") => 2,
        _ => 3,
    };
    let date = |r: &Release| r.date.as_deref().and_then(|d| parse_date(d, false));
    // The latest release on or before the video came out (a release dated
    // only "1998" counts as out by the end of 1998); if the video came
    // first, the earliest one after it.
    let chosen = match published {
        Some(day) => {
            let before = pool
                .iter()
                .filter(|(r, _)| {
                    r.date
                        .as_deref()
                        .and_then(|d| parse_date(d, true))
                        .is_some_and(|d| d <= day)
                })
                .max_by(|(a, _), (b, _)| date(a).cmp(&date(b)).then(rank(b).cmp(&rank(a))));
            before.or_else(|| {
                pool.iter().min_by(|(a, _), (b, _)| {
                    date(a)
                        .is_none()
                        .cmp(&date(b).is_none())
                        .then(date(a).cmp(&date(b)))
                        .then(rank(a).cmp(&rank(b)))
                })
            })
        }
        None => pool.iter().min_by(|(a, _), (b, _)| {
            rank(a)
                .cmp(&rank(b))
                .then(date(a).is_none().cmp(&date(b).is_none()))
                .then(date(a).cmp(&date(b)))
        }),
    };
    let (release, track) = chosen.expect("a non-empty pool");
    Found::Match {
        release: Box::new((*release).clone()),
        track: (*track).clone(),
    }
}

/// `2019`, `2019-05` or `2019-05-10` as a date: when partial, its first
/// day, or its last with `end`.
fn parse_date(text: &str, end: bool) -> Option<jiff::civil::Date> {
    let mut parts = text.split('-');
    let year: i16 = parts.next()?.parse().ok()?;
    let month: Option<i8> = parts.next().and_then(|m| m.parse().ok());
    let day: Option<i8> = parts.next().and_then(|d| d.parse().ok());
    let month_of = month.unwrap_or(if end { 12 } else { 1 });
    let first = jiff::civil::Date::new(year, month_of, 1).ok()?;
    match (day, end) {
        (Some(day), _) => jiff::civil::Date::new(year, month_of, day).ok(),
        (None, false) => Some(first),
        (None, true) => Some(first.last_of_month()),
    }
}

/// Words in brackets that mark a video, not part of the song's title.
const VIDEO_WORDS: [&str; 16] = [
    "official",
    "video",
    "clipe",
    "clip",
    "lyric",
    "lyrics",
    "audio",
    "visualizer",
    "visualiser",
    "remaster",
    "remastered",
    "4k",
    "hd",
    "hq",
    "mv",
    "oficial",
];

/// A video title without what isn't the song's: the artist in front
/// ("Artist - Song") and brackets like "(Official Video)".
pub fn clean_title(title: &str, artist: Option<&str>) -> String {
    let mut text = title.trim().to_string();
    if let Some(artist) = artist.filter(|a| !a.is_empty())
        && let Some((front, rest)) = text.split_once(" - ")
        && normalize(front) == normalize(artist)
    {
        text = rest.trim().to_string();
    }
    // Drop bracketed parts that only describe the video.
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find(['(', '[']) {
        let close = if rest[start..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(len) = rest[start..].find(close) else {
            break;
        };
        let inside = &rest[start + 1..start + len];
        let words = normalize(inside);
        let describes_video = words.split(' ').any(|w| VIDEO_WORDS.contains(&w));
        out.push_str(&rest[..start]);
        if !describes_video {
            out.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// For comparing titles: ASCII, lowercase, letters and digits only.
pub fn normalize(text: &str) -> String {
    deunicode::deunicode(text)
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Serialize, Deserialize)]
struct Cached {
    fetched_at: jiff::Timestamp,
    releases: Vec<Release>,
}

fn cache_file(server: &str, data_dir: &Path, artist_id: &str) -> PathBuf {
    let host: String = server
        .split("://")
        .last()
        .unwrap_or(server)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir
        .join("cache/musicbrainz")
        .join(host)
        .join(format!("{artist_id}.json"))
}

/// The artist's official releases, from the cache when it's fresh.
async fn artist_releases(
    server: &str,
    data_dir: &Path,
    artist_id: &str,
) -> Result<Vec<Release>, String> {
    let path = cache_file(server, data_dir, artist_id);
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(cached) = serde_json::from_str::<Cached>(&text)
        && jiff::Timestamp::now()
            .duration_since(cached.fetched_at)
            .unsigned_abs()
            < CACHE_FOR
    {
        return Ok(cached.releases);
    }
    let mut releases = Vec::new();
    loop {
        let url = format!(
            "{}/ws/2/release?artist={artist_id}&inc=recordings+release-groups&status=official&limit=100&offset={}&fmt=json",
            server.trim_end_matches('/'),
            releases.len()
        );
        let page = get(server, &url).await?;
        let total = page["release-count"].as_u64().unwrap_or(0) as usize;
        let got: Vec<Release> = page["releases"]
            .as_array()
            .into_iter()
            .flatten()
            .map(release_of)
            .collect();
        if got.is_empty() {
            break;
        }
        releases.extend(got);
        if releases.len() >= total {
            break;
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let cached = Cached {
        fetched_at: jiff::Timestamp::now(),
        releases,
    };
    let _ = std::fs::write(&path, serde_json::to_vec(&cached).expect("JSON"));
    Ok(cached.releases)
}

fn release_of(r: &Value) -> Release {
    let text = |v: &Value| v.as_str().map(str::to_string);
    let group = &r["release-group"];
    Release {
        id: text(&r["id"]).unwrap_or_default(),
        title: text(&r["title"]).unwrap_or_default(),
        date: text(&r["date"]).filter(|d| !d.is_empty()),
        group_id: text(&group["id"]),
        primary_type: text(&group["primary-type"]),
        secondary_types: group["secondary-types"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect(),
        tracks: r["media"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["tracks"].as_array().into_iter().flatten())
            .filter_map(|t| {
                Some(Track {
                    id: text(&t["id"])?,
                    recording_id: text(&t["recording"]["id"])?,
                    title: text(&t["title"])?,
                })
            })
            .collect(),
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!(
                "tidepool/",
                env!("CARGO_PKG_VERSION"),
                " (self-hosted video mirror)"
            ))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("an HTTP client")
    })
}

/// Wait for our turn: musicbrainz.org takes one request per second from
/// the whole process; other servers (mirrors) don't wait.
async fn wait_turn(server: &str) {
    let official = server.contains("musicbrainz.org");
    if !official {
        return;
    }
    static NEXT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let wait = {
        let mut next = NEXT
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let slot = next.get(server).copied().unwrap_or(now).max(now);
        next.insert(server.to_string(), slot + OFFICIAL_SPACING);
        slot - now
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// GET JSON, retrying when the server says to slow down (503).
async fn get(server: &str, url: &str) -> Result<Value, String> {
    let mut backoff = Duration::from_secs(2);
    for attempt in 0..4 {
        wait_turn(server).await;
        let response = http()
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("MusicBrainz ({server}): {e}"))?;
        let status = response.status();
        if status.as_u16() == 503 && attempt < 3 {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
            continue;
        }
        if !status.is_success() {
            return Err(format!("MusicBrainz ({server}) answered {status}"));
        }
        return response
            .json()
            .await
            .map_err(|e| format!("MusicBrainz ({server}): unexpected answer: {e}"));
    }
    Err(format!("MusicBrainz ({server}) is busy (503)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(
        id: &str,
        title: &str,
        date: &str,
        kind: &str,
        secondary: &[&str],
        tracks: &[&str],
    ) -> Release {
        Release {
            id: id.into(),
            title: title.into(),
            date: Some(date.into()),
            group_id: Some(format!("{id}-group")),
            primary_type: Some(kind.into()),
            secondary_types: secondary.iter().map(|s| s.to_string()).collect(),
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, t)| Track {
                    id: format!("{id}-t{i}"),
                    recording_id: format!("rec-{}", normalize(t)),
                    title: t.to_string(),
                })
                .collect(),
        }
    }

    fn day(s: &str) -> Option<jiff::civil::Date> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn cleans_video_titles() {
        let clean = |t| clean_title(t, Some("Dead Fish"));
        assert_eq!(
            clean("Dead Fish - Sonho Médio (Clipe Oficial)"),
            "Sonho Médio"
        );
        assert_eq!(
            clean("Sonho Médio [Official Music Video] [4K]"),
            "Sonho Médio"
        );
        assert_eq!(
            clean("Molotov (Ao Vivo)"),
            "Molotov (Ao Vivo)",
            "not about the video"
        );
        assert_eq!(
            clean("Other Band - Song"),
            "Other Band - Song",
            "another artist"
        );
        assert_eq!(normalize("Sonho  Médio!"), "sonho medio");
    }

    #[test]
    fn picks_the_latest_release_before_the_video() {
        let releases = vec![
            release(
                "album",
                "Sirva-se",
                "1998",
                "Album",
                &[],
                &["Molotov", "Compra!"],
            ),
            release(
                "best",
                "Best Of",
                "2009",
                "Album",
                &["Compilation"],
                &["Molotov"],
            ),
            release(
                "reissue",
                "Sirva-se (Deluxe)",
                "2012-03-01",
                "Album",
                &[],
                &["Molotov"],
            ),
            release("single", "Compra!", "1997-11", "Single", &[], &["Compra!"]),
        ];
        let found = |title, date| match pick(&releases, title, Some("Dead Fish"), date) {
            Found::Match { release, .. } => release.id,
            Found::None(why) => format!("none: {why}"),
        };
        assert_eq!(
            found("Dead Fish - Molotov (Official Video)", day("2010-01-01")),
            "album",
            "not the compilation"
        );
        assert_eq!(found("Molotov", day("2020-01-01")), "reissue");
        assert_eq!(
            found("Molotov", day("1990-01-01")),
            "album",
            "the video came first"
        );
        assert_eq!(found("Compra!", day("1998-01-01")), "single");
        assert_eq!(found("Compra", None), "album", "no date: albums first");
        assert!(found("Unknown Song", None).starts_with("none: no track called “Unknown Song”"));

        // Only a compilation has it: that's still a match.
        let only = vec![release(
            "best",
            "Best Of",
            "2009",
            "Album",
            &["Compilation"],
            &["Rare"],
        )];
        assert!(matches!(
            pick(&only, "Rare", None, None),
            Found::Match { .. }
        ));
    }

    #[test]
    fn partial_dates() {
        assert_eq!(parse_date("1998", false), day("1998-01-01"));
        assert_eq!(parse_date("1998", true), day("1998-12-31"));
        assert_eq!(parse_date("2024-02", true), day("2024-02-29"));
        assert_eq!(parse_date("2001-07-04", true), day("2001-07-04"));
        assert_eq!(parse_date("", false), None);
    }

    #[test]
    fn tags_and_records() {
        let r = release("rel", "Afasia", "2001", "Album", &[], &["Afasia"]);
        let found = Found::Match {
            track: r.tracks[0].clone(),
            release: Box::new(r),
        };
        let tags = found.tags();
        assert!(tags.contains(&("MUSICBRAINZ_ALBUMID", "rel".into())));
        assert!(tags.contains(&("MUSICBRAINZ_RELEASEGROUPID", "rel-group".into())));
        assert!(tags.contains(&("MUSICBRAINZ_TRACKID", "rec-afasia".into())));
        assert!(tags.contains(&("MUSICBRAINZ_RELEASETRACKID", "rel-t0".into())));
        let record = found.record("artist");
        assert_eq!(Found::tags_from_record(&record).unwrap(), tags);
        assert_eq!(
            Found::tags_from_record(&Found::None("x".into()).record("a")),
            None
        );
    }

    #[tokio::test]
    async fn browses_pages_and_caches_them() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let page = |offset: usize, ids: &[&str]| {
            serde_json::json!({
                "release-count": 3,
                "release-offset": offset,
                "releases": ids.iter().map(|id| serde_json::json!({
                    "id": id, "title": "Afasia", "date": "2001",
                    "release-group": {"id": "g", "primary-type": "Album", "secondary-types": []},
                    "media": [{"tracks": [{"id": format!("{id}-t"), "title": "Afasia", "recording": {"id": "r"}}]}]
                })).collect::<Vec<_>>()
            })
        };
        Mock::given(path("/ws/2/release"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(0, &["a", "b"])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/ws/2/release"))
            .and(query_param("offset", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(2, &["c"])))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let releases = artist_releases(&server.uri(), dir.path(), "artist")
            .await
            .unwrap();
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[2].tracks[0].recording_id, "r");
        // Cached: no more requests (the mocks expect one call each).
        let again = artist_releases(&server.uri(), dir.path(), "artist")
            .await
            .unwrap();
        assert_eq!(again, releases);
        let found = lookup(
            &server.uri(),
            dir.path(),
            "artist",
            "Afasia (Official Video)",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(found, Found::Match { .. }));
    }

    #[tokio::test]
    async fn a_busy_server_is_tried_again_then_reported() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(path("/ws/2/release"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let err = artist_releases(&server.uri(), dir.path(), "artist")
            .await
            .unwrap_err();
        assert!(err.contains("answered 500"), "{err}");
    }

    #[tokio::test]
    async fn musicbrainz_org_is_asked_once_a_second() {
        let start = Instant::now();
        for _ in 0..3 {
            wait_turn("https://musicbrainz.org.test").await;
        }
        assert!(
            start.elapsed() >= Duration::from_millis(2200),
            "{:?}",
            start.elapsed()
        );
        let start = Instant::now();
        for _ in 0..3 {
            wait_turn("http://mirror.local").await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "mirrors don't wait"
        );
    }
}
