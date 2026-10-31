//! XDG directories and path expansion.
//!
//! Config lives in `$XDG_CONFIG_HOME/tidepool` (`~/.config/tidepool`), state in
//! `$XDG_DATA_HOME/tidepool` (`~/.local/share/tidepool`), on macOS too.

use std::path::{Path, PathBuf};

use etcetera::BaseStrategy;
use etcetera::base_strategy::Xdg;

/// tidepool's base directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

/// Per-library state paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPaths {
    pub dir: PathBuf,
    pub state_db: PathBuf,
    pub lock: PathBuf,
    pub scheduler_lock: PathBuf,
    pub hook_logs: PathBuf,
}

impl Dirs {
    /// Resolve from the environment (`$HOME`, `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`).
    pub fn from_env() -> Result<Self, String> {
        let xdg = Xdg::new().map_err(|e| format!("cannot find the home directory: {e}"))?;
        Ok(Self {
            home: xdg.home_dir().to_path_buf(),
            config_dir: xdg.config_dir().join("tidepool"),
            data_dir: xdg.data_dir().join("tidepool"),
        })
    }

    /// The conventional config file location.
    pub fn default_config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Media root when `library.root` is unset.
    pub fn default_media_root(&self) -> PathBuf {
        self.data_dir.join("media")
    }

    /// Managed binaries, shared by all libraries.
    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    /// The managed yt-dlp binary for a release channel.
    pub fn managed_ytdlp(&self, channel: crate::config::YtdlpChannel) -> PathBuf {
        self.bin_dir().join(format!("yt-dlp-{}", channel.as_str()))
    }

    pub fn library(&self, name: &str) -> LibraryPaths {
        let dir = self.data_dir.join("libraries").join(name);
        LibraryPaths {
            state_db: dir.join("state.db"),
            lock: dir.join("lock"),
            scheduler_lock: dir.join("scheduler.lock"),
            hook_logs: dir.join("logs").join("hooks"),
            dir,
        }
    }
}

/// Expand a leading `~` and `$VAR` / `${VAR}` references.
pub fn expand(input: &str, home: &Path) -> Result<PathBuf, String> {
    expand_with(input, home, |name| std::env::var(name).ok())
}

/// [`expand`] with an injectable variable lookup.
pub fn expand_with(
    input: &str,
    home: &Path,
    var: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    let mut out = String::new();
    let rest = if input == "~" {
        out.push_str(&home.to_string_lossy());
        ""
    } else if let Some(rest) = input.strip_prefix("~/") {
        out.push_str(&home.to_string_lossy());
        out.push('/');
        rest
    } else {
        input
    };

    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.next() != Some('}') {
            return Err(format!("{input:?}: unclosed ${{"));
        }
        if name.is_empty() {
            out.push('$');
            continue;
        }
        let value = var(&name)
            .ok_or_else(|| format!("{input:?}: environment variable ${name} is not set"))?;
        out.push_str(&value);
    }
    Ok(PathBuf::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(s: &str) -> Result<PathBuf, String> {
        expand_with(s, Path::new("/home/me"), |name| {
            (name == "VIDEOS").then(|| "/mnt/v".into())
        })
    }

    #[test]
    fn expands_home_and_variables() {
        assert_eq!(exp("~"), Ok("/home/me".into()));
        assert_eq!(exp("~/Videos"), Ok("/home/me/Videos".into()));
        assert_eq!(exp("$VIDEOS/yt"), Ok("/mnt/v/yt".into()));
        assert_eq!(exp("${VIDEOS}yt"), Ok("/mnt/vyt".into()));
        assert_eq!(exp("a~b$"), Ok("a~b$".into()));
        assert!(exp("$MISSING/x").is_err());
        assert!(exp("${VIDEOS").is_err());
    }

    #[test]
    fn library_paths() {
        let dirs = Dirs {
            home: "/h".into(),
            config_dir: "/c/tidepool".into(),
            data_dir: "/d/tidepool".into(),
        };
        let lib = dirs.library("default");
        assert_eq!(
            lib.state_db,
            PathBuf::from("/d/tidepool/libraries/default/state.db")
        );
        assert_eq!(
            lib.lock,
            PathBuf::from("/d/tidepool/libraries/default/lock")
        );
        assert_eq!(
            dirs.default_media_root(),
            PathBuf::from("/d/tidepool/media")
        );
    }
}
