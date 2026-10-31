//! TOML ↔ JSON plumbing: convert a span-preserving `toml_edit` document to
//! `serde_json::Value`, and map JSON pointers back to TOML line:column.

use std::ops::Range;

use serde_json::{Map, Number, Value};
use toml_edit::{Document, Item, TableLike};

/// Convert a parsed TOML document to JSON. Datetimes become strings.
pub fn to_json(doc: &Document<String>) -> Value {
    table_to_json(doc.as_table())
}

fn table_to_json(table: &dyn TableLike) -> Value {
    let mut map = Map::new();
    for (key, item) in table.iter() {
        if let Some(value) = item_to_json(item) {
            map.insert(key.to_string(), value);
        }
    }
    Value::Object(map)
}

fn item_to_json(item: &Item) -> Option<Value> {
    Some(match item {
        Item::None => return None,
        Item::Value(value) => value_to_json(value),
        Item::Table(table) => table_to_json(table),
        Item::ArrayOfTables(tables) => {
            Value::Array(tables.iter().map(|t| table_to_json(t)).collect())
        }
    })
}

fn value_to_json(value: &toml_edit::Value) -> Value {
    use toml_edit::Value as V;
    match value {
        V::String(s) => Value::String(s.value().clone()),
        V::Integer(i) => Value::Number((*i.value()).into()),
        V::Float(f) => Number::from_f64(*f.value())
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(f.value().to_string())),
        V::Boolean(b) => Value::Bool(*b.value()),
        V::Datetime(d) => Value::String(d.value().to_string()),
        V::Array(items) => Value::Array(items.iter().map(value_to_json).collect()),
        V::InlineTable(table) => table_to_json(table),
    }
}

/// Split a JSON pointer into unescaped segments.
pub fn pointer_segments(pointer: &str) -> Vec<String> {
    if pointer.is_empty() {
        return Vec::new();
    }
    pointer
        .trim_start_matches('/')
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Render a JSON pointer as a dotted config path: `sources.lofi.include[0]`.
pub fn dotted(pointer: &str) -> String {
    let mut out = String::new();
    for segment in pointer_segments(pointer) {
        if segment.chars().all(|c| c.is_ascii_digit()) && !segment.is_empty() {
            out.push_str(&format!("[{segment}]"));
            continue;
        }
        if !out.is_empty() {
            out.push('.');
        }
        let bare = !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if bare {
            out.push_str(&segment);
        } else {
            out.push_str(&format!("{segment:?}"));
        }
    }
    out
}

enum Node<'a> {
    Item(&'a Item),
    Value(&'a toml_edit::Value),
    Table(&'a toml_edit::Table),
}

impl<'a> Node<'a> {
    fn span(&self) -> Option<Range<usize>> {
        match self {
            Node::Item(item) => item.span(),
            Node::Value(value) => value.span(),
            Node::Table(table) => table.span(),
        }
    }

    fn table_like(&self) -> Option<&'a dyn TableLike> {
        match self {
            Node::Item(item) => item.as_table_like(),
            Node::Value(value) => value.as_inline_table().map(|t| t as &dyn TableLike),
            Node::Table(table) => Some(*table as &dyn TableLike),
        }
    }

    fn index(&self, idx: usize) -> Option<Node<'a>> {
        match self {
            Node::Item(Item::ArrayOfTables(tables)) => tables.get(idx).map(Node::Table),
            Node::Item(item) => item.as_array().and_then(|a| a.get(idx)).map(Node::Value),
            Node::Value(value) => value.as_array().and_then(|a| a.get(idx)).map(Node::Value),
            Node::Table(_) => None,
        }
    }
}

/// Byte span for a JSON pointer: the value's span, or with `prefer_key`, the
/// key's. Falls back to the closest ancestor that has a span.
pub fn locate(doc: &Document<String>, pointer: &str, prefer_key: bool) -> Option<Range<usize>> {
    let segments = pointer_segments(pointer);
    let mut node = Node::Item(doc.as_item());
    let mut best = None;
    for (i, segment) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        let next = if let Some(table) = node.table_like() {
            let Some((key, item)) = table.get_key_value(segment) else {
                break;
            };
            if let Some(span) = key.span() {
                best = Some(span.clone());
                if last && prefer_key {
                    return Some(span);
                }
            }
            Node::Item(item)
        } else if let Ok(idx) = segment.parse::<usize>() {
            match node.index(idx) {
                Some(next) => next,
                None => break,
            }
        } else {
            break;
        };
        if let Some(span) = next.span() {
            best = Some(span);
        }
        node = next;
    }
    best
}

/// 1-based line and column (in characters) of a byte offset.
pub fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before[line_start..].chars().count() + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SRC: &str = r#"
[library]
concurrency = 2

[sources.lofi]
url = "https://www.youtube.com/playlist?list=PLx"
include = ['a', { field = "channel", pattern = 'b' }]

[sources.lofi.cleanup]
max_size = "20 gigs"
"#;

    fn doc() -> Document<String> {
        Document::parse(SRC.to_string()).unwrap()
    }

    #[test]
    fn converts_to_json() {
        assert_eq!(
            to_json(&doc()),
            json!({
                "library": { "concurrency": 2 },
                "sources": { "lofi": {
                    "url": "https://www.youtube.com/playlist?list=PLx",
                    "include": ["a", { "field": "channel", "pattern": "b" }],
                    "cleanup": { "max_size": "20 gigs" }
                }}
            })
        );
    }

    #[test]
    fn converts_datetimes_to_strings() {
        let doc = Document::parse("d = 2024-01-01".to_string()).unwrap();
        assert_eq!(to_json(&doc), json!({ "d": "2024-01-01" }));
    }

    #[test]
    fn locates_values_and_keys() {
        let doc = doc();
        let at = |p, key| locate(&doc, p, key).map(|r| line_col(SRC, r.start));
        assert_eq!(at("/library/concurrency", false), Some((3, 15)));
        assert_eq!(at("/library/concurrency", true), Some((3, 1)));
        assert_eq!(at("/sources/lofi/cleanup/max_size", false), Some((10, 12)));
        assert_eq!(at("/sources/lofi/include/1/pattern", false), Some((7, 48)));
        assert_eq!(at("/sources/lofi/include/0", false), Some((7, 12)));
        // Unknown leaf: falls back to the closest ancestor with a span.
        assert!(at("/sources/lofi/nope", false).is_some());
    }

    #[test]
    fn renders_dotted_paths() {
        assert_eq!(dotted("/sources/lofi/include/0"), "sources.lofi.include[0]");
        assert_eq!(dotted("/sources/Bad Key"), "sources.\"Bad Key\"");
        assert_eq!(dotted(""), "");
    }

    #[test]
    fn computes_line_and_column() {
        assert_eq!(line_col("ab\ncd", 0), (1, 1));
        assert_eq!(line_col("ab\ncd", 4), (2, 2));
        assert_eq!(line_col("é\nx", 3), (2, 1));
    }
}
