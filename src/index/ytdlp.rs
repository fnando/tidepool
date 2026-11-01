//! Indexing with `yt-dlp --flat-playlist -J`: one process per source.

use std::path::Path;

use jiff::Timestamp;
use serde::Deserialize;

use super::{Availability, IndexError, RemoteItem, RemoteList, Thumbnail};

#[derive(Debug, Deserialize)]
struct FlatPlaylist {
    id: String,
    title: Option<String>,
    #[serde(default)]
    entries: Vec<Option<FlatEntry>>,
}

#[derive(Debug, Deserialize)]
struct FlatEntry {
    id: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    timestamp: Option<i64>,
    live_status: Option<String>,
    availability: Option<String>,
    channel: Option<String>,
    channel_id: Option<String>,
    #[serde(default)]
    thumbnails: Option<Vec<FlatThumbnail>>,
}

#[derive(Debug, Deserialize)]
struct FlatThumbnail {
    url: String,
    width: Option<u32>,
    height: Option<u32>,
}

/// Parse `yt-dlp --flat-playlist -J` output.
pub fn parse_flat_playlist(json: &str) -> Result<RemoteList, IndexError> {
    let playlist: FlatPlaylist =
        serde_json::from_str(json).map_err(|e| IndexError::Parse(e.to_string()))?;
    let items = playlist
        .entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let entry = entry?;
            let video_id = entry.id?;
            let title = entry.title.unwrap_or_default();
            Some(RemoteItem {
                availability: availability(
                    &title,
                    entry.availability.as_deref(),
                    entry.live_status.as_deref(),
                ),
                position: u32::try_from(index + 1).unwrap_or(u32::MAX),
                video_id,
                title,
                channel: entry.channel,
                channel_id: entry.channel_id,
                description: None,
                tags: None,
                duration: entry
                    .duration
                    .filter(|d| *d >= 0.0)
                    .map(|d| d.round() as u64),
                published_at: entry.timestamp.and_then(|t| Timestamp::from_second(t).ok()),
                thumbnails: entry
                    .thumbnails
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| Thumbnail {
                        url: t.url,
                        width: t.width,
                        height: t.height,
                    })
                    .collect(),
                // yt-dlp says so when it knows a video was streamed.
                kind: match entry.live_status.as_deref() {
                    Some("is_live" | "was_live" | "is_upcoming" | "post_live") => {
                        Some(crate::index::VideoKind::Live)
                    }
                    _ => None,
                },
            })
        })
        .collect();
    Ok(RemoteList {
        id: playlist.id,
        title: playlist.title,
        items,
    })
}

fn availability(
    title: &str,
    availability: Option<&str>,
    live_status: Option<&str>,
) -> Availability {
    match (title, availability, live_status) {
        ("[Private video]", _, _) | (_, Some("private"), _) => Availability::Private,
        ("[Deleted video]", _, _) => Availability::Deleted,
        (_, Some("needs_auth" | "subscriber_only" | "premium_only"), _) => Availability::Restricted,
        (_, _, Some("is_upcoming")) => Availability::Upcoming,
        (_, _, Some("is_live")) => Availability::Live,
        (_, Some("unlisted"), _) => Availability::Unlisted,
        _ => Availability::Public,
    }
}

/// The arguments for listing a source.
pub fn list_args(url: &str, cookies_file: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = [
        "--flat-playlist",
        "-J",
        "--no-update",
        "--no-warnings",
        "--extractor-args",
        "youtubetab:approximate_date",
    ]
    .map(String::from)
    .to_vec();
    if let Some(cookies) = cookies_file {
        args.push("--cookies".into());
        args.push(cookies.display().to_string());
    }
    args.push("--".into());
    args.push(url.to_string());
    args
}

