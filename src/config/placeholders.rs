//! `{placeholder}` templates shared by the path settings (`output_dir`,
//! `output_file`, `cover_file`, `subtitle_file`, `metadata_file`, `tmp_dir`),
//! `embed.tags` and hook `command` arrays.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Which setting a template belongs to; decides which placeholders are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// `output_dir`: the video's directory, a full path.
    OutputDir,
    /// `output_file`: the media file, a full path.
    OutputFile,
    /// `cover_file`: the poster, a full path ending in .jpg, .png or .webp.
    CoverFile,
    /// `subtitle_file`: each subtitle, a full path.
    SubtitleFile,
    /// `metadata_file`: metadata.json, a full path.
    MetadataFile,
    /// `tmp_dir`: the work area, a full path.
    TmpDir,
    /// `embed.tags` values.
    Tags,
    /// Hook `command` elements.
    Hook,
}

impl Setting {
    fn bit(self) -> u8 {
        match self {
            Setting::OutputDir => OUT_DIR,
            Setting::OutputFile => OUT_FILE,
            Setting::CoverFile => COV,
            Setting::SubtitleFile => SUB,
            Setting::MetadataFile => META,
            Setting::TmpDir => TMP,
            Setting::Tags => TAG,
            Setting::Hook => HOOK,
        }
    }

    /// Whether rendered values are sanitized for use in file names.
    pub fn sanitizes(self) -> bool {
        self.bit() & PATHS != 0
    }

    /// Settings that are full paths.
    fn is_path(self) -> bool {
        self.bit() & (PATHS | TMP) != 0
    }

    /// The setting's name in the config.
    pub fn name(self) -> &'static str {
        match self {
            Setting::OutputDir => "output_dir",
            Setting::OutputFile => "output_file",
            Setting::CoverFile => "cover_file",
            Setting::SubtitleFile => "subtitle_file",
            Setting::MetadataFile => "metadata_file",
            Setting::TmpDir => "tmp_dir",
            Setting::Tags => "embed.tags",
            Setting::Hook => "hook command",
        }
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

const OUT_DIR: u8 = 1;
const OUT_FILE: u8 = 2;
const TAG: u8 = 4;
const HOOK: u8 = 8;
const TMP: u8 = 16;
const COV: u8 = 32;
const SUB: u8 = 64;
const META: u8 = 128;
/// The per-video path settings.
const PATHS: u8 = OUT_DIR | OUT_FILE | COV | SUB | META;
/// The files inside (or relative to) `output_dir`.
const FILES: u8 = OUT_FILE | COV | SUB | META;
const ALL: u8 = PATHS | TAG | HOOK;

/// Poster extensions `cover_file` may end with.
pub const COVER_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// One known placeholder.
#[derive(Debug)]
pub struct Placeholder {
    pub name: &'static str,
    /// Accepts zero padding (`{position:03}`).
    pub numeric: bool,
    allowed: u8,
    /// Matching environment variable passed to hooks.
    pub env: Option<&'static str>,
}

impl Placeholder {
    pub fn allowed_in(&self, setting: Setting) -> bool {
        self.allowed & setting.bit() != 0
    }
}

macro_rules! ph {
    ($name:literal, $numeric:literal, $allowed:expr, $env:expr) => {
        Placeholder {
            name: $name,
            numeric: $numeric,
            allowed: $allowed,
            env: $env,
        }
    };
}

/// Every placeholder tidepool knows, with where it may be used.
pub const PLACEHOLDERS: &[Placeholder] = &[
    ph!("id", false, ALL, Some("TIDEPOOL_VIDEO_ID")),
    ph!("video_id", false, ALL, Some("TIDEPOOL_VIDEO_ID")),
    ph!("title", false, ALL, Some("TIDEPOOL_TITLE")),
    ph!("normalized_title", false, ALL, None),
    ph!("channel", false, ALL, Some("TIDEPOOL_CHANNEL")),
    ph!("channel_id", false, ALL, Some("TIDEPOOL_CHANNEL_ID")),
    ph!("upload_date", false, ALL, Some("TIDEPOOL_UPLOAD_DATE")),
    ph!("upload_year", true, ALL, None),
    ph!("duration", true, ALL, Some("TIDEPOOL_DURATION")),
    ph!("position", true, ALL, Some("TIDEPOOL_POSITION")),
    ph!("source", false, ALL, Some("TIDEPOOL_SOURCE")),
    ph!("source_title", false, ALL, Some("TIDEPOOL_SOURCE_TITLE")),
    ph!("url", false, TAG | HOOK, Some("TIDEPOOL_URL")),
    ph!("category", false, ALL, Some("TIDEPOOL_CATEGORY")),
    ph!("media_dir", false, PATHS | TMP, None),
    ph!("data_dir", false, PATHS | TMP, None),
    ph!(
        "output_dir",
        false,
        FILES | HOOK,
        Some("TIDEPOOL_OUTPUT_DIR")
    ),
    ph!("ext", false, OUT_FILE | SUB, None),
    ph!("lang", false, SUB, None),
    ph!("output_file", false, HOOK, Some("TIDEPOOL_OUTPUT_FILE")),
    ph!("cover_file", false, HOOK, Some("TIDEPOOL_COVER_FILE")),
    ph!("metadata_file", false, HOOK, Some("TIDEPOOL_METADATA_FILE")),
];

/// Look up a placeholder by name.
pub fn lookup(name: &str) -> Option<&'static Placeholder> {
    PLACEHOLDERS.iter().find(|p| p.name == name)
}

