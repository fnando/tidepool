//! The config JSON Schema: generated from [`super::types`] with `schemars`,
//! validated with `jsonschema` plus tidepool's custom formats.

use std::sync::OnceLock;

use jsonschema::{Draft, Validator};
use schemars::Schema;
use schemars::generate::SchemaSettings;
use serde_json::{Value, json};

use super::KEY_PATTERN;
use super::duration::{Cutoff, parse_duration};
use super::placeholders::{self, Setting};
use super::size::parse_size;
use super::types::Config;

/// Stable `$id`, pinned to the release tag of this version.
pub const SCHEMA_ID: &str = concat!(
    "https://raw.githubusercontent.com/fnando/tidepool/v",
    env!("CARGO_PKG_VERSION"),
    "/schema/tidepool.v1.schema.json"
);

/// Custom string formats registered with the validator.
pub const FORMATS: &[&str] = &[
    "duration",
    "optional-duration",
    "optional-byte-size",
    "cutoff",
    "output-template",
    "file-name-template",
    "tag-template",
    "hook-template",
    "fancy-regex",
    "socket-address",
    "playlist-source",
];

/// Check `value` against one of tidepool's custom formats, explaining failures.
pub fn check_format(format: &str, value: &str) -> Result<(), String> {
    let template = |setting| {
        placeholders::validate(value, setting)
            .map(drop)
            .map_err(|e| format!("{value:?}: {e}"))
    };
    match format {
        "duration" => parse_duration(value).map(drop),
        "optional-duration" if value.is_empty() => Ok(()),
        "optional-duration" => parse_duration(value).map(drop),
        "optional-byte-size" if value.is_empty() => Ok(()),
        "optional-byte-size" => parse_size(value).map(drop),
        "cutoff" => value.parse::<Cutoff>().map(drop),
        "output-template" => template(Setting::Output),
        "file-name-template" => template(Setting::CoverName),
        "tag-template" => template(Setting::Tags),
        "hook-template" => template(Setting::Hook),
        "fancy-regex" => fancy_regex::Regex::new(value)
            .map(drop)
            .map_err(|e| format!("{value:?} is not a valid regex: {}", regex_error_detail(&e))),
        "socket-address" => value
            .parse::<std::net::SocketAddr>()
            .map(drop)
            .map_err(|_| {
                format!("{value:?} is not an address with a port (e.g. \"127.0.0.1:8787\")")
            }),
        "playlist-source" => playlist_id_from_source(value).map(drop),
        other => Err(format!("unknown format {other:?}")),
    }
}

/// The useful part of a fancy-regex error. For errors from the inner `regex`
/// engine, the detail (e.g. an invalid class range) is in the source chain.
fn regex_error_detail(error: &fancy_regex::Error) -> String {
    if let fancy_regex::Error::CompileError(inner) = error
        && let fancy_regex::CompileError::InnerError(build) = inner.as_ref()
    {
        let mut cause: &dyn std::error::Error = build;
        while let Some(next) = cause.source() {
            cause = next;
        }
        let detail = cause.to_string();
        if let Some(last) = detail
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with("error:"))
        {
            return last.trim().trim_start_matches("error:").trim().to_string();
        }
        return detail;
    }
    error.to_string()
}

/// The playlist id in a `source` URL: the `list=` parameter of a YouTube URL.
/// Only URLs are accepted, so channel URLs can be told apart later.
pub fn playlist_id_from_source(source: &str) -> Result<String, String> {
    const HOSTS: [&str; 5] = [
        "youtube.com",
        "www.youtube.com",
        "m.youtube.com",
        "music.youtube.com",
        "youtu.be",
    ];
    let example = "e.g. \"https://www.youtube.com/playlist?list=PLxxxxxxxx\"";
    let Some(rest) = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("http://"))
    else {
        return Err(format!(
            "{source:?} is not a URL; use the playlist's URL ({example})"
        ));
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if !HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
        return Err(format!("{source:?} is not a YouTube URL ({example})"));
    }
    crate::cli::playlist_id_from_url(source).ok_or_else(|| {
        format!("{source:?} is not a playlist URL: it must contain list=<playlist id>")
    })
}

/// Generate the config schema.
pub fn generate() -> Schema {
    let mut schema = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<Config>();
    schema.insert("$id".into(), SCHEMA_ID.into());
    let mut value = schema.to_value();
    strip_null(&mut value);
    Schema::try_from(value).expect("schema is an object")
}

