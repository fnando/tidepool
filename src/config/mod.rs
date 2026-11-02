//! Configuration: types, parsing, validation and merging.

pub mod duration;
pub mod placeholders;
pub mod secret;
pub mod size;

/// Source, category and hook names: `^[a-z0-9][a-z0-9_-]*$`.
pub const KEY_PATTERN: &str = "^[a-z0-9][a-z0-9_-]*$";

/// Whether `s` is a valid source, category or hook name.
#[must_use]
pub fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
pub mod schema;
pub mod types;

pub use types::*;
pub mod load;
pub mod merge;
pub mod semantic;
pub mod spans;

pub use load::{ConfigError, Loaded, load_file, load_str};
pub mod check;
pub mod defaults;
pub mod diff;
pub mod store;
