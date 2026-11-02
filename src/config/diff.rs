//! Comparing configs (key-level summary + text diff) and masking secrets for
//! display.

use serde_json::Value;
use similar::TextDiff;
use toml_edit::DocumentMut;

/// Mask an API key: `AIzaSyAbc…x1Qw` → `AIza…x1Qw`.
#[must_use]
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Whether a value at `path` (table keys from the root) is a secret: any
/// `api_key` (in a category, a source or anywhere else) and `server.token`.
fn is_secret(path: &[&str]) -> bool {
    matches!(path, [.., "api_key"] | ["server", "token"])
}

/// Mask a secret for display; empty values stay empty.
#[must_use]
pub fn mask_secret(value: &str) -> String {
    if value.is_empty() || super::secret::env_name(value).is_some() {
        // Empty, or naming an environment variable: nothing to hide.
        value.to_string()
    } else {
        mask_key(value)
    }
}

/// Mask secrets in TOML text, keeping comments and formatting. Text that
/// doesn't parse is returned unchanged.
#[must_use]
pub fn mask_toml(text: &str) -> String {
    fn walk(table: &mut dyn toml_edit::TableLike, path: &mut Vec<String>) {
        for (key, item) in table.iter_mut() {
            path.push(key.get().to_string());
            let keys: Vec<&str> = path.iter().map(String::as_str).collect();
            if is_secret(&keys)
                && let Some(value) = item.as_value_mut()
                && let Some(secret) = value.as_str()
            {
                let decor = value.decor().clone();
                *value = mask_secret(secret).into();
                *value.decor_mut() = decor;
            } else if let Some(inner) = item.as_table_like_mut() {
                walk(inner, path);
            }
            path.pop();
        }
    }
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return text.to_string();
    };
    walk(doc.as_table_mut(), &mut Vec::new());
    doc.to_string()
}

/// Undo [`mask_toml`] on edited text: a secret still showing the mask of
/// the one in `original` (same place) gets its real value back. Text that
/// doesn't parse is returned unchanged (validation reports it).
#[must_use]
pub fn unmask_toml(edited: &str, original: &str) -> String {
    fn secrets(
        table: &dyn toml_edit::TableLike,
        path: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, String)>,
    ) {
        for (key, item) in table.iter() {
            path.push(key.to_string());
            let keys: Vec<&str> = path.iter().map(String::as_str).collect();
            if is_secret(&keys)
                && let Some(secret) = item.as_value().and_then(|v| v.as_str())
            {
                out.push((path.clone(), secret.to_string()));
            } else if let Some(inner) = item.as_table_like() {
                secrets(inner, path, out);
            }
            path.pop();
        }
    }
    fn restore(
        table: &mut dyn toml_edit::TableLike,
        path: &mut Vec<String>,
        real: &[(Vec<String>, String)],
    ) {
        for (key, item) in table.iter_mut() {
            path.push(key.get().to_string());
            let keys: Vec<&str> = path.iter().map(String::as_str).collect();
            if is_secret(&keys)
                && let Some(value) = item.as_value_mut()
                && let Some(shown) = value.as_str()
                && let Some((_, secret)) = real.iter().find(|(p, _)| p == path)
                && !secret.is_empty()
                && shown == mask_secret(secret)
            {
                let decor = value.decor().clone();
                *value = secret.clone().into();
                *value.decor_mut() = decor;
            } else if let Some(inner) = item.as_table_like_mut() {
                restore(inner, path, real);
            }
            path.pop();
        }
    }
    let (Ok(mut doc), Ok(before)) = (
        edited.parse::<DocumentMut>(),
        original.parse::<DocumentMut>(),
    ) else {
        return edited.to_string();
    };
    let mut real = Vec::new();
    secrets(before.as_table(), &mut Vec::new(), &mut real);
    restore(doc.as_table_mut(), &mut Vec::new(), &real);
    doc.to_string()
}

/// Mask secrets in a JSON config.
pub fn mask_json(value: &mut Value) {
    fn walk(value: &mut Value, path: &mut Vec<String>) {
        let Value::Object(map) = value else { return };
        for (key, inner) in map.iter_mut() {
            path.push(key.clone());
            let keys: Vec<&str> = path.iter().map(String::as_str).collect();
            match inner {
                Value::String(s) if is_secret(&keys) => *s = mask_secret(s),
                _ => walk(inner, path),
            }
            path.pop();
        }
    }
    walk(value, &mut Vec::new());
}

/// One key-level difference.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Added {
        path: String,
        value: Value,
    },
    Removed {
        path: String,
        value: Value,
    },
    Changed {
        path: String,
        old: Value,
        new: Value,
    },
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Added { path, value } => write!(f, "+ {path} = {value}"),
            Change::Removed { path, value } => write!(f, "- {path} = {value}"),
            Change::Changed { path, old, new } => write!(f, "~ {path}: {old} → {new}"),
        }
    }
}

/// Differences between two configs, by key. Tables are compared key by key;
/// arrays (e.g. `include`, `hooks`) are compared as whole values.
#[must_use]
pub fn key_changes(old: &Value, new: &Value) -> Vec<Change> {
    let mut out = Vec::new();
    walk("", old, new, &mut out);
    out
}

