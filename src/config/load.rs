//! The load pipeline: parse TOML (keeping spans) → validate against the JSON
//! Schema (all errors) → deserialize → semantic checks. Every problem is
//! reported as `file:line:col path: message`.

use std::fmt;
use std::ops::Range;
use std::path::Path;

use jsonschema::ValidationError;
use jsonschema::error::ValidationErrorKind;
use serde_json::Value;
use toml_edit::Document;

use super::schema::{check_format, validator};
use super::spans::{dotted, line_col, locate, pointer_segments, to_json};
use super::types::Config;
use super::{KEY_PATTERN, semantic};

/// The config format version this build understands.
pub const CONFIG_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One problem found in a config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// JSON pointer of the offending value (`""` = the whole file).
    pub pointer: String,
    pub message: String,
    /// Point at the key instead of the value (e.g. unknown keys).
    pub at_key: bool,
}

impl Diagnostic {
    pub fn error(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            pointer: pointer.into(),
            message: message.into(),
            at_key: false,
        }
    }

    pub fn warning(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(pointer, message)
        }
    }

    fn at_key(mut self) -> Self {
        self.at_key = true;
        self
    }
}

/// Diagnostics rendered against their source text.
#[derive(Debug, Clone)]
pub struct Report {
    pub source_name: String,
    pub lines: Vec<RenderedDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDiagnostic {
    pub severity: Severity,
    pub location: Option<(usize, usize)>,
    pub path: String,
    pub message: String,
}

impl Report {
    /// Render diagnostics, ordered by position in the file.
    pub fn new(
        source_name: &str,
        text: &str,
        doc: Option<&Document<String>>,
        diagnostics: Vec<Diagnostic>,
    ) -> Self {
        let mut lines: Vec<(Option<Range<usize>>, RenderedDiagnostic)> = diagnostics
            .into_iter()
            .map(|d| {
                let span = doc.and_then(|doc| locate(doc, &d.pointer, d.at_key));
                let location = span.as_ref().map(|s| line_col(text, s.start));
                (
                    span,
                    RenderedDiagnostic {
                        severity: d.severity,
                        location,
                        path: dotted(&d.pointer),
                        message: d.message,
                    },
                )
            })
            .collect();
        lines.sort_by(|(a, x), (b, y)| {
            (a.as_ref().map(|s| s.start), &x.path, &x.message).cmp(&(
                b.as_ref().map(|s| s.start),
                &y.path,
                &y.message,
            ))
        });
        lines.dedup_by(|(_, a), (_, b)| a == b);
        Self {
            source_name: source_name.to_string(),
            lines: lines.into_iter().map(|(_, l)| l).collect(),
        }
    }

    pub fn has_errors(&self) -> bool {
        self.lines.iter().any(|l| l.severity == Severity::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = String> + '_ {
        self.lines
            .iter()
            .filter(|l| l.severity == Severity::Warning)
            .map(|l| self.render(l))
    }

    fn render(&self, line: &RenderedDiagnostic) -> String {
        let mut out = self.source_name.clone();
        if let Some((l, c)) = line.location {
            out.push_str(&format!(":{l}:{c}"));
        }
        if line.severity == Severity::Warning {
            out.push_str(" warning:");
        }
        if !line.path.is_empty() {
            out.push_str(&format!(" {}:", line.path));
        }
        out.push(' ');
        out.push_str(&line.message);
        out
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            f.write_str(&self.render(line))?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(Report),
}

/// A validated config.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// Normalized JSON (sorted keys), as stored in the DB.
    pub json: Value,
    /// Rendered warnings (`file:line:col warning: path: message`).
    pub warnings: Vec<String>,
}

impl Loaded {
    /// Canonical JSON text, used to compare configs semantically.
    pub fn normalized(&self) -> String {
        serde_json::to_string(&self.json).expect("JSON value serializes")
    }
}

/// Read and load a config file.
pub fn load_file(path: &Path) -> Result<Loaded, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    load_str(&path.display().to_string(), &text)
}

/// Load config text; `source_name` prefixes every diagnostic.
pub fn load_str(source_name: &str, text: &str) -> Result<Loaded, ConfigError> {
    // 1. Parse, keeping spans.
    let doc = match Document::parse(text.to_string()) {
        Ok(doc) => doc,
        Err(err) => {
            let location = err.span().map(|s| line_col(text, s.start));
            let message = err.message().trim().to_string();
            let report = Report {
                source_name: source_name.to_string(),
                lines: vec![RenderedDiagnostic {
                    severity: Severity::Error,
                    location,
                    path: String::new(),
                    message: format!("TOML syntax error: {message}"),
                }],
            };
            return Err(ConfigError::Invalid(report));
        }
    };
    let json = to_json(&doc);
    let fail =
        |diagnostics| ConfigError::Invalid(Report::new(source_name, text, Some(&doc), diagnostics));

    // A clear message for configs written for another major version.
    if let Some(version) = json.get("version")
        && version.as_u64() != Some(CONFIG_VERSION)
    {
        return Err(fail(vec![Diagnostic::error(
            "/version",
            format!(
                "config version {version} is not supported by tidepool {}; it reads version {CONFIG_VERSION}",
                env!("CARGO_PKG_VERSION")
            ),
        )]));
    }

    // 2–3. Validate against the schema, collecting every error.
    let diagnostics = schema_diagnostics(&json);
    if !diagnostics.is_empty() {
        return Err(fail(diagnostics));
    }

    // 4. Deserialize; after validation this cannot fail on shape.
    let config: Config = serde_json::from_value(json.clone())
        .map_err(|e| fail(vec![Diagnostic::error("", e.to_string())]))?;

    // 5. Semantic checks.
    let diagnostics = semantic::check(&config);
    let report = Report::new(source_name, text, Some(&doc), diagnostics);
    if report.has_errors() {
        return Err(ConfigError::Invalid(report));
    }
    let warnings = report.warnings().collect();
    Ok(Loaded {
        config,
        json,
        warnings,
    })
}

/// Every schema violation, as diagnostics with human-oriented messages.
pub fn schema_diagnostics(json: &Value) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for error in validator().iter_errors(json) {
        convert(&error, &mut out);
    }
    out
}

