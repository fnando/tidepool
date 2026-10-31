//! Every invalid fixture produces clear `file:line:col path: message` errors;
//! every valid fixture (and the shipped example) loads.

use std::fs;
use std::path::Path;

use tidepool::config::schema::{SCHEMA_ID, schema_json};
use tidepool::config::{ConfigError, load_str};

fn fixtures(kind: &str) -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/config")
        .join(kind);
    let mut out: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(&p).unwrap())
        })
        .collect();
    out.sort();
    assert!(!out.is_empty());
    out
}

#[test]
fn invalid_fixtures_report_every_error() {
    for (name, text) in fixtures("invalid") {
        let err = match load_str(&name, &text) {
            Ok(_) => panic!("{name} should be invalid"),
            Err(ConfigError::Invalid(report)) => report.to_string(),
            Err(other) => panic!("{name}: unexpected error {other}"),
        };
        for line in err.lines() {
            assert!(
                line.starts_with(&format!("{name}:")),
                "{name}: line without location: {line}"
            );
        }
        insta::assert_snapshot!(name.trim_end_matches(".toml"), err);
    }
}

#[test]
fn valid_fixtures_load() {
    for (name, text) in fixtures("valid") {
        if let Err(err) = load_str(&name, &text) {
            panic!("{name} should be valid:\n{err}");
        }
    }
}

#[test]
fn warnings_are_reported_with_locations() {
    let text =
        fs::read_to_string("tests/fixtures/config/valid/fallback_without_keys.toml").unwrap();
    let loaded = load_str("c.toml", &text).unwrap();
    assert_eq!(
        loaded.warnings,
        vec![
            "c.toml:2:21 warning: youtube.fallback_to_ytdlp: has no effect without api_keys: yt-dlp is always the indexer then"
        ]
    );
}

#[test]
fn example_config_loads_and_points_at_this_schema() {
    let text = fs::read_to_string("examples/config.toml").unwrap();
    let loaded = load_str("examples/config.toml", &text).unwrap_or_else(|e| panic!("{e}"));
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(
        text.lines().next().unwrap(),
        format!("#:schema {SCHEMA_ID}")
    );
}

#[test]
fn committed_schema_is_up_to_date() {
    let committed = fs::read_to_string("schema/tidepool.v1.schema.json").unwrap_or_default();
    assert!(
        committed == schema_json(),
        "schema/tidepool.v1.schema.json is stale; regenerate with:\n  cargo run -- config schema > schema/tidepool.v1.schema.json"
    );
}