fn join(prefix: &str, key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let key = if bare {
        key.to_string()
    } else {
        format!("{key:?}")
    };
    if prefix.is_empty() {
        key
    } else {
        format!("{prefix}.{key}")
    }
}

fn walk(path: &str, old: &Value, new: &Value, out: &mut Vec<Change>) {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = join(path, key);
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => walk(&child, x, y, out),
                    (Some(x), None) => out.push(Change::Removed {
                        path: child,
                        value: x.clone(),
                    }),
                    (None, Some(y)) => out.push(Change::Added {
                        path: child,
                        value: y.clone(),
                    }),
                    (None, None) => {}
                }
            }
        }
        _ if old != new => out.push(Change::Changed {
            path: path.to_string(),
            old: old.clone(),
            new: new.clone(),
        }),
        _ => {}
    }
}

/// A unified diff of two TOML texts.
#[must_use]
pub fn text_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masked_secrets_get_their_real_value_back() {
        let original = "[server]\ntoken = \"my-long-secret-token\"\n\n\
                        [sources.lofi]\nurls = [\"u\"]\nyoutube.api_key = \"AIzaSyLOFI00000000yyyy\"\n";
        let edited = mask_toml(original).replace("urls = [\"u\"]", "urls = [\"v\"] # changed");
        assert_eq!(
            unmask_toml(&edited, original),
            original.replace("urls = [\"u\"]", "urls = [\"v\"] # changed")
        );
        // A secret typed in full, or one moved elsewhere, stays as written.
        let typed = mask_toml(original).replace("my-l…oken", "brand-new-token");
        assert!(unmask_toml(&typed, original).contains("brand-new-token"));
        let moved = "[sources.other]\nyoutube.api_key = \"AIza…yyyy\"\n";
        assert_eq!(unmask_toml(moved, original), moved);
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask_key("AIzaSyD-1234567890x1Qw"), "AIza…x1Qw");
        assert_eq!(mask_key("short"), "…");
        let text = "[category.default.youtube]\napi_key = \"AIzaSyD-1234567890x1Qw\" # key\n\n\
                    [category.music]\nyoutube = { api_key = \"AIzaSyMUSIC0000000zzzz\" }\n\n\
                    [sources.lofi]\nurls = [\"u\"]\nyoutube.api_key = \"AIzaSyLOFI00000000yyyy\"\n\n\
                    [server]\ntoken = \"my-long-secret-token\"\n";
        assert_eq!(
            mask_toml(text),
            "[category.default.youtube]\napi_key = \"AIza…x1Qw\" # key\n\n\
             [category.music]\nyoutube = { api_key = \"AIza…zzzz\" }\n\n\
             [sources.lofi]\nurls = [\"u\"]\nyoutube.api_key = \"AIza…yyyy\"\n\n\
             [server]\ntoken = \"my-l…oken\"\n"
        );
        // A variable's name isn't a secret.
        assert_eq!(
            mask_toml("[server]\ntoken = \"$TIDEPOOL_TOKEN\"\n"),
            "[server]\ntoken = \"$TIDEPOOL_TOKEN\"\n"
        );
        assert_eq!(
            mask_toml("[server]\ntoken = \"\"\n"),
            "[server]\ntoken = \"\"\n"
        );
        assert_eq!(
            mask_toml("[sources.x]\ntoken = \"not a secret\"\n"),
            "[sources.x]\ntoken = \"not a secret\"\n"
        );

        let mut value = json!({
            "category": { "default": { "youtube": { "api_key": "AIzaSyD-1234567890x1Qw" } } },
            "server": { "token": "my-long-secret-token" }
        });
        mask_json(&mut value);
        assert_eq!(
            value,
            json!({
                "category": { "default": { "youtube": { "api_key": "AIza…x1Qw" } } },
                "server": { "token": "my-l…oken" }
            })
        );
    }

    #[test]
    fn summarizes_key_changes() {
        let old = json!({ "sources": { "lofi": { "cleanup": { "max_age": "30d" }, "include": ["a"] }, "old": { "id": "PL1" } } });
        let new = json!({ "sources": { "lofi": { "cleanup": { "max_age": "14d" }, "include": ["a", "b"] }, "new pl": { "id": "PL2" } } });
        let lines: Vec<String> = key_changes(&old, &new)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            lines,
            vec![
                r#"~ sources.lofi.cleanup.max_age: "30d" → "14d""#,
                r#"~ sources.lofi.include: ["a"] → ["a","b"]"#,
                r#"+ sources."new pl" = {"id":"PL2"}"#,
                r#"- sources.old = {"id":"PL1"}"#,
            ]
        );
        assert!(key_changes(&old, &old).is_empty());
    }

    #[test]
    fn renders_unified_diff() {
        let diff = text_diff("a = 1\nb = 2\n", "a = 1\nb = 3\n", "stored", "file");
        assert!(diff.starts_with("--- stored\n+++ file\n"), "{diff}");
        assert!(diff.contains("-b = 2\n+b = 3\n"), "{diff}");
    }
}
