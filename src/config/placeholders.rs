//! `{placeholder}` templates shared by `output`, `cover_name`, `embed.tags`
//! and hook `command` arrays.

use std::fmt;

/// Which setting a template belongs to; decides which placeholders are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// `output`: the video's directory path under `root`.
    Output,
    /// `cover_name`: the poster file name, no extension.
    CoverName,
    /// `embed.tags` values.
    Tags,
    /// Hook `command` elements.
    Hook,
}

impl Setting {
    fn bit(self) -> u8 {
        match self {
            Setting::Output => OUT,
            Setting::CoverName => COV,
            Setting::Tags => TAG,
            Setting::Hook => HOOK,
        }
    }

    /// Whether rendered values are sanitized for use in file names.
    pub fn sanitizes(self) -> bool {
        matches!(self, Setting::Output | Setting::CoverName)
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Setting::Output => "output",
            Setting::CoverName => "cover_name",
            Setting::Tags => "embed.tags",
            Setting::Hook => "hook command",
        })
    }
}

const OUT: u8 = 1;
const COV: u8 = 2;
const TAG: u8 = 4;
const HOOK: u8 = 8;
const ALL: u8 = OUT | COV | TAG | HOOK;

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
    ph!("channel", false, ALL, Some("TIDEPOOL_CHANNEL")),
    ph!("channel_id", false, ALL, Some("TIDEPOOL_CHANNEL_ID")),
    ph!("upload_date", false, ALL, Some("TIDEPOOL_UPLOAD_DATE")),
    ph!("upload_year", true, ALL, None),
    ph!("duration", true, ALL, Some("TIDEPOOL_DURATION")),
    ph!("position", true, ALL, Some("TIDEPOOL_POSITION")),
    ph!("playlist", false, ALL, Some("TIDEPOOL_PLAYLIST_KEY")),
    ph!("playlist_dir", false, OUT | HOOK, None),
    ph!("playlist_id", false, ALL, Some("TIDEPOOL_PLAYLIST_ID")),
    ph!(
        "playlist_title",
        false,
        ALL,
        Some("TIDEPOOL_PLAYLIST_TITLE")
    ),
    ph!("url", false, TAG | HOOK, Some("TIDEPOOL_URL")),
    ph!("library", false, HOOK, Some("TIDEPOOL_LIBRARY")),
    ph!("video_dir", false, HOOK, Some("TIDEPOOL_VIDEO_DIR")),
    ph!("media_path", false, HOOK, Some("TIDEPOOL_MEDIA_PATH")),
    ph!("cover_path", false, HOOK, Some("TIDEPOOL_COVER_PATH")),
    ph!("metadata_path", false, HOOK, Some("TIDEPOOL_METADATA_PATH")),
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
    MissingId,
    PathSeparator,
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
            TemplateError::MissingId => f.write_str("must contain {id}"),
            TemplateError::PathSeparator => f.write_str("must not contain path separators"),
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
    let mut has_id = false;
    for segment in &segments {
        match segment {
            Segment::Literal(text) => {
                if setting == Setting::CoverName && text.contains(['/', '\\']) {
                    return Err(TemplateError::PathSeparator);
                }
            }
            Segment::Placeholder { name, pad } => {
                let placeholder =
                    lookup(name).ok_or_else(|| TemplateError::Unknown(name.clone()))?;
                if !placeholder.allowed_in(setting) {
                    return Err(TemplateError::NotAllowed {
                        name: name.clone(),
                        setting,
                    });
                }
                if pad.is_some() && !placeholder.numeric {
                    return Err(TemplateError::BadFormatSpec(name.clone()));
                }
                has_id |= matches!(placeholder.name, "id" | "video_id");
            }
        }
    }
    if setting == Setting::Output && !has_id {
        return Err(TemplateError::MissingId);
    }
    Ok(segments)
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
        assert!(
            validate(
                "{playlist_dir}/{upload_date} - {title} [{id}]",
                Setting::Output
            )
            .is_ok()
        );
        assert_eq!(
            validate("{title}", Setting::Output),
            Err(TemplateError::MissingId)
        );
        assert_eq!(
            validate("{nope} {id}", Setting::Output),
            Err(TemplateError::Unknown("nope".into()))
        );
        assert_eq!(
            validate("{media_path} {id}", Setting::Output),
            Err(TemplateError::NotAllowed {
                name: "media_path".into(),
                setting: Setting::Output
            })
        );
        assert_eq!(
            validate("a/{title}", Setting::CoverName),
            Err(TemplateError::PathSeparator)
        );
        assert!(validate("{title}-poster", Setting::CoverName).is_ok());
        assert!(validate("{channel}", Setting::Tags).is_ok());
        assert!(validate("{media_path}", Setting::Hook).is_ok());
        assert_eq!(
            validate("{title:03}", Setting::Hook),
            Err(TemplateError::BadFormatSpec("title".into()))
        );
        assert_eq!(
            validate("{position:x}", Setting::Hook),
            Err(TemplateError::BadFormatSpec("position".into()))
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
        let segs = validate("{position:03} - {title} [{id}] {channel}", Setting::Output).unwrap();
        let out = render(&segs, Setting::Output, |name| match name {
            "position" => Some("7".into()),
            "title" => Some("a/b: c".into()),
            "id" => Some("dQw4w9WgXcQ".into()),
            _ => None,
        });
        assert_eq!(out, "007 - ab c [dQw4w9WgXcQ] NA");

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
