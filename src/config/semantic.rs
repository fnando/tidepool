//! Checks the JSON Schema can't express: cross-references, uniqueness and
//! ordering between values. Reported with the same path + line format.

use std::collections::BTreeMap;

use super::load::Diagnostic;
use super::merge::{DEFAULT_CATEGORY, category_of, category_settings, effective_source};
use super::types::Config;

/// Run every semantic check.
pub fn check(config: &Config) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    check_hook_references(config, &mut out);
    check_categories(config, &mut out);
    check_kind_rules(config, &mut out);
    check_unique_ids(config, &mut out);
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

/// `kind` is matched by value (`video`, `short`, `live`), never by regex.
fn check_kind_rules(config: &Config, out: &mut Vec<Diagnostic>) {
    use super::types::{KINDS, Options, Rule, RuleField};
    let mut layers: Vec<(String, Options)> = config
        .category
        .iter()
        .map(|(name, c)| (format!("/category/{name}"), c.options()))
        .collect();
    layers.extend(
        config
            .sources
            .iter()
            .map(|(key, s)| (format!("/sources/{key}"), s.options())),
    );
    for (base, options) in layers {
        if let Some(fields) = &options.match_fields
            && let Some(i) = fields.iter().position(|f| *f == RuleField::Kind)
        {
            out.push(Diagnostic::error(
                format!("{base}/match_fields/{i}"),
                "kind can't be matched by a regex; use { field = \"kind\", value = \"short\" } in include/exclude",
            ));
        }
        for (list, rules) in [("include", &options.include), ("exclude", &options.exclude)] {
            for (i, rule) in rules.iter().flatten().enumerate() {
                match rule {
                    Rule::Field(f) if f.field == RuleField::Kind => out.push(Diagnostic::error(
                        format!("{base}/{list}/{i}"),
                        "kind is matched by value, not pattern: use { field = \"kind\", value = \"short\" } (video, short or live)",
                    )),
                    Rule::Value(v) if v.field == RuleField::Kind && !KINDS.contains(&v.value.as_str()) => {
                        out.push(Diagnostic::error(
                            format!("{base}/{list}/{i}/value"),
                            format!("{:?} isn't a kind: use \"video\", \"short\" or \"live\"", v.value),
                        ))
                    }
                    _ => {}
                }
            }
        }
    }
}

fn check_unique_ids(config: &Config, out: &mut Vec<Diagnostic>) {
    let mut ids: BTreeMap<String, &str> = BTreeMap::new();
    for (key, source) in &config.sources {
        let effective = effective_source(config, key, source);
        for (i, list) in effective.lists().iter().enumerate() {
            let used = if list.channel { "channel" } else { "playlist" };
            match ids.insert(list.id.clone(), key) {
                Some(other) if other == key => out.push(Diagnostic::error(
                    format!("/sources/{key}/urls/{i}"),
                    format!("{used} {} is already in this source's urls", list.id),
                )),
                Some(other) => out.push(Diagnostic::error(
                    format!("/sources/{key}/urls/{i}"),
                    format!("{used} {} is already used by sources.{other}", list.id),
                )),
                None => {}
            }
        }
    }
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