/// A parsed template piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Placeholder { name: String, pad: Option<usize> },
}

/// Why a template is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    Unbalanced,
    Unknown(String),
    NotAllowed { name: String, setting: Setting },
    BadFormatSpec(String),
    Missing(&'static str),
    CoverExtension,
    NotFullPath(Setting),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::Unbalanced => {
                f.write_str("unbalanced braces (use {{ and }} for literal braces)")
            }
            TemplateError::Unknown(name) => write!(f, "unknown placeholder {{{name}}}"),
            TemplateError::NotAllowed { name, setting } => {
                write!(f, "placeholder {{{name}}} is not allowed in {setting}")
            }
            TemplateError::BadFormatSpec(name) => write!(
                f,
                "invalid format in {{{name}}} (only numeric placeholders take zero padding, e.g. {{position:03}})"
            ),
            TemplateError::Missing(name) => write!(f, "must contain {{{name}}}"),
            TemplateError::CoverExtension => f.write_str("must end with .jpg, .jpeg, .png or .webp"),
            TemplateError::NotFullPath(setting) if setting.bit() & FILES != 0 => f.write_str(
                "must be a full path: start with /, ~, $VAR, {media_dir}, {data_dir} or {output_dir}",
            ),
            TemplateError::NotFullPath(_) => {
                f.write_str("must be a full path: start with /, ~, $VAR, {media_dir} or {data_dir}")
            }
        }
    }
}

/// Split a template into literals and placeholders.
pub fn parse(template: &str) -> Result<Vec<Segment>, TemplateError> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => return Err(TemplateError::Unbalanced),
            '{' => {
                let mut inner = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') | None => return Err(TemplateError::Unbalanced),
                        Some(c) => inner.push(c),
                    }
                }
                if inner.is_empty() {
                    return Err(TemplateError::Unbalanced);
                }
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                let (name, spec) = match inner.split_once(':') {
                    Some((name, spec)) => (name.to_string(), Some(spec)),
                    None => (inner, None),
                };
                let pad = match spec {
                    None => None,
                    Some(spec) => {
                        let width = spec
                            .strip_prefix('0')
                            .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()))
                            .and_then(|w| w.parse().ok())
                            .ok_or_else(|| TemplateError::BadFormatSpec(name.clone()))?;
                        Some(width)
                    }
                };
                segments.push(Segment::Placeholder { name, pad });
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    Ok(segments)
}

/// Parse and check a template for a setting.
pub fn validate(template: &str, setting: Setting) -> Result<Vec<Segment>, TemplateError> {
    let segments = parse(template)?;
    let mut used: Vec<&str> = Vec::new();
    for segment in &segments {
        if let Segment::Placeholder { name, pad } = segment {
            let placeholder = lookup(name).ok_or_else(|| TemplateError::Unknown(name.clone()))?;
            if !placeholder.allowed_in(setting) {
                return Err(TemplateError::NotAllowed {
                    name: name.clone(),
                    setting,
                });
            }
            if pad.is_some() && !placeholder.numeric {
                return Err(TemplateError::BadFormatSpec(name.clone()));
            }
            used.push(placeholder.name);
        }
    }
    if setting.is_path() {
        let starts_full = match segments.first() {
            Some(Segment::Literal(text)) => is_full_path(text),
            Some(Segment::Placeholder { name, .. }) => {
                matches!(name.as_str(), "media_dir" | "data_dir" | "output_dir")
            }
            None => false,
        };
        if !starts_full {
            return Err(TemplateError::NotFullPath(setting));
        }
    }
    let required: &[&'static str] = match setting {
        Setting::OutputFile => &["ext"],
        Setting::SubtitleFile => &["lang", "ext"],
        _ => &[],
    };
    if let Some(missing) = required.iter().find(|r| !used.contains(r)) {
        return Err(TemplateError::Missing(missing));
    }
    if setting == Setting::CoverFile {
        let extension = match segments.last() {
            Some(Segment::Literal(text)) => cover_extension(text),
            _ => None,
        };
        if extension.is_none() {
            return Err(TemplateError::CoverExtension);
        }
    }
    Ok(segments)
}

