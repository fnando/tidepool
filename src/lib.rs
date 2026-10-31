//! tidepool: keep local mirrors of YouTube playlists in sync.

pub mod app;
pub mod cli;
pub mod config;
pub mod download;
pub mod filter;
pub mod index;
pub mod init;
pub mod lock;
pub mod paths;
pub mod pipeline;
pub mod problems;
pub mod state;
pub mod ytdlp;
