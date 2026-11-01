//! tidepool: keep local mirrors of YouTube playlists in sync.

pub mod app;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod download;
pub mod filter;
pub mod hooks;
pub mod index;
pub mod init;
pub mod lock;
pub mod paths;
pub mod pipeline;
pub mod problems;
pub mod process;
pub mod shutdown;
pub mod state;
pub mod tools;
