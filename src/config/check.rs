//! `tidepool config check`: environment checks and the effective config.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use super::diff::mask_secret;
use super::merge::{EffectiveSource, effective_sources};
use super::types::{
    Config, HookConfig, LibraryConfig, ScheduleConfig, ServerConfig, YoutubeConfig, YtdlpConfig,
};
use crate::paths::{Dirs, expand};

/// Something found while checking the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Ok(String),
    Warning(String),
    Error(String),
}

/// Resolve `library.root` (expanding `~`/`$VARS`), defaulting to the data dir.
pub fn media_root(config: &Config, dirs: &Dirs) -> Result<PathBuf, String> {
    match &config.library.root {
        Some(root) => expand(root, &dirs.home),
        None => Ok(dirs.default_media_root()),
    }
}

/// Check what a run needs: yt-dlp (and ffmpeg when embedding needs it), a
/// writable media root, and a private config file when it holds API keys.
pub fn check_environment(config: &Config, dirs: &Dirs, config_file: Option<&Path>) -> Vec<Finding> {
    let mut out = check_tools(config, dirs);
    out.extend(check_storage(config, dirs, config_file));
    out
}

/// yt-dlp (unless managed and not downloaded yet) and ffmpeg/ffprobe.
pub fn check_tools(config: &Config, dirs: &Dirs) -> Vec<Finding> {
    let mut out = Vec::new();

    let ytdlp = &config.ytdlp;
    if ytdlp.managed {
        let managed = dirs.managed_ytdlp(ytdlp.channel);
        out.push(match tool_version(&managed.to_string_lossy(), "--version") {
            Ok(version) => Finding::Ok(format!("yt-dlp {version} (managed, {})", managed.display())),
            Err(_) => Finding::Ok(format!(
                "yt-dlp is managed by tidepool ({} channel) and will be downloaded on first use",
                channel_name(ytdlp)
            )),
        });
    } else {
        let path = &ytdlp.path;
        out.push(match tool_version(path, "--version") {
            Ok(version) => Finding::Ok(format!("yt-dlp {version} ({path})")),
            Err(e) => Finding::Error(format!(
                "yt-dlp: cannot run {path:?} (ytdlp.path): {e}; install yt-dlp or set ytdlp.managed = true"
            )),
        });
    }

    // Needed for merging formats and for embedding.
    for tool in ["ffmpeg", "ffprobe"] {
        out.push(match tool_version(tool, "-version") {
            Ok(version) => Finding::Ok(version),
            Err(e) => Finding::Error(format!(
                "{tool} is required (merging formats, embedding) but cannot be run: {e}; \
                 install ffmpeg (e.g. `brew install ffmpeg` or `apt install ffmpeg`)"
            )),
        });
    }

    out
}

/// A writable media root, and a private config file when it holds secrets.
pub fn check_storage(config: &Config, dirs: &Dirs, config_file: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    match media_root(config, dirs) {
        Ok(root) => out.push(match check_writable(&root) {
            Ok(()) => Finding::Ok(format!("library.root {} is writable", root.display())),
            Err(e) => Finding::Error(format!("library.root {}: {e}", root.display())),
        }),
        Err(e) => out.push(Finding::Error(format!("library.root: {e}"))),
    }

    if let Some(path) = config_file
        && has_secrets(config)
        && let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        out.push(Finding::Warning(format!(
            "{} contains secrets (youtube.api_key or server.token) and is readable by other users; run `chmod 600 {}`",
            path.display(),
            path.display()
        )));
    }
    out
}

fn has_secrets(config: &Config) -> bool {
    config.youtube.api_key.is_some() || !config.server.token.is_empty()
}

