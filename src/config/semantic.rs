//! Checks the JSON Schema can't express: cross-references, uniqueness and
//! ordering between values. Reported with the same path + line format.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use super::load::Diagnostic;
use super::merge::effective_playlist;
use super::types::Config;

/// Run every semantic check.
pub fn check(config: &Config) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    check_hook_references(config, &mut out);
    check_unique_ids_and_dirs(config, &mut out);
    check_durations(config, &mut out);
    check_youtube(config, &mut out);
    check_server(config, &mut out);
    out
}

fn check_hook_references(config: &Config, out: &mut Vec<Diagnostic>) {
    let known = || {
        if config.hooks.is_empty() {
            "no hooks are defined".to_string()
        } else {
            let names: Vec<_> = config.hooks.keys().map(String::as_str).collect();
            format!("defined hooks: {}", names.join(", "))
        }
    };
    let mut check = |base: String, hooks: &Option<Vec<String>>| {
        for (i, name) in hooks.iter().flatten().enumerate() {
            if !config.hooks.contains_key(name) {
                out.push(Diagnostic::error(
                    format!("{base}/hooks/{i}"),
                    format!("unknown hook {name:?} ({})", known()),
                ));
            }
        }
    };
    check("/defaults".into(), &config.defaults.hooks);
    for (key, playlist) in &config.playlists {
        check(format!("/playlists/{key}"), &playlist.hooks);
    }
}

fn check_unique_ids_and_dirs(config: &Config, out: &mut Vec<Diagnostic>) {
    let mut ids: BTreeMap<String, &str> = BTreeMap::new();
    let mut dirs: BTreeMap<String, &str> = BTreeMap::new();
    for (key, playlist) in &config.playlists {
        let effective = effective_playlist(key, playlist, &config.defaults);
        let id_pointer = format!("/playlists/{key}/source");
        if let Some(other) = ids.insert(effective.id.clone(), key) {
            out.push(Diagnostic::error(
                id_pointer,
                format!(
                    "playlist {} is already configured as playlists.{other}",
                    effective.id
                ),
            ));
        }

        let dir_pointer = if playlist.dir.is_some() {
            format!("/playlists/{key}/dir")
        } else {
            format!("/playlists/{key}")
        };
        match normalize_dir(&effective.dir) {
            None => out.push(Diagnostic::error(
                dir_pointer,
                format!(
                    "dir {:?} must be a relative path that stays inside library.root",
                    effective.dir
                ),
            )),
            Some(normalized) => {
                if let Some(other) = dirs.insert(normalized, key) {
                    out.push(Diagnostic::error(
                        dir_pointer,
                        format!("dir {:?} is also used by playlists.{other}", effective.dir),
                    ));
                }
            }
        }
    }
}

/// Normalize a playlist dir; `None` if it is absolute or escapes the root.
fn normalize_dir(dir: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(dir).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn check_durations(config: &Config, out: &mut Vec<Diagnostic>) {
    let message = "min_duration must not be greater than max_duration";
    let d = &config.defaults;
    if let (Some(min), Some(max)) = (
        d.min_duration.and_then(|v| v.0),
        d.max_duration.and_then(|v| v.0),
    ) && min > max
    {
        out.push(Diagnostic::error("/defaults/min_duration", message));
    }
    for (key, playlist) in &config.playlists {
        if playlist.min_duration.is_none() && playlist.max_duration.is_none() {
            continue; // already reported for [defaults]
        }
        let effective = effective_playlist(key, playlist, d);
        if let (Some(min), Some(max)) = (effective.min_duration.0, effective.max_duration.0)
            && min > max
        {
            let field = if playlist.min_duration.is_some() {
                "min_duration"
            } else {
                "max_duration"
            };
            out.push(Diagnostic::error(
                format!("/playlists/{key}/{field}"),
                message,
            ));
        }
    }
}

fn check_youtube(config: &Config, out: &mut Vec<Diagnostic>) {
    if config.youtube.api_key.is_none() && !config.youtube.fallback_to_ytdlp {
        out.push(Diagnostic::warning(
            "/youtube/fallback_to_ytdlp",
            "has no effect without api_key: yt-dlp is always the indexer then",
        ));
    }
}

fn check_server(config: &Config, out: &mut Vec<Diagnostic>) {
    let server = &config.server;
    let Ok(addr) = server.listen.parse::<std::net::SocketAddr>() else {
        return;
    };
    if server.enabled && !addr.ip().is_loopback() && server.token.is_empty() {
        out.push(Diagnostic::error(
            "/server/listen",
            format!(
                "{} is reachable from other machines, so server.token is required (or listen on 127.0.0.1)",
                server.listen
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dirs() {
        assert_eq!(normalize_dir("lofi"), Some("lofi".into()));
        assert_eq!(normalize_dir("./a/b/../c"), Some("a/c".into()));
        assert_eq!(normalize_dir("../x"), None);
        assert_eq!(normalize_dir("/abs"), None);
        assert_eq!(normalize_dir("."), None);
    }
}
