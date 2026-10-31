//! Include/exclude rules, compiled once per playlist.
//!
//! An item is kept when it matches at least one include rule (or the include
//! list is empty) and no exclude rule. Exclude always wins.

use std::fmt;

use fancy_regex::{Regex, RegexBuilder};

use crate::config::merge::EffectivePlaylist;
use crate::config::{Rule as RuleConfig, RuleField};

/// The fields of an item rules can look at. `None` = not known yet (e.g. the
/// yt-dlp indexer returns no description).
#[derive(Debug, Clone, Default)]
pub struct Fields<'a> {
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub channel: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub id: Option<&'a str>,
}

impl Fields<'_> {
    fn values(&self, field: RuleField) -> Option<Vec<&str>> {
        match field {
            RuleField::Title => self.title.map(|v| vec![v]),
            RuleField::Description => self.description.map(|v| vec![v]),
            RuleField::Channel => self.channel.map(|v| vec![v]),
            RuleField::Id => self.id.map(|v| vec![v]),
            RuleField::Tags => self
                .tags
                .map(|tags| tags.iter().map(String::as_str).collect()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Include,
    Exclude,
}

impl fmt::Display for List {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            List::Include => "include",
            List::Exclude => "exclude",
        })
    }
}

/// One compiled rule.
#[derive(Debug, Clone)]
pub struct Rule {
    pub list: List,
    pub index: usize,
    pub fields: Vec<RuleField>,
    pub source: String,
    re: Regex,
}

/// The result of testing one rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    Yes,
    No,
    /// A field the rule needs isn't known yet.
    Unknown(RuleField),
    /// The backtrack limit was hit.
    Error(String),
}

impl Rule {
    pub fn test(&self, item: &Fields<'_>) -> Match {
        let mut missing = None;
        for &field in &self.fields {
            let Some(values) = item.values(field) else {
                missing.get_or_insert(field);
                continue;
            };
            for value in values {
                match self.re.is_match(value) {
                    Ok(true) => return Match::Yes,
                    Ok(false) => {}
                    Err(e) => return Match::Error(e.to_string()),
                }
            }
        }
        missing.map_or(Match::No, Match::Unknown)
    }
}

/// Why an item is kept or dropped. Stored in the DB and shown by
/// `tidepool list` and `tidepool filter test`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Kept; `include` is the matching include rule, if any.
    Keep {
        include: Option<usize>,
    },
    Excluded {
        rule: usize,
    },
    NotIncluded,
    /// Needs a field the indexer didn't provide; check again before download.
    Deferred(RuleField),
    /// A rule hit the regex backtrack limit; held until the rule is fixed.
    FilterError {
        list: List,
        rule: usize,
        message: String,
    },
}

/// A playlist's compiled include/exclude rules.
#[derive(Debug, Clone)]
pub struct RuleSet {
    pub include: Vec<Rule>,
    pub exclude: Vec<Rule>,
}