fn channel_name(ytdlp: &YtdlpConfig) -> String {
    serde_json::to_value(ytdlp.channel)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn tool_version(program: &str, flag: &str) -> Result<String, String> {
    let output = Command::new(program)
        .arg(flag)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("`{program} {flag}` exited with {}", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or_default();
    // "ffmpeg version 9.0.2 Copyright ..." → "ffmpeg version 9.0.2"
    Ok(first
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" "))
}

/// The root, or its closest existing ancestor (it will be created), must be a
/// writable directory.
fn check_writable(root: &Path) -> Result<(), String> {
    let existing = root
        .ancestors()
        .find(|p| p.exists())
        .ok_or("no existing parent directory")?;
    if !existing.is_dir() {
        return Err(format!("{} is not a directory", existing.display()));
    }
    let probe = existing.join(format!(".tidepool-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| format!("not writable ({e})"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// The merged config `config check` prints: every default filled in.
#[derive(Debug, Serialize)]
pub struct EffectiveConfig {
    pub library: EffectiveLibrary,
    pub ytdlp: YtdlpConfig,
    pub youtube: YoutubeConfig,
    pub schedule: ScheduleConfig,
    pub server: ServerConfig,
    pub hooks: BTreeMap<String, HookConfig>,
    pub sources: BTreeMap<String, EffectiveSource>,
}

#[derive(Debug, Serialize)]
pub struct EffectiveLibrary {
    pub root: String,
    #[serde(flatten)]
    pub rest: LibraryConfig,
}

impl EffectiveConfig {
    pub fn new(config: &Config, dirs: &Dirs, show_secrets: bool) -> Self {
        let mut youtube = config.youtube.clone();
        let mut server = config.server.clone();
        if !show_secrets {
            youtube.api_key = youtube.api_key.as_deref().map(mask_secret);
            server.token = mask_secret(&server.token);
        }
        let root = media_root(config, dirs)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| config.library.root.clone().unwrap_or_default());
        let mut rest = config.library.clone();
        rest.root = None;
        Self {
            library: EffectiveLibrary { root, rest },
            ytdlp: config.ytdlp.clone(),
            youtube,
            schedule: config.schedule.clone(),
            server,
            hooks: config.hooks.clone(),
            sources: effective_sources(config)
                .into_iter()
                .map(|p| (p.key.clone(), p))
                .collect(),
        }
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("effective config serializes to TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs {
            home: "/home/me".into(),
            config_dir: "/c".into(),
            data_dir: "/d/tidepool".into(),
        }
    }

    #[test]
    fn resolves_media_root() {
        let config: Config = toml::from_str("[library]\nroot = '~/Videos'").unwrap();
        assert_eq!(media_root(&config, &dirs()), Ok("/home/me/Videos".into()));
        assert_eq!(
            media_root(&Config::default(), &dirs()),
            Ok("/d/tidepool/media".into())
        );
    }

    #[test]
    fn effective_config_masks_keys_and_fills_defaults() {
        let config: Config = toml::from_str(
            "[youtube]\napi_key = 'AIzaSyD-1234567890x1Qw'\n[server]\ntoken = 'my-long-secret-token'\n\
             [sources.lofi]\nurl = 'https://www.youtube.com/playlist?list=PLx'",
        )
        .unwrap();
        let text = EffectiveConfig::new(&config, &dirs(), false).to_toml();
        assert!(
            text.contains("AIza…x1Qw") && !text.contains("1234567890"),
            "{text}"
        );
        assert!(text.contains("root = \"/d/tidepool/media\""), "{text}");
        assert!(text.contains("[sources.lofi]"), "{text}");
        assert!(
            text.contains("output = \"{source_dir}/{upload_date} - {title} [{id}]\""),
            "{text}"
        );
        let shown = EffectiveConfig::new(&config, &dirs(), true).to_toml();
        assert!(shown.contains("AIzaSyD-1234567890x1Qw"));
    }

    #[test]
    fn missing_ytdlp_is_an_error() {
        let config: Config = toml::from_str(
            "[ytdlp]\nmanaged = false\npath = '/nonexistent/yt-dlp'\n[library]\nroot = '/tmp'",
        )
        .unwrap();
        let findings = check_environment(&config, &dirs(), None);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::Error(m) if m.contains("yt-dlp"))),
            "{findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::Ok(m) if m.contains("writable"))),
            "{findings:?}"
        );
    }

    #[test]
    fn missing_managed_ytdlp_is_fine() {
        let config: Config = toml::from_str("[library]\nroot = '/tmp'").unwrap();
        let findings = check_environment(&config, &dirs(), None);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::Ok(m) if m.contains("downloaded on first use"))),
            "{findings:?}"
        );
        assert!(
            !findings
                .iter()
                .any(|f| matches!(f, Finding::Error(m) if m.contains("yt-dlp")))
        );
    }

    #[test]
    fn warns_about_world_readable_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let config: Config =
            toml::from_str("[server]\ntoken = 'k'\n[library]\nroot = '/tmp'").unwrap();
        let findings = check_environment(&config, &dirs(), Some(&file));
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::Warning(m) if m.contains("chmod 600"))),
            "{findings:?}"
        );
    }
}
