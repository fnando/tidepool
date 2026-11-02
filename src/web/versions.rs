//! The versions the dashboard shows: tidepool's, and the yt-dlp and Deno
//! the config points at. Asking a binary its version takes a moment (yt-dlp
//! starts Python), so answers are remembered until the file changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::config::types::ToolsConfig;
use crate::tools::manager::{Tool, tool_version};

/// How long to trust an answer about a binary whose file can't be seen
/// (a bare name found on `PATH`).
const UNSEEN_FOR: Duration = Duration::from_mins(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Software {
    pub name: &'static str,
    /// `None`: not installed yet, or it doesn't run.
    pub version: Option<String>,
    /// How tidepool gets it, e.g. "managed" or "your own".
    pub how: &'static str,
}

#[derive(Default)]
pub struct Versions {
    known: Mutex<HashMap<PathBuf, Known>>,
}

struct Known {
    modified: Option<SystemTime>,
    asked_at: Instant,
    version: Option<String>,
}

impl Versions {
    /// tidepool, yt-dlp and Deno. Runs binaries; call off the async threads.
    pub fn all(&self, tools: &ToolsConfig, bin_dir: &Path) -> Vec<Software> {
        let tool = |tool: Tool, managed: bool, path: &str| {
            let binary = if managed {
                bin_dir.join(tool.file_name())
            } else {
                PathBuf::from(path)
            };
            Software {
                name: tool.name(),
                version: self.version_of(tool, &binary, managed),
                how: if managed { "managed" } else { "your own" },
            }
        };
        vec![
            Software {
                name: "tidepool",
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
                how: "",
            },
            tool(
                Tool::Ytdlp(tools.ytdlp.channel),
                tools.ytdlp.managed,
                &tools.ytdlp.path,
            ),
            tool(Tool::Deno, tools.deno.managed, &tools.deno.path),
        ]
    }

    fn version_of(&self, tool: Tool, binary: &Path, managed: bool) -> Option<String> {
        let modified = std::fs::metadata(binary).and_then(|m| m.modified()).ok();
        // A managed binary that isn't there hasn't been downloaded yet.
        if managed && modified.is_none() {
            return None;
        }
        let mut known = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(k) = known.get(binary)
            && k.modified == modified
            && (modified.is_some() || k.asked_at.elapsed() < UNSEEN_FOR)
        {
            return k.version.clone();
        }
        let version = tool_version(binary).ok().map(|line| short(tool, &line));
        known.insert(
            binary.to_path_buf(),
            Known {
                modified,
                asked_at: Instant::now(),
                version: version.clone(),
            },
        );
        version
    }
}

/// yt-dlp prints "2026.10.01"; Deno "deno 2.9.8 (stable, release, …)".
fn short(tool: Tool, line: &str) -> String {
    match tool {
        Tool::Deno => line.split_whitespace().nth(1).unwrap_or(line).to_string(),
        Tool::Ytdlp(_) => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::YtdlpChannel;

    #[test]
    fn keeps_just_the_version_number() {
        assert_eq!(
            short(
                Tool::Deno,
                "deno 2.9.8 (stable, release, x86_64-unknown-linux-gnu)"
            ),
            "2.9.8"
        );
        assert_eq!(
            short(Tool::Ytdlp(YtdlpChannel::Stable), "2026.10.01"),
            "2026.10.01"
        );
    }

    #[test]
    fn a_managed_tool_not_downloaded_has_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let versions = Versions::default().all(&ToolsConfig::default(), dir.path());
        let names: Vec<_> = versions
            .iter()
            .map(|v| (v.name, v.version.clone()))
            .collect();
        assert_eq!(
            names,
            [
                ("tidepool", Some(env!("CARGO_PKG_VERSION").to_string())),
                ("yt-dlp", None),
                ("Deno", None),
            ]
        );
    }
}
