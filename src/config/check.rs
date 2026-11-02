//! `tidepool config check`: environment checks and the effective config.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use super::diff::mask_secret;
use super::merge::{DEFAULT_CATEGORY, EffectiveSource, category_settings, effective_sources};
use super::placeholders::{Setting, render_path};
use super::types::{
    Config, GeneralConfig, HookConfig, ScheduleConfig, ServerConfig, ToolsConfig, YtdlpConfig,
};
use crate::paths::{Dirs, expand};

/// Something found while checking the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Ok(String),
    Warning(String),
    Error(String),
}

/// A source's media folder (`media_dir`, expanding `~`/`$VARS`), defaulting
/// to ~/.local/share/tidepool/media.
///
/// # Errors
///
/// When `media_dir` has an unclosed `${` or names an environment variable
/// that isn't set.
pub fn media_dir(source: &EffectiveSource, dirs: &Dirs) -> Result<PathBuf, String> {
    match &source.media_dir {
        Some(dir) => expand(dir, &dirs.home),
        None => Ok(dirs.default_media_dir()),
    }
}

/// A source's work area (`tmp_dir`, with `{media_dir}` and `{data_dir}` filled in).
///
/// # Errors
///
/// When the media folder can't be resolved (see [`media_dir`]), or `tmp_dir`
/// has an unknown placeholder or doesn't render to a full path without `..`.
pub fn tmp_dir(source: &EffectiveSource, dirs: &Dirs) -> Result<PathBuf, String> {
    work_dir(&source.tmp_dir, Setting::TmpDir, source, dirs)
}

/// A source's trash (`trash_dir`, with `{media_dir}` and `{data_dir}` filled in).
///
/// # Errors
///
/// When the media folder can't be resolved (see [`media_dir`]), or
/// `trash_dir` has an unknown placeholder or doesn't render to a full path
/// without `..`.
pub fn trash_dir(source: &EffectiveSource, dirs: &Dirs) -> Result<PathBuf, String> {
    work_dir(&source.trash_dir, Setting::TrashDir, source, dirs)
}

fn work_dir(
    template: &str,
    setting: Setting,
    source: &EffectiveSource,
    dirs: &Dirs,
) -> Result<PathBuf, String> {
    let media = media_dir(source, dirs)?;
    render_path(template, setting, &dirs.home, |name| match name {
        "media_dir" => Some(media.display().to_string()),
        "data_dir" => Some(dirs.data_dir.display().to_string()),
        _ => None,
    })
    .map_err(|e| format!("{setting} {e}"))
}

/// Every media folder the config uses: each category's and each source's,
/// without repeats.
pub fn media_dirs(config: &Config, dirs: &Dirs) -> Vec<Result<PathBuf, String>> {
    let mut categories: Vec<&str> = vec![DEFAULT_CATEGORY];
    categories.extend(
        config
            .category
            .keys()
            .map(String::as_str)
            .filter(|c| *c != DEFAULT_CATEGORY),
    );
    let mut roots: Vec<Result<PathBuf, String>> = Vec::new();
    let settings = categories
        .iter()
        .map(|c| category_settings(config, c))
        .chain(effective_sources(config));
    for dir in settings.map(|s| media_dir(&s, dirs)) {
        if !roots.contains(&dir) {
            roots.push(dir);
        }
    }
    roots
}

/// Check what a run needs: yt-dlp (and ffmpeg when embedding needs it), a
/// writable media root, and a private config file when it holds API keys.
#[must_use]
pub fn check_environment(config: &Config, dirs: &Dirs, config_file: Option<&Path>) -> Vec<Finding> {
    let mut out = check_tools(config, dirs);
    out.extend(check_storage(config, dirs, config_file));
    out
}