/// The poster extension a `cover_file` ends with, lowercased.
pub fn cover_extension(path: &str) -> Option<String> {
    let (_, extension) = path.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    COVER_EXTENSIONS
        .contains(&extension.as_str())
        .then_some(extension)
}

/// A title for file names: transliterated to ASCII, then sanitized.
pub fn normalize_title(title: &str) -> String {
    sanitize(&deunicode::deunicode(title))
}

/// A path that doesn't depend on the current directory: `/…`, `~`, `~/…` or
/// `$VAR…`.
pub fn is_full_path(path: &str) -> bool {
    path.starts_with('/') || path == "~" || path.starts_with("~/") || path.starts_with('$')
}

/// Strip characters that are unsafe in file names, collapse whitespace and cap
/// the result at 120 bytes on a character boundary.
pub fn sanitize(value: &str) -> String {
    const MAX_BYTES: usize = 120;
    let cleaned: String = value
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let mut out = String::with_capacity(cleaned.len().min(MAX_BYTES));
    for word in cleaned.split(' ').filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.truncate(out.trim_end().len());
    }
    out
}

/// Render parsed segments. `value` returns `None` for unknown values, which
/// render as `NA` when sanitizing (file names) and as an empty string otherwise.
pub fn render(
    segments: &[Segment],
    setting: Setting,
    value: impl Fn(&str) -> Option<String>,
) -> String {
    let mut out = String::new();
    for segment in segments {
        match segment {
            Segment::Literal(text) => out.push_str(text),
            Segment::Placeholder { name, pad } => {
                let rendered = match value(name) {
                    Some(v) => {
                        let v = match pad {
                            Some(width) => format!("{v:0>width$}"),
                            None => v,
                        };
                        if setting.sanitizes() { sanitize(&v) } else { v }
                    }
                    None if setting.sanitizes() => "NA".to_string(),
                    None => String::new(),
                };
                out.push_str(&rendered);
            }
        }
    }
    out
}