/// TOML has no null, so drop the `null` alternatives schemars adds for
/// `Option` fields: `["string", "null"]` → `"string"`, and
/// `anyOf: [{ $ref }, { type: null }]` → the `$ref` itself.
fn strip_null(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|t| t != "null");
                if types.len() == 1 {
                    let only = types.remove(0);
                    map.insert("type".into(), only);
                }
            }
            for keyword in ["anyOf", "oneOf"] {
                let Some(Value::Array(branches)) = map.get_mut(keyword) else {
                    continue;
                };
                branches.retain(|b| b != &json!({ "type": "null" }));
                if branches.len() == 1 {
                    let Value::Object(only) = branches.remove(0) else {
                        continue;
                    };
                    map.remove(keyword);
                    for (k, v) in only {
                        map.entry(k).or_insert(v);
                    }
                }
            }
            map.values_mut().for_each(strip_null);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_null),
        _ => {}
    }
}

/// The schema as pretty JSON with a trailing newline, as committed in
/// `schema/tidepool.v1.schema.json`.
pub fn schema_json() -> String {
    let mut out = serde_json::to_string_pretty(generate().as_value()).expect("schema serializes");
    out.push('\n');
    out
}

/// The compiled validator, built once.
pub fn validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let mut options = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true);
        for format in FORMATS {
            options = options.with_format(*format, move |s: &str| check_format(format, s).is_ok());
        }
        options
            .build(generate().as_value())
            .expect("the generated config schema is valid")
    })
}

/// Root transform: key patterns for `playlists` and `hooks`.
pub(crate) fn transform_root(schema: &mut Schema) {
    for table in ["playlists", "hooks"] {
        if let Some(Value::Object(prop)) = schema.pointer_mut(&format!("/properties/{table}")) {
            prop.insert("propertyNames".into(), json!({ "pattern": KEY_PATTERN }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_id_and_draft() {
        let schema = generate();
        assert_eq!(schema.get("$id"), Some(&Value::from(SCHEMA_ID)));
        assert_eq!(
            schema.get("$schema"),
            Some(&Value::from("https://json-schema.org/draft/2020-12/schema"))
        );
        assert!(SCHEMA_ID.contains(concat!("/v", env!("CARGO_PKG_VERSION"), "/")));
    }

    #[test]
    fn every_object_denies_unknown_keys() {
        fn walk(value: &Value, path: &str) {
            if let Value::Object(map) = value {
                if map.get("type") == Some(&Value::from("object")) && map.contains_key("properties")
                {
                    assert_eq!(
                        map.get("additionalProperties"),
                        Some(&Value::Bool(false)),
                        "{path} allows unknown keys"
                    );
                }
                for (k, v) in map {
                    walk(v, &format!("{path}/{k}"));
                }
            } else if let Value::Array(items) = value {
                for (i, v) in items.iter().enumerate() {
                    walk(v, &format!("{path}/{i}"));
                }
            }
        }
        walk(generate().as_value(), "");
    }

    #[test]
    fn every_property_has_a_description_or_ref() {
        let schema = generate();
        let defs = schema.get("$defs").and_then(Value::as_object).unwrap();
        for (name, def) in defs {
            let Some(props) = def.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (prop, body) in props {
                let documented = body.get("description").is_some() || body.get("$ref").is_some();
                assert!(documented, "{name}.{prop} has no description");
            }
        }
    }

    #[test]
    fn schema_has_no_null_types() {
        let text = serde_json::to_string(generate().as_value()).unwrap();
        assert!(!text.contains("\"null\""), "{text}");
    }

    #[test]
    fn validator_builds() {
        assert!(validator().is_valid(&json!({})));
        assert!(!validator().is_valid(&json!({ "nope": 1 })));
    }

    #[test]
    fn checks_formats() {
        assert!(check_format("duration", "30d").is_ok());
        assert!(check_format("duration", "").is_err());
        assert!(check_format("optional-duration", "").is_ok());
        assert!(check_format("optional-byte-size", "20GiB").is_ok());
        assert!(check_format("optional-byte-size", "20 gigs").is_err());
        assert!(check_format("cutoff", "none").is_ok());
        assert!(check_format("output-template", "{title}").is_err());
        assert!(check_format("fancy-regex", r"(?i)\blive\b(?! stream)").is_ok());
        assert!(check_format("fancy-regex", "(unclosed").is_err());
        assert!(check_format("hook-template", "{media_path}").is_ok());
        assert!(check_format("tag-template", "{media_path}").is_err());
        let source = |s| check_format("playlist-source", s);
        assert!(source("https://www.youtube.com/playlist?list=PLabc_-1").is_ok());
        assert!(source("https://youtube.com/watch?v=x&list=PLa").is_ok());
        assert!(source("http://music.youtube.com/playlist?list=PLa").is_ok());
        assert!(source("https://youtu.be/dQw4w9WgXcQ?list=PLa").is_ok());
        assert!(source("https://youtube.com/watch?v=x").is_err());
        assert!(source("https://example.com/?list=PLa").is_err());
        assert!(source("PLabc").is_err(), "bare ids are not accepted");
        assert!(source("").is_err());
    }
}