/// yt-dlp and Deno (unless managed and not downloaded yet) and ffmpeg/ffprobe.
#[must_use]
pub fn check_tools(config: &Config, dirs: &Dirs) -> Vec<Finding> {
    let mut out = Vec::new();

    let ytdlp = &config.tools.ytdlp;
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
                "yt-dlp: cannot run {path:?} (tools.yt-dlp.path): {e}; install yt-dlp or set managed = true in [tools.yt-dlp]"
            )),
        });
    }

    // yt-dlp needs a JavaScript runtime for YouTube.
    let deno = &config.tools.deno;
    let deno_version = |path: &str| {
        // "deno 2.9.8 (stable, release, aarch64-apple-darwin)" → "deno 2.9.8"
        tool_version(path, "--version")
            .map(|v| v.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
    };
    if deno.managed {
        let managed = dirs.managed_deno();
        out.push(match deno_version(&managed.to_string_lossy()) {
            Ok(version) => Finding::Ok(format!("{version} (managed, {})", managed.display())),
            Err(_) => Finding::Ok(
                "Deno (the JavaScript runtime yt-dlp needs for YouTube) is managed by tidepool and will be downloaded on first use".into(),
            ),
        });
    } else {
        let path = &deno.path;
        out.push(match deno_version(path) {
            Ok(version) => Finding::Ok(format!("{version} ({path})")),
            Err(e) => Finding::Warning(format!(
                "Deno: cannot run {path:?} (tools.deno.path): {e}; yt-dlp needs a JavaScript runtime \
                 for YouTube: install Deno, set managed = true in [tools.deno], or point yt-dlp at \
                 another runtime with ytdlp_args = [\"--js-runtimes\", \"node\"]"
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
#[must_use]
pub fn check_storage(config: &Config, dirs: &Dirs, config_file: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    for root in media_dirs(config, dirs) {
        out.push(match root {
            Ok(root) => match check_writable(&root) {
                Ok(()) => Finding::Ok(format!("media folder {} is writable", root.display())),
                Err(e) => Finding::Error(format!("media folder {}: {e}", root.display())),
            },
            Err(e) => Finding::Error(format!("media_dir: {e}")),
        });
    }
    let mut work_areas: Vec<Result<PathBuf, String>> = Vec::new();
    for dir in effective_sources(config).iter().map(|s| tmp_dir(s, dirs)) {
        if !work_areas.contains(&dir) {
            work_areas.push(dir);
        }
    }
    for dir in work_areas {
        out.push(match dir {
            Ok(dir) => match check_writable(&dir) {
                Ok(()) => Finding::Ok(format!("work area {} is writable", dir.display())),
                Err(e) => Finding::Error(format!("work area {}: {e}", dir.display())),
            },
            Err(e) => Finding::Error(e),
        });
    }

    if let Some(path) = config_file
        && has_secrets(config)
        && let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        out.push(Finding::Warning(format!(
            "{} contains secrets (an api_key or server.token) and is readable by other users; run `chmod 600 {}`",
            path.display(),
            path.display()
        )));
    }
    out
}

fn has_secrets(config: &Config) -> bool {
    let key =
        |y: &Option<super::types::YoutubeOptions>| y.as_ref().is_some_and(|y| y.api_key.is_some());
    config.category.values().any(|c| key(&c.youtube))
        || config.sources.values().any(|s| key(&s.youtube))
        || !config.server.token.is_empty()
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

/// The merged config `config check` prints: every default filled in. Each
/// source shows its resolved settings, including what its categories set.
#[derive(Debug, Serialize)]
pub struct EffectiveConfig {
    pub general: GeneralConfig,
    pub tools: ToolsConfig,
    pub schedule: ScheduleConfig,
    pub server: ServerConfig,
    pub hooks: BTreeMap<String, HookConfig>,
    pub sources: BTreeMap<String, EffectiveSource>,
}

impl EffectiveConfig {
    #[must_use]
    pub fn new(config: &Config, dirs: &Dirs, show_secrets: bool) -> Self {
        let mut server = config.server.clone();
        if !show_secrets {
            server.token = mask_secret(&server.token);
        }
        let sources = effective_sources(config)
            .into_iter()
            .map(|mut source| {
                if !show_secrets {
                    source.youtube.api_key = source.youtube.api_key.as_deref().map(mask_secret);
                }
                if let Ok(dir) = media_dir(&source, dirs) {
                    source.media_dir = Some(dir.display().to_string());
                }
                (source.key.clone(), source)
            })
            .collect();
        Self {
            general: config.general.clone(),
            tools: config.tools.clone(),
            schedule: config.schedule.clone(),
            server,
            hooks: config.hooks.clone(),
            sources,
        }
    }

    /// The effective config as pretty TOML.
    ///
    /// # Panics
    ///
    /// If the config can't be represented as TOML, which would be a bug in
    /// its types rather than anything in the user's config.
    #[must_use]
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
    fn resolves_media_dirs() {
        let config: Config = toml::from_str(
            "[category.default]\nmedia_dir = '~/Videos'\n[category.music]\nmedia_dir = '/nas/music'\ntmp_dir = '{media_dir}/.work'\n[category.misc]\n\
             [sources.a]\nurls = ['https://www.youtube.com/playlist?list=PLa']\ncategory = 'music'",
        )
        .unwrap();
        let source = effective_sources(&config).remove(0);
        assert_eq!(media_dir(&source, &dirs()), Ok("/nas/music".into()));
        assert_eq!(tmp_dir(&source, &dirs()), Ok("/nas/music/.work".into()));
        assert_eq!(
            media_dirs(&config, &dirs()),
            vec![Ok("/home/me/Videos".into()), Ok("/nas/music".into())],
            "default, then each category's, without repeats"
        );
        assert_eq!(
            media_dirs(&Config::default(), &dirs()),
            vec![Ok("/d/tidepool/media".into())]
        );
        let config: Config =
            toml::from_str("[sources.a]\nurls = ['https://www.youtube.com/playlist?list=PLa']")
                .unwrap();
        let source = effective_sources(&config).remove(0);
        assert_eq!(tmp_dir(&source, &dirs()), Ok("/d/tidepool/tmp".into()));
    }

    #[test]
    fn effective_config_masks_keys_and_fills_defaults() {
        let config: Config = toml::from_str(
            "[category.default.youtube]\napi_key = 'AIzaSyD-1234567890x1Qw'\n[server]\ntoken = 'my-long-secret-token'\n\
             [sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLx']",
        )
        .unwrap();
        let text = EffectiveConfig::new(&config, &dirs(), false).to_toml();
        assert!(
            text.contains("AIza…x1Qw") && !text.contains("1234567890"),
            "{text}"
        );
        assert!(text.contains("media_dir = \"/d/tidepool/media\""), "{text}");
        assert!(text.contains("[sources.lofi]"), "{text}");
        assert!(
            text.contains("output_dir = \"{media_dir}/{source}/{upload_date} - {title} [{id}]\"")
                && text.contains("cover_file = \"{output_dir}/{normalized_title}-poster.jpg\""),
            "{text}"
        );
        let shown = EffectiveConfig::new(&config, &dirs(), true).to_toml();
        assert!(shown.contains("AIzaSyD-1234567890x1Qw"));
    }

    #[test]
    fn missing_ytdlp_is_an_error() {
        let config: Config = toml::from_str(
            "[tools.yt-dlp]\nmanaged = false\npath = '/nonexistent/yt-dlp'\n[category.default]\nmedia_dir = '/tmp'",
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
        let config: Config = toml::from_str("[category.default]\nmedia_dir = '/tmp'").unwrap();
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
            toml::from_str("[server]\ntoken = 'k'\n[category.default]\nmedia_dir = '/tmp'")
                .unwrap();
        let findings = check_environment(&config, &dirs(), Some(&file));
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::Warning(m) if m.contains("chmod 600"))),
            "{findings:?}"
        );
    }
}