/// Render a full-path template (`output`, `tmp_dir`): expand `~` and `$VARS`
/// in the template itself, fill in placeholders (values used as given), and
/// normalize the result. Fails unless it's a full path without `..`.
pub fn render_path(
    template: &str,
    setting: Setting,
    home: &Path,
    value: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    let expanded = crate::paths::expand(template, home)?;
    let expanded = expanded.to_string_lossy();
    let segments = validate(&expanded, setting).map_err(|e| format!("{template:?}: {e}"))?;
    let rendered = render(&segments, Setting::Tags, value);
    let mut path = PathBuf::from("/");
    for component in Path::new(&rendered).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => path.push(part),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(format!(
                    "{template:?} renders to {rendered:?}, which uses \"..\""
                ));
            }
        }
    }
    if !rendered.starts_with('/') || path == Path::new("/") {
        return Err(format!(
            "{template:?} renders to {rendered:?}, which isn't a full path"
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Segment {
        Segment::Literal(s.into())
    }
    fn ph(name: &str, pad: Option<usize>) -> Segment {
        Segment::Placeholder {
            name: name.into(),
            pad,
        }
    }

    #[test]
    fn parses_literals_placeholders_and_escapes() {
        assert_eq!(
            parse("{upload_date} - {title} [{id}]").unwrap(),
            vec![
                ph("upload_date", None),
                lit(" - "),
                ph("title", None),
                lit(" ["),
                ph("id", None),
                lit("]")
            ]
        );
        assert_eq!(
            parse("{{x}} {position:03}").unwrap(),
            vec![lit("{x} "), ph("position", Some(3))]
        );
        assert_eq!(parse("plain").unwrap(), vec![lit("plain")]);
    }

    #[test]
    fn rejects_unbalanced_braces() {
        for bad in ["{title", "title}", "{ti{tle}", "{}"] {
            assert_eq!(parse(bad), Err(TemplateError::Unbalanced), "{bad:?}");
        }
    }

    #[test]
    fn validates_per_setting() {
        use Setting::*;
        let ok = [
            (
                "{media_dir}/{source}/{upload_date} - {title} [{id}]",
                OutputDir,
            ),
            ("{media_dir}/{source}", OutputDir),
            ("/nas/{id}", OutputDir),
            ("~/Videos/{id}", OutputDir),
            ("$MEDIA/{id}", OutputDir),
            ("{data_dir}/x/{id}", OutputDir),
            ("{output_dir}/{normalized_title}.{ext}", OutputFile),
            ("/elsewhere/{id}.{ext}", OutputFile),
            ("{output_dir}/{normalized_title}-poster.jpg", CoverFile),
            ("{output_dir}/cover.WEBP", CoverFile),
            ("{output_dir}/{normalized_title}.{lang}.{ext}", SubtitleFile),
            ("{output_dir}/metadata.json", MetadataFile),
            ("{data_dir}/tmp", TmpDir),
            ("{channel}", Tags),
            ("{output_file}", Hook),
            ("{output_dir}/x", Hook),
            ("{cover_file} {metadata_file}", Hook),
        ];
        for (template, setting) in ok {
            assert!(
                validate(template, setting).is_ok(),
                "{template:?} in {setting}"
            );
        }
        let bad = [
            (
                "{source}/{id}",
                OutputDir,
                TemplateError::NotFullPath(OutputDir),
            ),
            (
                "videos/{id}",
                OutputDir,
                TemplateError::NotFullPath(OutputDir),
            ),
            ("./{id}", OutputDir, TemplateError::NotFullPath(OutputDir)),
            ("~me/{id}", OutputDir, TemplateError::NotFullPath(OutputDir)),
            ("tmp", TmpDir, TemplateError::NotFullPath(TmpDir)),
            (
                "{title}.{ext}",
                OutputFile,
                TemplateError::NotFullPath(OutputFile),
            ),
            (
                "{output_dir}/video.mkv",
                OutputFile,
                TemplateError::Missing("ext"),
            ),
            (
                "{output_dir}/sub.{ext}",
                SubtitleFile,
                TemplateError::Missing("lang"),
            ),
            (
                "{output_dir}/poster",
                CoverFile,
                TemplateError::CoverExtension,
            ),
            (
                "{output_dir}/poster.gif",
                CoverFile,
                TemplateError::CoverExtension,
            ),
            (
                "{output_dir}/poster.{ext}",
                CoverFile,
                TemplateError::NotAllowed {
                    name: "ext".into(),
                    setting: CoverFile,
                },
            ),
            (
                "{output_dir}/x",
                OutputDir,
                TemplateError::NotAllowed {
                    name: "output_dir".into(),
                    setting: OutputDir,
                },
            ),
            (
                "{data_dir}/{title}",
                TmpDir,
                TemplateError::NotAllowed {
                    name: "title".into(),
                    setting: TmpDir,
                },
            ),
            (
                "{nope} {id}",
                OutputDir,
                TemplateError::Unknown("nope".into()),
            ),
            (
                "{output_file} {id}",
                OutputDir,
                TemplateError::NotAllowed {
                    name: "output_file".into(),
                    setting: OutputDir,
                },
            ),
            (
                "{output_file}",
                OutputDir,
                TemplateError::NotAllowed {
                    name: "output_file".into(),
                    setting: OutputDir,
                },
            ),
            (
                "{title:03}",
                Hook,
                TemplateError::BadFormatSpec("title".into()),
            ),
            (
                "{position:x}",
                Hook,
                TemplateError::BadFormatSpec("position".into()),
            ),
        ];
        for (template, setting, error) in bad {
            assert_eq!(
                validate(template, setting),
                Err(error),
                "{template:?} in {setting}"
            );
        }
    }

    #[test]
    fn normalizes_titles() {
        assert_eq!(
            normalize_title("Café Ñoño: Live at 東京 / 2024 (Official)?"),
            "Cafe Nono Live at Dong Jing 2024 (Official)"
        );
        assert_eq!(
            normalize_title("Ölçü — “quotes”"),
            "Olcu -- \"quotes\"".replace('"', "")
        );
    }

    #[test]
    fn sanitizes_values() {
        assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), "abcdefghij");
        assert_eq!(sanitize("  many   spaces\there \n"), "many spaces here");
        let long = "é".repeat(100); // 200 bytes
        let out = sanitize(&long);
        assert!(out.len() <= 120 && out.chars().all(|c| c == 'é'));
        assert_eq!(out.len(), 120);
    }

    #[test]
    fn renders_with_padding_sanitizing_and_missing_values() {
        let segs = validate(
            "/x/{position:03} - {title} [{id}] {channel}",
            Setting::OutputDir,
        )
        .unwrap();
        let out = render(&segs, Setting::OutputDir, |name| match name {
            "position" => Some("7".into()),
            "title" => Some("a/b: c".into()),
            "id" => Some("dQw4w9WgXcQ".into()),
            _ => None,
        });
        assert_eq!(out, "/x/007 - ab c [dQw4w9WgXcQ] NA");

        let segs = validate("New: {title} {channel}", Setting::Hook).unwrap();
        let out = render(&segs, Setting::Hook, |name| {
            (name == "title").then(|| "a/b".into())
        });
        assert_eq!(out, "New: a/b ");
    }

    #[test]
    fn every_env_var_is_prefixed() {
        for p in PLACEHOLDERS {
            if let Some(env) = p.env {
                assert!(env.starts_with("TIDEPOOL_"), "{}", p.name);
            }
        }
    }
}