/// A channel's id and name: yt-dlp reads the channel page without listing
/// any videos (`--playlist-items 0`).
pub async fn channel(
    ytdlp: &crate::tools::Ytdlp,
    url: &str,
    cookies_file: Option<&Path>,
    shutdown: &crate::shutdown::Shutdown,
    grace: std::time::Duration,
) -> Result<(String, Option<String>), IndexError> {
    let mut args = vec![
        "--flat-playlist".to_string(),
        "-J".into(),
        "--no-update".into(),
        "--no-warnings".into(),
        "--playlist-items".into(),
        "0".into(),
    ];
    if let Some(cookies) = cookies_file {
        args.extend(["--cookies".into(), cookies.display().to_string()]);
    }
    args.extend(["--".into(), url.to_string()]);
    let output = crate::process::output(ytdlp.command().args(&args), grace, shutdown)
        .await
        .map_err(|e| match e {
            crate::process::Error::Stopped => IndexError::Stopped,
            e => IndexError::Ytdlp(format!("cannot run {}: {e}", ytdlp.display())),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .rev()
            .find(|l| l.starts_with("ERROR:"))
            .unwrap_or("no error message")
            .trim()
            .to_string();
        return Err(IndexError::Ytdlp(message));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| IndexError::Parse(e.to_string()))?;
    let id = json["channel_id"]
        .as_str()
        .filter(|id| id.starts_with("UC"))
        .ok_or_else(|| IndexError::Parse(format!("no channel id for {url}")))?;
    Ok((id.to_string(), json["channel"].as_str().map(String::from)))
}

/// List a source with yt-dlp.
pub async fn list(
    ytdlp: &crate::tools::Ytdlp,
    url: &str,
    cookies_file: Option<&Path>,
    shutdown: &crate::shutdown::Shutdown,
    grace: std::time::Duration,
) -> Result<RemoteList, IndexError> {
    let args = list_args(url, cookies_file);
    tracing::trace!("yt-dlp command: {} {}", ytdlp.display(), args.join(" "));
    let output = crate::process::output(ytdlp.command().args(&args), grace, shutdown)
        .await
        .map_err(|e| match e {
            crate::process::Error::Stopped => IndexError::Stopped,
            e => IndexError::Ytdlp(format!("cannot run {}: {e}", ytdlp.display())),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .rev()
            .find(|l| l.starts_with("ERROR:"))
            .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
            .unwrap_or("no error message")
            .trim()
            .to_string();
        return Err(IndexError::Ytdlp(message));
    }
    parse_flat_playlist(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/ytdlp/playlists/{id}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn parses_a_real_playlist() {
        let list = parse_flat_playlist(&fixture("PLXLpuA-bwY4s")).unwrap();
        assert_eq!(list.id, "PLXLpuA-bwY4s");
        assert_eq!(list.title.as_deref(), Some("Rick Astley"));
        assert_eq!(list.items.len(), 1);
        let item = &list.items[0];
        assert_eq!(item.video_id, "dQw4w9WgXcQ");
        assert_eq!(item.position, 1);
        assert_eq!(
            item.title,
            "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"
        );
        assert_eq!(item.duration, Some(214));
        assert_eq!(item.channel.as_deref(), Some("Rick Astley"));
        assert_eq!(
            item.published_at,
            Some(Timestamp::from_second(1257033600).unwrap())
        );
        assert_eq!(item.availability, Availability::Public);
        assert_eq!(
            (item.description.as_deref(), item.tags.as_deref()),
            (None, None)
        );
        assert!(!item.thumbnails.is_empty());
    }

    #[test]
    fn maps_availability() {
        let list = parse_flat_playlist(&fixture("PLmixedmixedmixed")).unwrap();
        let by_title = |t: &str| list.items.iter().find(|i| i.title == t).unwrap();
        assert_eq!(
            by_title("[Private video]").availability,
            Availability::Private
        );
        assert_eq!(
            by_title("[Deleted video]").availability,
            Availability::Deleted
        );
        assert_eq!(
            by_title("Premiere tomorrow").availability,
            Availability::Upcoming
        );
        assert_eq!(by_title("Streaming now").availability, Availability::Live);
        assert_eq!(
            by_title("Members only").availability,
            Availability::Restricted
        );
        assert_eq!(by_title("No date yet").published_at, None);
        let positions: Vec<u32> = list.items.iter().map(|i| i.position).collect();
        assert_eq!(positions, (1..=12).collect::<Vec<_>>());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_flat_playlist("not json"),
            Err(IndexError::Parse(_))
        ));
        // Null entries (yt-dlp emits them for some unavailable items) are skipped.
        let list = parse_flat_playlist(
            r#"{"id":"PLx","title":null,"entries":[null,{"id":"abc","title":"t"}]}"#,
        )
        .unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].position, 2);
    }

    #[test]
    fn builds_arguments() {
        let args = list_args(
            "https://www.youtube.com/playlist?list=PLx",
            Some(Path::new("/c.txt")),
        );
        assert_eq!(
            args.join(" "),
            "--flat-playlist -J --no-update --no-warnings --extractor-args youtubetab:approximate_date \
             --cookies /c.txt -- https://www.youtube.com/playlist?list=PLx"
        );
    }

    #[tokio::test]
    async fn runs_the_fake_ytdlp() {
        let fake = crate::tools::Ytdlp::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bin/yt-dlp"),
        );
        let shutdown = crate::shutdown::Shutdown::default();
        let grace = std::time::Duration::from_secs(5);
        let remote = list(
            &fake,
            "https://www.youtube.com/playlist?list=PLXLpuA-bwY4s",
            None,
            &shutdown,
            grace,
        )
        .await
        .unwrap();
        assert_eq!(remote.items.len(), 1);
        let err = list(
            &fake,
            "https://www.youtube.com/playlist?list=PLnope",
            None,
            &shutdown,
            grace,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "yt-dlp failed: ERROR: [youtube:tab] PLnope: The playlist does not exist."
        );
    }
}
