//! Indexing: what a source contains right now. Every indexer produces the
//! same [`RemoteList`], so everything downstream is source-agnostic.

pub mod api;
#[cfg(test)]
pub mod youtube_fake;
pub mod ytdlp;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Whether a video can be downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Public,
    Unlisted,
    Private,
    Deleted,
    /// Members-only, premium, sign-in required.
    Restricted,
    /// A premiere or scheduled stream; downloadable once it's over.
    Upcoming,
    /// Streaming right now; downloadable once it's over.
    Live,
}

impl Availability {
    pub fn as_str(self) -> &'static str {
        match self {
            Availability::Public => "public",
            Availability::Unlisted => "unlisted",
            Availability::Private => "private",
            Availability::Deleted => "deleted",
            Availability::Restricted => "restricted",
            Availability::Upcoming => "upcoming",
            Availability::Live => "live",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "unlisted" => Availability::Unlisted,
            "private" => Availability::Private,
            "deleted" => Availability::Deleted,
            "restricted" => Availability::Restricted,
            "upcoming" => Availability::Upcoming,
            "live" => Availability::Live,
            _ => Availability::Public,
        }
    }

    pub fn downloadable(self) -> bool {
        matches!(self, Availability::Public | Availability::Unlisted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// One video in a source, as the indexer saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteItem {
    pub video_id: String,
    /// 1-based position in the source.
    pub position: u32,
    pub title: String,
    pub channel: Option<String>,
    pub channel_id: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    /// Length in seconds.
    pub duration: Option<u64>,
    /// Publish time; approximate on the yt-dlp path.
    pub published_at: Option<Timestamp>,
    pub availability: Availability,
    pub thumbnails: Vec<Thumbnail>,
}

/// A source's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteList {
    /// YouTube id (playlist id).
    pub id: String,
    pub title: Option<String>,
    pub items: Vec<RemoteItem>,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("yt-dlp failed: {0}")]
    Ytdlp(String),
    #[error("stopped")]
    Stopped,
    #[error("unexpected yt-dlp output: {0}")]
    Parse(String),
}
