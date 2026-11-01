//! Tools tidepool runs: yt-dlp and Deno (the JavaScript runtime yt-dlp needs
//! for YouTube), managed by tidepool (downloaded, verified, kept up to date)
//! or provided by the user.

pub mod manager;
pub mod platform;
pub mod verify;

use std::path::{Path, PathBuf};

/// How to run yt-dlp: the binary, and the Deno it should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ytdlp {
    pub path: PathBuf,
    /// Passed as `--js-runtimes deno:<path>`; `None` leaves the choice to
    /// yt-dlp (a `deno` on `PATH`).
    pub deno: Option<PathBuf>,
}

impl Ytdlp {
    /// Just a binary, no explicit runtime.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            deno: None,
        }
    }

    /// A command for yt-dlp with the runtime option in place.
    pub fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.path);
        command.args(self.runtime_args());
        command
    }

    /// `--js-runtimes deno:<path>`, when a runtime is set.
    pub fn runtime_args(&self) -> Vec<String> {
        match &self.deno {
            Some(deno) => vec!["--js-runtimes".into(), format!("deno:{}", deno.display())],
            None => Vec::new(),
        }
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.path.display()
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }
}
