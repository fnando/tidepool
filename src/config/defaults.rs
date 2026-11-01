//! `tidepool config --include-defaults`: the config as written, with every
//! setting it leaves out filled in with its default, where that default
//! lives: `[general]`, `[tools.*]`, `[schedule]`, `[server]`,
//! `[category.default]` (and its `youtube`, `embed` and `cleanup` tables)
//! and each hook. Other categories and sources only list what they set, as
//! they inherit the rest.

use toml_edit::{DocumentMut, Item, Table};

use super::Config;
use super::merge::DEFAULT_CATEGORY as DEFAULT_CATEGORY_NAME;
use super::merge::category_settings;

/// `text` (a config) with the defaults it leaves out. Comments, order and
/// what's set stay as they are; defaults go at the end of their table.
pub fn with_defaults(
    text: &str,
    config: &Config,
    default_media_dir: &str,
) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let defaults = defaults(config, default_media_dir)?;
    fill(doc.as_table_mut(), defaults.as_table());
    Ok(doc.to_string())
}

/// Every default, as a config would set it.
fn defaults(config: &Config, default_media_dir: &str) -> Result<DocumentMut, String> {
    let empty = Config::default();
    let value = |v: Result<toml::Value, toml::ser::Error>| v.map_err(|e| e.to_string());
    let mut table = toml::Table::new();
    table.insert(
        "general".into(),
        value(toml::Value::try_from(&empty.general))?,
    );
    table.insert("tools".into(), value(toml::Value::try_from(&empty.tools))?);
    table.insert(
        "schedule".into(),
        value(toml::Value::try_from(&empty.schedule))?,
    );
    table.insert(
        "server".into(),
        value(toml::Value::try_from(&empty.server))?,
    );

    let mut category = match value(toml::Value::try_from(category_settings(
        &empty,
        DEFAULT_CATEGORY_NAME,
    )))? {
        toml::Value::Table(t) => t,
        _ => return Err("the default category isn't a table".into()),
    };
    // Settings of a source, not of a category.
    for key in ["urls", "category", "enabled"] {
        category.remove(key);
    }
    category.insert(
        "media_dir".into(),
        toml::Value::String(default_media_dir.into()),
    );
    let mut categories = toml::Table::new();
    categories.insert(DEFAULT_CATEGORY_NAME.into(), toml::Value::Table(category));
    table.insert("category".into(), toml::Value::Table(categories));

    // Each hook's own defaults (`type`, `timeout`, …), as loaded.
    let mut hooks = toml::Table::new();
    for (name, hook) in &config.hooks {
        hooks.insert(name.clone(), value(toml::Value::try_from(hook))?);
    }
    if !hooks.is_empty() {
        table.insert("hooks".into(), toml::Value::Table(hooks));
    }

    let text = toml::to_string(&table).map_err(|e| e.to_string())?;
    text.parse().map_err(|e| format!("{e}"))
}

/// Add to `table` what `defaults` has and it doesn't, table by table.
fn fill(table: &mut Table, defaults: &Table) {
    for (key, default) in defaults.iter() {
        match (table.get_mut(key), default) {
            (None, Item::Table(sub)) => {
                let mut sub = sub.clone();
                sub.set_implicit(false);
                table.insert(key, Item::Table(sub));
            }
            (None, item) => {
                table.insert(key, item.clone());
            }
            (Some(Item::Table(own)), Item::Table(sub)) => fill(own, sub),
            // Set already (or written another way, e.g. as an inline table).
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# mine\n[category.default]\nmedia_dir = \"~/Videos\" # kept\n\n\
                        [category.default.cleanup]\nmax_items = 50\n\n\
                        [category.music]\nformat = \"ba\"\n\n\
                        [hooks.notify]\ncommand = [\"ntfy\"]\n\n\
                        [sources.lofi]\nurls = [\"https://www.youtube.com/playlist?list=PLx\"]\n";

    fn filled() -> String {
        let config: Config = toml::from_str(FILE).unwrap();
        with_defaults(FILE, &config, "/home/me/media").unwrap()
    }

    #[test]
    fn keeps_what_is_set_and_adds_the_rest() {
        let out = filled();
        assert!(
            out.starts_with("# mine\n[category.default]\nmedia_dir = \"~/Videos\" # kept\n"),
            "{out}"
        );
        assert!(out.contains("max_items = 50"), "set values win");
        assert!(out.contains("trash_dir = \"{data_dir}/trash\""), "{out}");
        assert!(out.contains("trash_for = \"1w\""));
        assert!(out.contains("[general]") && out.contains("[tools.yt-dlp]"));
        assert!(out.contains("timeout = \"10m\""), "hook defaults: {out}");
        let music = out.split("[category.music]").nth(1).unwrap();
        let music = music.split("\n[").next().unwrap();
        assert_eq!(music.trim(), "format = \"ba\"", "other categories inherit");
        let lofi = out.split("[sources.lofi]").nth(1).unwrap();
        assert!(!lofi.contains("trash_dir"), "sources inherit");
    }

    #[test]
    fn the_result_is_a_valid_config_with_the_same_meaning() {
        let out = filled();
        let loaded =
            crate::config::load_str("filled", &out).unwrap_or_else(|e| panic!("{e}\n{out}"));
        let before: Config = toml::from_str(FILE).unwrap();
        assert_eq!(
            crate::config::merge::effective_sources(&loaded.config),
            crate::config::merge::effective_sources(&before),
        );
    }
}