impl RuleSet {
    /// Compile a playlist's rules. Patterns were validated at config load.
    pub fn compile(playlist: &EffectivePlaylist, backtrack_limit: usize) -> Result<Self, String> {
        let compile = |list, rules: &[RuleConfig]| {
            rules
                .iter()
                .enumerate()
                .map(|(index, rule)| {
                    let (fields, source) = match rule {
                        RuleConfig::Pattern(p) => (playlist.match_fields.clone(), p.clone()),
                        RuleConfig::Field(f) => (vec![f.field], f.pattern.clone()),
                    };
                    let re = RegexBuilder::new(&source)
                        .backtrack_limit(backtrack_limit)
                        .build()
                        .map_err(|e| format!("playlists.{}.{list}[{index}]: {e}", playlist.key))?;
                    Ok(Rule {
                        list,
                        index,
                        fields,
                        source,
                        re,
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        };
        Ok(Self {
            include: compile(List::Include, &playlist.include)?,
            exclude: compile(List::Exclude, &playlist.exclude)?,
        })
    }

    pub fn decide(&self, item: &Fields<'_>) -> Decision {
        let mut deferred = None;
        for rule in &self.exclude {
            match rule.test(item) {
                Match::Yes => return Decision::Excluded { rule: rule.index },
                Match::No => {}
                Match::Unknown(field) => {
                    deferred.get_or_insert(field);
                }
                Match::Error(message) => {
                    return Decision::FilterError {
                        list: List::Exclude,
                        rule: rule.index,
                        message,
                    };
                }
            }
        }
        // An unknown field in an exclude rule could still drop the item.
        if let Some(field) = deferred {
            return Decision::Deferred(field);
        }
        if self.include.is_empty() {
            return Decision::Keep { include: None };
        }
        for rule in &self.include {
            match rule.test(item) {
                Match::Yes => {
                    return Decision::Keep {
                        include: Some(rule.index),
                    };
                }
                Match::No => {}
                Match::Unknown(field) => {
                    deferred.get_or_insert(field);
                }
                Match::Error(message) => {
                    return Decision::FilterError {
                        list: List::Include,
                        rule: rule.index,
                        message,
                    };
                }
            }
        }
        deferred.map_or(Decision::NotIncluded, Decision::Deferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_playlists;
    use proptest::prelude::*;

    fn rules(toml: &str) -> RuleSet {
        let config: crate::config::Config = toml::from_str(&format!(
            "[playlists.x]\nsource = 'https://www.youtube.com/playlist?list=PLx'\n{toml}"
        ))
        .unwrap();
        RuleSet::compile(&effective_playlists(&config)[0], 1_000_000).unwrap()
    }

    fn title(t: &str) -> Fields<'_> {
        Fields {
            title: Some(t),
            id: Some("abc"),
            ..Fields::default()
        }
    }

    #[test]
    fn empty_rules_keep_everything() {
        assert_eq!(
            rules("").decide(&title("anything")),
            Decision::Keep { include: None }
        );
    }

    #[test]
    fn include_and_exclude() {
        let set = rules(
            r#"include = ['(?i)\blive\b', 'mix']
exclude = ['(?i)\b(trailer|teaser)\b']"#,
        );
        assert_eq!(
            set.decide(&title("Live at Rome")),
            Decision::Keep { include: Some(0) }
        );
        assert_eq!(
            set.decide(&title("summer mix")),
            Decision::Keep { include: Some(1) }
        );
        assert_eq!(
            set.decide(&title("Live trailer")),
            Decision::Excluded { rule: 0 }
        );
        assert_eq!(set.decide(&title("studio")), Decision::NotIncluded);
    }

    #[test]
    fn look_around_works() {
        let set = rules(r#"include = ['(?i)\blive\b(?! stream)']"#);
        assert_eq!(
            set.decide(&title("live set")),
            Decision::Keep { include: Some(0) }
        );
        assert_eq!(set.decide(&title("live stream")), Decision::NotIncluded);
    }

    #[test]
    fn field_rules_and_match_fields() {
        let set = rules(
            r#"match_fields = ["title", "tags"]
include = [{ field = "channel", pattern = '^Lofi Girl$' }, 'chill']"#,
        );
        let tags = vec!["chill".to_string()];
        let item = Fields {
            title: Some("x"),
            channel: Some("Other"),
            tags: Some(&tags),
            ..Fields::default()
        };
        assert_eq!(set.decide(&item), Decision::Keep { include: Some(1) });
        let item = Fields {
            title: Some("x"),
            channel: Some("Lofi Girl"),
            tags: Some(&[]),
            ..Fields::default()
        };
        assert_eq!(set.decide(&item), Decision::Keep { include: Some(0) });
    }

    #[test]
    fn unknown_fields_defer() {
        let set = rules(r#"exclude = [{ field = "description", pattern = 'sponsor' }]"#);
        assert_eq!(
            set.decide(&title("x")),
            Decision::Deferred(RuleField::Description)
        );
        let item = Fields {
            title: Some("x"),
            description: Some("no ads"),
            ..Fields::default()
        };
        assert_eq!(set.decide(&item), Decision::Keep { include: None });
    }

    #[test]
    fn backtrack_limit_is_a_filter_error() {
        let config: crate::config::Config =
            toml::from_str("[playlists.x]\nsource = 'https://www.youtube.com/playlist?list=PLx'\ninclude = ['(a|a)*\\1b']").unwrap();
        let set = RuleSet::compile(&effective_playlists(&config)[0], 100).unwrap();
        let decision = set.decide(&title(&"a".repeat(40)));
        assert!(
            matches!(
                decision,
                Decision::FilterError {
                    list: List::Include,
                    rule: 0,
                    ..
                }
            ),
            "{decision:?}"
        );
    }

    fn arb_rules() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(
            prop::sample::select(vec!["a", "b", "^c", "d$", "(?i)E", "x|y"]),
            0..4,
        )
        .prop_map(|v| v.into_iter().map(String::from).collect())
    }

    fn toml_list(rules: &[String]) -> String {
        let quoted: Vec<_> = rules.iter().map(|r| format!("'{r}'")).collect();
        format!("[{}]", quoted.join(", "))
    }

    proptest! {
        #[test]
        fn exclude_always_wins(inc in arb_rules(), exc in arb_rules(), t in "[a-eA-Exy ]{0,8}") {
            let set = rules(&format!("include = {}\nexclude = {}", toml_list(&inc), toml_list(&exc)));
            let excluded = set.exclude.iter().any(|r| r.test(&title(&t)) == Match::Yes);
            let decision = set.decide(&title(&t));
            prop_assert_eq!(excluded, matches!(decision, Decision::Excluded { .. }));
        }

        #[test]
        fn empty_include_keeps_all_not_excluded(exc in arb_rules(), t in "[a-eA-Exy ]{0,8}") {
            let set = rules(&format!("exclude = {}", toml_list(&exc)));
            let decision = set.decide(&title(&t));
            let ok = matches!(decision, Decision::Keep { include: None } | Decision::Excluded { .. });
            prop_assert!(ok, "unexpected decision: {:?}", decision);
        }

        #[test]
        fn keep_or_drop_is_stable_across_rule_order(inc in arb_rules(), exc in arb_rules(), t in "[a-eA-Exy ]{0,8}") {
            let kept = |inc: &[String], exc: &[String]| {
                let set = rules(&format!("include = {}\nexclude = {}", toml_list(inc), toml_list(exc)));
                matches!(set.decide(&title(&t)), Decision::Keep { .. })
            };
            let mut inc_rev = inc.clone();
            inc_rev.reverse();
            let mut exc_rev = exc.clone();
            exc_rev.reverse();
            prop_assert_eq!(kept(&inc, &exc), kept(&inc_rev, &exc_rev));
        }
    }
}
