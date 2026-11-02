//! Secrets (`api_key`, `server.token`) can name an environment variable
//! instead of holding the value: `"$NAME"` or `"${NAME}"`. The variable is
//! read when the config is used, so the stored config (and the web UI)
//! only ever has the reference.

/// The variable a secret refers to, when the whole value is `$NAME` or
/// `${NAME}` (letters, digits and `_`, not starting with a digit).
#[must_use]
pub fn env_name(value: &str) -> Option<&str> {
    let name = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(name)
}

/// A secret's value: the variable's when it names one (`None` if that's
/// unset or empty), else the value itself (`None` if empty).
#[must_use]
pub fn resolve(value: &str) -> Option<String> {
    resolve_with(value, |name| std::env::var(name).ok())
}

/// [`resolve`] with an injectable variable lookup.
#[must_use]
pub fn resolve_with(value: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    match env_name(value) {
        Some(name) => var(name).filter(|v| !v.is_empty()),
        None => (!value.is_empty()).then(|| value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_variable_only_when_that_is_the_whole_value() {
        assert_eq!(env_name("$YOUTUBE_API_KEY"), Some("YOUTUBE_API_KEY"));
        assert_eq!(env_name("${YOUTUBE_API_KEY}"), Some("YOUTUBE_API_KEY"));
        assert_eq!(env_name("$_key2"), Some("_key2"));
        for literal in [
            "AIzaSyD-123",
            "$",
            "${}",
            "${KEY",
            "$2KEY",
            "$KEY-1",
            "key$KEY",
            "",
        ] {
            assert_eq!(env_name(literal), None, "{literal:?}");
        }
    }

    #[test]
    fn resolves_from_the_environment() {
        let var = |name: &str| match name {
            "KEY" => Some("AIza-from-env".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        assert_eq!(resolve_with("$KEY", var).as_deref(), Some("AIza-from-env"));
        assert_eq!(
            resolve_with("${KEY}", var).as_deref(),
            Some("AIza-from-env")
        );
        assert_eq!(resolve_with("$UNSET", var), None);
        assert_eq!(resolve_with("$EMPTY", var), None);
        assert_eq!(
            resolve_with("AIza-literal", var).as_deref(),
            Some("AIza-literal")
        );
        assert_eq!(resolve_with("", var), None);
    }
}
