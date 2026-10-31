//! Comparing configs (key-level summary + text diff) and masking secrets for
//! display.

use serde_json::Value;
use similar::TextDiff;
use toml_edit::DocumentMut;

/// Mask an API key: `AIzaSyAbc…x1Qw` → `AIza…x1Qw`.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Config values that are secrets: masked unless `--show-secrets`.
pub const SECRETS: &[(&str, &str)] = &[("youtube", "api_key"), ("server", "token")];

/// Mask a secret for display; empty values stay empty.
pub fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        mask_key(value)
    }
}

/// Mask [`SECRETS`] in TOML text, keeping comments and formatting. Text that
/// doesn't parse is returned unchanged.
pub fn mask_toml(text: &str) -> String {
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return text.to_string();
    };
    for (table, key) in SECRETS {
        let Some(value) = doc
            .get_mut(table)
            .and_then(|t| t.as_table_like_mut())
            .and_then(|t| t.get_mut(key))
            .and_then(|v| v.as_value_mut())
        else {
            continue;
        };
        if let Some(secret) = value.as_str() {
            let decor = value.decor().clone();
            *value = mask_secret(secret).into();
            *value.decor_mut() = decor;
        }
    }
    doc.to_string()
}

/// Mask [`SECRETS`] in a JSON config.
pub fn mask_json(value: &mut Value) {
    for (table, key) in SECRETS {
        if let Some(Value::String(s)) = value.pointer_mut(&format!("/{table}/{key}")) {
            *s = mask_secret(s);
        }
    }
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
    fn masks_keys() {
        assert_eq!(mask_key("AIzaSyD-1234567890x1Qw"), "AIza…x1Qw");
        assert_eq!(mask_key("short"), "…");
        let text = "[youtube]\napi_key = \"AIzaSyD-1234567890x1Qw\" # key\n\n\
                    [server]\ntoken = \"my-long-secret-token\"\n";
        assert_eq!(
            mask_toml(text),
            "[youtube]\napi_key = \"AIza…x1Qw\" # key\n\n[server]\ntoken = \"my-l…oken\"\n"
        );
        assert_eq!(
            mask_toml("[server]\ntoken = \"\"\n"),
            "[server]\ntoken = \"\"\n"
        );
        assert_eq!(mask_toml("[library]\n"), "[library]\n");

        let mut value = json!({
            "youtube": { "api_key": "AIzaSyD-1234567890x1Qw" },
            "server": { "token": "my-long-secret-token" }
        });
        mask_json(&mut value);
        assert_eq!(
            value,
            json!({ "youtube": { "api_key": "AIza…x1Qw" }, "server": { "token": "my-l…oken" } })
        );
    }

    #[test]
    fn summarizes_key_changes() {
        let old = json!({ "playlists": { "lofi": { "cleanup": { "max_age": "30d" }, "include": ["a"] }, "old": { "id": "PL1" } } });
        let new = json!({ "playlists": { "lofi": { "cleanup": { "max_age": "14d" }, "include": ["a", "b"] }, "new pl": { "id": "PL2" } } });
        let lines: Vec<String> = key_changes(&old, &new)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            lines,
            vec![
                r#"~ playlists.lofi.cleanup.max_age: "30d" → "14d""#,
                r#"~ playlists.lofi.include: ["a"] → ["a","b"]"#,
                r#"+ playlists."new pl" = {"id":"PL2"}"#,
                r#"- playlists.old = {"id":"PL1"}"#,
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
