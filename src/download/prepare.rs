//! What `prepare` hooks see and may change: a video's details and where its
//! files go, before anything else uses them.
//!
//! stdin: `{"details": {…}, "paths": {…}, "context": {…}}`; stdout: the same
//! shape, any part of it. A missing key keeps its value; `null` clears a
//! detail. Paths are full paths with `{ext}` (and `{lang}` for subtitles) left
//! in; the ones a hook doesn't return follow the changed details.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

use super::info::VideoInfo;
use super::thumbnail::CoverFormat;
use crate::hooks::parse_output;

/// The details a `prepare` hook may change.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Details {
    pub title: String,
    pub description: Option<String>,
    pub channel: Option<String>,
    /// `YYYY-MM-DD`.
    pub upload_date: Option<String>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
}

/// The paths a `prepare` hook may change, in the order shown.
pub const PATHS: [&str; 5] = [
    "output_dir",
    "output_file",
    "cover_file",
    "subtitle_file",
    "metadata_file",
];

impl Details {
    pub fn of(info: &VideoInfo) -> Self {
        Self {
            title: info.title.clone(),
            description: info.description.clone(),
            channel: info.channel.clone(),
            upload_date: info.upload_date_iso(),
            tags: info.tags.clone().unwrap_or_default(),
            categories: info.categories.clone().unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "description": self.description,
            "channel": self.channel,
            "upload_date": self.upload_date,
            "tags": self.tags,
            "categories": self.categories,
        })
    }

    /// Put these details into `info`. A changed upload date also replaces
    /// the exact publish time, which would contradict it.
    pub fn apply_to(&self, info: &mut VideoInfo) {
        if info.upload_date_iso() != self.upload_date {
            info.upload_date = self.upload_date.as_ref().map(|d| d.replace('-', ""));
            info.timestamp = None;
        }
        info.title = self.title.clone();
        info.description = self.description.clone();
        info.channel = self.channel.clone();
        info.tags = Some(self.tags.clone());
        info.categories = Some(self.categories.clone());
    }

    /// `original`'s value of each detail that differs here, for
    /// metadata.json; `None` when nothing changed.
    pub fn originals(&self, original: &Details) -> Option<Value> {
        let (now, before) = (self.to_json(), original.to_json());
        let changed: Map<String, Value> = before
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, value)| now.get(key.as_str()) != Some(*value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        (!changed.is_empty()).then_some(Value::Object(changed))
    }
}

/// A `prepare` hook's input.
pub fn input(details: &Details, paths: &BTreeMap<String, String>, context: &Value) -> Vec<u8> {
    let paths: Map<String, Value> = PATHS
        .iter()
        .filter_map(|key| Some((key.to_string(), json!(paths.get(*key)?))))
        .collect();
    serde_json::to_vec(&json!({
        "details": details.to_json(),
        "paths": paths,
        "context": context,
    }))
    .expect("JSON")
}

