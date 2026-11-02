//! Is a video a Short? YouTube serves `youtube.com/shorts/<id>` for Shorts
//! (200) and redirects anything else to `/watch?v=<id>` (303). Neither the
//! API nor yt-dlp's playlist listing says so.

use std::time::Duration;

use super::{RemoteItem, SHORTS_MAX_SECONDS, VideoKind};
use crate::shutdown::Shutdown;

const YOUTUBE: &str = "https://www.youtube.com";

pub struct ShortsCheck {
    client: reqwest::Client,
    base: String,
}

impl Default for ShortsCheck {
    fn default() -> Self {
        #[allow(unused_mut)]
        let mut base = YOUTUBE.to_string();
        // Debug builds (tests) can point at a fake YouTube.
        #[cfg(debug_assertions)]
        if let Ok(fake) = std::env::var("TIDEPOOL_TEST_SHORTS_BASE") {
            base = fake;
        }
        Self::with(&base)
    }
}

impl ShortsCheck {
    /// A check against `base` instead of YouTube.
    ///
    /// # Panics
    ///
    /// If the HTTP client can't be built: its TLS backend fails to
    /// initialize, or the system's DNS configuration can't be loaded.
    #[must_use]
    pub fn with(base: &str) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("tidepool/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("HTTP client builds");
        Self {
            client,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// `Some(true)` for a Short, `Some(false)` for a regular video, `None`
    /// when YouTube didn't say (an error, or a redirect elsewhere, e.g. a
    /// cookie consent page).
    pub async fn is_short(&self, video_id: &str) -> Option<bool> {
        let url = format!("{}/shorts/{video_id}", self.base);
        let response = self.client.head(&url).send().await.ok()?;
        let status = response.status();
        if status.is_success() {
            return Some(true);
        }
        if status.is_redirection() {
            let location = response.headers().get("location")?.to_str().ok()?;
            return location.contains("/watch").then_some(false);
        }
        None
    }

    /// Fill in `kind` where it's unknown: a Short, or (when `live_known`,
    /// i.e. the item would say if it had been streamed) a regular video.
    /// Videos longer than a Short can be skip the check.
    pub async fn classify(&self, items: &mut [RemoteItem], live_known: bool, shutdown: &Shutdown) {
        // Streamed (or a Premiere): yt-dlp decides before download.
        for item in items.iter_mut().filter(|i| i.kind.is_none() && !i.streamed) {
            if shutdown.is_stopping() {
                return;
            }
            if item.duration.is_some_and(|d| d > SHORTS_MAX_SECONDS) {
                continue;
            }
            item.kind = match self.is_short(&item.video_id).await {
                Some(true) => Some(VideoKind::Short),
                Some(false) if live_known => Some(VideoKind::Video),
                _ => None,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn youtube() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/shorts/DdmxNoj2cGQ"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/shorts/HF1aUFFI-N0"))
            .respond_with(
                ResponseTemplate::new(303)
                    .insert_header("location", "https://www.youtube.com/watch?v=HF1aUFFI-N0"),
            )
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/shorts/consent0001"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "https://consent.youtube.com/m"),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn tells_shorts_from_videos() {
        let server = youtube().await;
        let check = ShortsCheck::with(&server.uri());
        assert_eq!(check.is_short("DdmxNoj2cGQ").await, Some(true));
        assert_eq!(check.is_short("HF1aUFFI-N0").await, Some(false));
        assert_eq!(check.is_short("consent0001").await, None, "not an answer");
        assert_eq!(check.is_short("missing0001").await, None);
    }

    fn item(id: &str, duration: Option<u64>) -> RemoteItem {
        RemoteItem {
            video_id: id.into(),
            position: 1,
            title: id.into(),
            channel: None,
            channel_id: None,
            description: None,
            tags: None,
            duration,
            published_at: None,
            availability: crate::index::Availability::Public,
            thumbnails: vec![],
            kind: None,
            streamed: false,
        }
    }

    #[tokio::test]
    async fn classifies_only_what_could_be_a_short() {
        let server = youtube().await;
        let check = ShortsCheck::with(&server.uri());
        let mut items = vec![
            item("DdmxNoj2cGQ", Some(37)),
            item("HF1aUFFI-N0", Some(160)),
            item("long0000001", Some(1261)),
        ];
        check.classify(&mut items, true, &Shutdown::default()).await;
        let kinds: Vec<_> = items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            [Some(VideoKind::Short), Some(VideoKind::Video), None]
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "no check for long videos"
        );

        // Without knowing about live streams, "not a Short" isn't enough.
        let mut items = vec![item("HF1aUFFI-N0", Some(160))];
        check
            .classify(&mut items, false, &Shutdown::default())
            .await;
        assert_eq!(items[0].kind, None);
    }
}