fn child(pointer: &str, key: &str) -> String {
    format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn convert(error: &ValidationError<'_>, out: &mut Vec<Diagnostic>) {
    let pointer = error.instance_path().as_str().to_string();
    let instance = error.instance();
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            for key in unexpected {
                out.push(
                    Diagnostic::error(child(&pointer, key), format!("unknown key {key:?}"))
                        .at_key(),
                );
            }
        }
        ValidationErrorKind::PropertyNames { error: inner } => {
            let name = inner.instance().as_str().unwrap_or_default().to_string();
            out.push(
                Diagnostic::error(
                    child(&pointer, &name),
                    format!("invalid name {name:?}: must match {KEY_PATTERN} (lowercase letters, digits, '-' and '_')"),
                )
                .at_key(),
            );
        }
        ValidationErrorKind::Format { format } => {
            let value = instance.as_str().unwrap_or_default();
            let message = check_format(format, value)
                .err()
                .unwrap_or_else(|| format!("{value:?} is not a valid {format}"));
            out.push(Diagnostic::error(pointer, message));
        }
        ValidationErrorKind::Pattern { .. } if pointer.ends_with("/category") => {
            out.push(Diagnostic::error(
                pointer,
                format!(
                    "{instance} isn't a category name: use lowercase letters, digits, '-' and '_'"
                ),
            ));
        }
        ValidationErrorKind::Enum { options } => {
            out.push(Diagnostic::error(
                pointer,
                not_one_of(instance, options.as_array().into_iter().flatten()),
            ));
        }
        ValidationErrorKind::OneOfNotValid { context }
            if !context.is_empty()
                && context.iter().all(|b| {
                    b.iter()
                        .any(|e| matches!(e.kind(), ValidationErrorKind::Constant { .. }))
                }) =>
        {
            // A documented enum: one `const` per variant.
            let options: Vec<Value> = context
                .iter()
                .flatten()
                .filter_map(|e| match e.kind() {
                    ValidationErrorKind::Constant { expected_value } => {
                        Some((**expected_value).clone())
                    }
                    _ => None,
                })
                .collect();
            out.push(Diagnostic::error(
                pointer,
                not_one_of(instance, options.iter()),
            ));
        }
        ValidationErrorKind::MinLength { limit: 1 } => {
            out.push(Diagnostic::error(
                pointer,
                "must not be empty (remove it to use the default)",
            ));
        }
        ValidationErrorKind::Required { property } => {
            let name = property.as_str().unwrap_or_default();
            out.push(Diagnostic::error(
                pointer,
                format!("missing required key {name:?}"),
            ));
        }
        ValidationErrorKind::OneOfNotValid { context }
        | ValidationErrorKind::OneOfMultipleValid { context }
        | ValidationErrorKind::AnyOf { context } => {
            // Pick the branch that matches the value's type (e.g. a rule given
            // as a string vs. as a table) and report its errors.
            let relevant: Vec<_> = context
                .iter()
                .filter(|branch| {
                    !branch.iter().any(|e| {
                        matches!(e.kind(), ValidationErrorKind::Type { .. })
                            && e.instance_path().as_str() == pointer
                    })
                })
                .collect();
            if let [branch] = relevant.as_slice() {
                for e in branch.iter() {
                    convert(e, out);
                }
            } else if pointer_segments(&pointer)
                .iter()
                .any(|s| s == "include" || s == "exclude")
            {
                out.push(Diagnostic::error(
                    pointer,
                    "must be a regex string or a { field, pattern } table",
                ));
            } else {
                out.push(Diagnostic::error(pointer, error.to_string()));
            }
        }
        _ => out.push(Diagnostic::error(pointer, error.to_string())),
    }
}

fn not_one_of<'a>(instance: &Value, options: impl Iterator<Item = &'a Value>) -> String {
    let options: Vec<String> = options.map(Value::to_string).collect();
    format!("{instance} is not one of {}", options.join(", "))
}