/// Apply a `prepare` hook's output to `details` and `overrides` (the paths
/// hooks set so far). `proposed` is what the paths are now; a changed path
/// must stay inside `media_dir`. Nothing changes when it's invalid.
pub fn apply(
    name: &str,
    output: &[u8],
    details: &mut Details,
    overrides: &mut BTreeMap<String, String>,
    proposed: &BTreeMap<String, String>,
    media_dir: &Path,
) -> Result<(), String> {
    let Some(value) = parse_output(name, output)? else {
        return Ok(());
    };
    let mut new_details = details.clone();
    if let Some(changes) = value.get("details") {
        let changes = changes
            .as_object()
            .ok_or_else(|| format!("hook {name}: details must be an object"))?;
        for (key, value) in changes {
            let what = format!("hook {name}: details.{key}");
            match key.as_str() {
                "title" => match value.as_str().map(str::trim) {
                    Some(title) if !title.is_empty() => new_details.title = title.to_string(),
                    _ => return Err(format!("{what} must be a non-empty string")),
                },
                "description" => new_details.description = optional_text(&what, value)?,
                "channel" => new_details.channel = optional_text(&what, value)?,
                "upload_date" => new_details.upload_date = date(&what, value)?,
                "tags" => new_details.tags = list(&what, value)?,
                "categories" => new_details.categories = list(&what, value)?,
                _ => {
                    return Err(format!(
                        "{what} can't be changed (only title, description, channel, \
                         upload_date, tags and categories)"
                    ));
                }
            }
        }
    }
    let mut new_overrides = overrides.clone();
    if let Some(paths) = value.get("paths") {
        let paths = paths
            .as_object()
            .ok_or_else(|| format!("hook {name}: paths must be an object"))?;
        for (key, value) in paths {
            let what = format!("hook {name}: paths.{key}");
            if !PATHS.contains(&key.as_str()) {
                return Err(format!(
                    "{what} isn't a path it can change ({})",
                    PATHS.join(", ")
                ));
            }
            let path = value
                .as_str()
                .ok_or_else(|| format!("{what} must be a path string"))?;
            let normalized = check_path(&what, key, path, proposed.get(key), media_dir)?;
            new_overrides.insert(key.clone(), normalized);
        }
    }
    *details = new_details;
    *overrides = new_overrides;
    Ok(())
}

fn optional_text(what: &str, value: &Value) -> Result<Option<String>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(format!("{what} must be a string or null")),
    }
}

fn date(what: &str, value: &Value) -> Result<Option<String>, String> {
    match optional_text(what, value)? {
        None => Ok(None),
        Some(d) if d.len() == 10 && d.parse::<jiff::civil::Date>().is_ok() => Ok(Some(d)),
        Some(d) => Err(format!("{what} {d:?} must be a date like 2025-03-01")),
    }
}

fn list(what: &str, value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{what} must be a list of strings"))
            })
            .collect(),
        _ => Err(format!("{what} must be a list of strings")),
    }
}

