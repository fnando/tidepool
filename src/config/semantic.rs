//! Checks the JSON Schema can't express: cross-references, uniqueness and
//! ordering between values. Reported with the same path + line format.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use super::load::Diagnostic;
use super::merge::{DEFAULT_CATEGORY, category_of, category_settings, effective_source};
use super::types::Config;

/// Run every semantic check.
pub fn check(config: &Config) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    check_hook_references(config, &mut out);
    check_categories(config, &mut out);
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
    for (name, category) in &config.category {
        check(format!("/category/{name}"), &category.hooks);
    }
    for (key, source) in &config.sources {
        check(format!("/sources/{key}"), &source.hooks);
    }
}

fn check_categories(config: &Config, out: &mut Vec<Diagnostic>) {
    for (key, source) in &config.sources {
        let category = category_of(source);
        if category != DEFAULT_CATEGORY && !config.category.contains_key(category) {
            let mut known: Vec<&str> = vec![DEFAULT_CATEGORY];
            known.extend(
                config
                    .category
                    .keys()
                    .map(String::as_str)
                    .filter(|c| *c != DEFAULT_CATEGORY),
            );
            out.push(Diagnostic::error(
                format!("/sources/{key}/category"),
                format!(
                    "unknown category {category:?} (categories: {})",
                    known.join(", ")
                ),
            ));
        }
    }
}

fn check_unique_ids_and_dirs(config: &Config, out: &mut Vec<Diagnostic>) {
    let mut ids: BTreeMap<String, &str> = BTreeMap::new();
    // Folders must be unique within the same root.
    let mut dirs: BTreeMap<(Option<String>, String), &str> = BTreeMap::new();
    for (key, source) in &config.sources {
        let effective = effective_source(config, key, source);
        let id_pointer = format!("/sources/{key}/url");
        if let Some(other) = ids.insert(effective.id.clone(), key) {
            out.push(Diagnostic::error(
                id_pointer,
                format!(
                    "playlist {} is already used by sources.{other}",
                    effective.id
                ),
            ));
        }

        let dir_pointer = if source.dir.is_some() {
            format!("/sources/{key}/dir")
        } else {
            format!("/sources/{key}")
        };
        match normalize_dir(&effective.dir) {
            None => out.push(Diagnostic::error(
                dir_pointer,
                format!(
                    "dir {:?} must be a relative path that stays inside the root",
                    effective.dir
                ),
            )),
            Some(normalized) => {
                let root = effective
                    .root
                    .clone()
                    .map(|r| r.trim_end_matches('/').to_string());
                if let Some(other) = dirs.insert((root, normalized), key) {
                    out.push(Diagnostic::error(
                        dir_pointer,
                        format!("dir {:?} is also used by sources.{other}", effective.dir),
                    ));
                }
            }
        }
    }
}

/// Normalize a source dir; `None` if it is absolute or escapes the root.
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
    let broken = |min: Option<std::time::Duration>, max: Option<std::time::Duration>| matches!((min, max), (Some(min), Some(max)) if min > max);
    // Each layer is reported where it sets a duration, so one mistake isn't
    // repeated for every source that inherits it.
    for (name, category) in &config.category {
        if category.min_duration.is_none() && category.max_duration.is_none() {
            continue;
        }
        let settings = category_settings(config, name);
        if broken(settings.min_duration.0, settings.max_duration.0) {
            let field = if category.min_duration.is_some() {
                "min_duration"
            } else {
                "max_duration"
            };
            out.push(Diagnostic::error(
                format!("/category/{name}/{field}"),
                message,
            ));
        }
    }
    for (key, source) in &config.sources {
        if source.min_duration.is_none() && source.max_duration.is_none() {
            continue;
        }
        let effective = effective_source(config, key, source);
        if broken(effective.min_duration.0, effective.max_duration.0) {
            let field = if source.min_duration.is_some() {
                "min_duration"
            } else {
                "max_duration"
            };
            out.push(Diagnostic::error(
                format!("/sources/{key}/{field}"),
                message,
            ));
        }
    }
}

fn check_youtube(config: &Config, out: &mut Vec<Diagnostic>) {
    let warning = "has no effect without api_key: yt-dlp is always the indexer then";
    for (name, category) in &config.category {
        if category
            .youtube
            .as_ref()
            .and_then(|y| y.fallback_to_ytdlp)
            .is_none()
        {
            continue;
        }
        let youtube = category_settings(config, name).youtube;
        if youtube.api_key.is_none() && !youtube.fallback_to_ytdlp {
            out.push(Diagnostic::warning(
                format!("/category/{name}/youtube/fallback_to_ytdlp"),
                warning,
            ));
        }
    }
    for (key, source) in &config.sources {
        if source
            .youtube
            .as_ref()
            .and_then(|y| y.fallback_to_ytdlp)
            .is_none()
        {
            continue;
        }
        let youtube = effective_source(config, key, source).youtube;
        if youtube.api_key.is_none() && !youtube.fallback_to_ytdlp {
            out.push(Diagnostic::warning(
                format!("/sources/{key}/youtube/fallback_to_ytdlp"),
                warning,
            ));
        }
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