/// A path a hook returned: full, without `..`, inside `media_dir` unless it's
/// what was proposed, and keeping the placeholders and cover format its
/// setting needs. Returns it normalized.
fn check_path(
    what: &str,
    key: &str,
    path: &str,
    proposed: Option<&String>,
    media_dir: &Path,
) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!("{what} {path:?} must be a full path"));
    }
    let mut normalized = PathBuf::from("/");
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(format!("{what} {path:?} can't use \"..\""));
            }
        }
    }
    let normalized_text = normalized.display().to_string();
    if proposed != Some(&normalized_text) && !normalized.starts_with(media_dir) {
        return Err(format!(
            "{what} {path:?} must be inside the media folder {}",
            media_dir.display()
        ));
    }
    let needs: &[&str] = match key {
        "output_file" => &["{ext}"],
        "subtitle_file" => &["{lang}", "{ext}"],
        _ => &[],
    };
    for placeholder in needs {
        if !path.contains(placeholder) {
            return Err(format!("{what} {path:?} must keep {placeholder}"));
        }
    }
    if key == "cover_file" {
        let format = |p: &Path| {
            p.extension()
                .and_then(|e| CoverFormat::from_extension(&e.to_string_lossy()))
        };
        let wanted = proposed.and_then(|p| format(Path::new(p)));
        if format(&normalized) != wanted {
            return Err(format!(
                "{what} {path:?} must end in .{} like cover_file",
                wanted.map(CoverFormat::extension).unwrap_or("jpg")
            ));
        }
    }
    Ok(normalized_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details() -> Details {
        Details {
            title: "AFI - 17 Crimes".into(),
            description: Some("desc".into()),
            channel: Some("AFI AFIREINSIDE".into()),
            upload_date: Some("2013-08-16".into()),
            tags: vec!["afi".into()],
            categories: vec!["Music".into()],
        }
    }

    fn proposed() -> BTreeMap<String, String> {
        [
            ("output_dir", "/media/afi/17 Crimes"),
            ("output_file", "/media/afi/17 Crimes/17 Crimes.{ext}"),
            ("cover_file", "/media/afi/17 Crimes/poster.jpg"),
            (
                "subtitle_file",
                "/media/afi/17 Crimes/17 Crimes.{lang}.{ext}",
            ),
            ("metadata_file", "/data/meta/x.json"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn go(output: &str) -> Result<(Details, BTreeMap<String, String>), String> {
        let mut d = details();
        let mut o = BTreeMap::new();
        apply(
            "h",
            output.as_bytes(),
            &mut d,
            &mut o,
            &proposed(),
            Path::new("/media"),
        )?;
        Ok((d, o))
    }

    #[test]
    fn missing_keys_keep_and_null_clears() {
        let (d, o) = go(r#"{"details": {"title": "17 Crimes", "channel": "AFI", "description": null, "tags": null}}"#).unwrap();
        assert_eq!(d.title, "17 Crimes");
        assert_eq!(d.channel.as_deref(), Some("AFI"));
        assert_eq!(d.description, None);
        assert!(d.tags.is_empty());
        assert_eq!(d.upload_date.as_deref(), Some("2013-08-16"));
        assert!(o.is_empty());
        assert_eq!(go("").unwrap().0, details(), "empty output: no change");
    }

    #[test]
    fn originals_lists_what_changed() {
        let (d, _) = go(r#"{"details": {"title": "17 Crimes"}}"#).unwrap();
        assert_eq!(
            d.originals(&details()),
            Some(json!({"title": "AFI - 17 Crimes"}))
        );
        assert_eq!(details().originals(&details()), None);
    }

    #[test]
    fn paths_are_checked() {
        let (_, o) = go(r#"{"paths": {"output_dir": "/media/AFI/./17 Crimes"}}"#).unwrap();
        assert_eq!(o["output_dir"], "/media/AFI/17 Crimes");
        // What was proposed may stay outside the media folder.
        assert!(go(r#"{"paths": {"metadata_file": "/data/meta/x.json"}}"#).is_ok());
        for (output, error) in [
            (
                r#"{"paths": {"output_dir": "/elsewhere"}}"#,
                "inside the media folder",
            ),
            (r#"{"paths": {"output_dir": "/media/../etc"}}"#, "\"..\""),
            (r#"{"paths": {"output_dir": "media/x"}}"#, "full path"),
            (
                r#"{"paths": {"output_file": "/media/x.mkv"}}"#,
                "keep {ext}",
            ),
            (
                r#"{"paths": {"subtitle_file": "/media/x.{ext}"}}"#,
                "keep {lang}",
            ),
            (
                r#"{"paths": {"cover_file": "/media/x.png"}}"#,
                "must end in .jpg",
            ),
            (
                r#"{"paths": {"work_dir": "/media/x"}}"#,
                "isn't a path it can change",
            ),
            (r#"{"details": {"id": "x"}}"#, "can't be changed"),
            (r#"{"details": {"title": ""}}"#, "non-empty"),
            (
                r#"{"details": {"upload_date": "16/08/2013"}}"#,
                "a date like",
            ),
            (r#"{"details": {"tags": "afi"}}"#, "list of strings"),
        ] {
            let err = go(output).unwrap_err();
            assert!(err.contains(error), "{output}: {err}");
        }
    }

    #[test]
    fn a_new_upload_date_replaces_the_publish_time() {
        let mut info = VideoInfo::parse(
            r#"{"id":"x","title":"t","upload_date":"20130816","timestamp":1376611200}"#,
        )
        .unwrap();
        let mut d = Details::of(&info);
        d.apply_to(&mut info);
        assert_eq!(info.timestamp, Some(1376611200), "unchanged date keeps it");
        d.upload_date = Some("2014-01-02".into());
        d.apply_to(&mut info);
        assert_eq!(
            (info.upload_date.as_deref(), info.timestamp),
            (Some("20140102"), None)
        );
    }
}
