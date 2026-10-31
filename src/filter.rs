//! What to download: structural filters (availability, duration, cutoff)
//! and include/exclude rules, compiled once per source.
//!
//! An item is kept when it's downloadable, within the duration limits and the
//! cutoff, matches at least one include rule (or the include list is empty)
//! and no exclude rule. Exclude always wins. A definite drop wins over a
//! missing field; a missing field defers the decision until download time.

use std::fmt;

use fancy_regex::{Regex, RegexBuilder};
use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use serde::Serialize;

use crate::config::duration::Cutoff;
use crate::config::merge::EffectiveSource;
use crate::config::{Rule as RuleConfig, RuleField};
use crate::index::{Availability, RemoteItem};

/// What filters can look at. `None` = not known yet (e.g. the yt-dlp indexer
/// returns no description, and sometimes no date).
#[derive(Debug, Clone, Default)]
pub struct Fields<'a> {
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub channel: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub id: Option<&'a str>,
    /// Seconds.
    pub duration: Option<u64>,
    pub published_at: Option<Timestamp>,
    /// `None` when it doesn't apply (e.g. `filter test` on a bare title).
    pub availability: Option<Availability>,
}

impl<'a> Fields<'a> {
    pub fn from_item(item: &'a RemoteItem) -> Self {
        Fields {
            title: Some(&item.title),
            description: item.description.as_deref(),
            channel: item.channel.as_deref(),
            tags: item.tags.as_deref(),
            id: Some(&item.video_id),
            duration: item.duration,
            published_at: item.published_at,
            availability: Some(item.availability),
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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

/// A field a decision is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Needs {
    Field(RuleField),
    Duration,
    PublishedAt,
}

impl fmt::Display for Needs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Needs::Field(field) => write!(f, "{}", format!("{field:?}").to_lowercase()),
            Needs::Duration => f.write_str("duration"),
            Needs::PublishedAt => f.write_str("publish date"),
        }
    }
}

/// A drop that has nothing to do with include/exclude rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Structural {
    Unavailable {
        availability: Availability,
    },
    TooShort {
        duration: u64,
        min: u64,
    },
    TooLong {
        duration: u64,
        max: u64,
    },
    BeforeCutoff {
        published_at: Timestamp,
        cutoff: Timestamp,
    },
}

/// Why an item is kept or dropped. Stored in the DB and shown by
/// `tidepool list` and `tidepool filter test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum Decision {
    /// Kept; `include` is the matching include rule, if any.
    Keep {
        include: Option<usize>,
    },
    Excluded {
        rule: usize,
    },
    NotIncluded,
    Structural {
        reason: Structural,
    },
    /// Needs a field the indexer didn't provide; check again before download.
    Deferred {
        needs: Needs,
    },
    /// A rule hit the regex backtrack limit; held until the rule is fixed.
    FilterError {
        list: List,
        rule: usize,
        message: String,
    },
}

impl Decision {
    /// A short, stable label for summaries.
    pub fn label(&self) -> &'static str {
        match self {
            Decision::Keep { .. } => "kept",
            Decision::Excluded { .. } => "excluded by a rule",
            Decision::NotIncluded => "not matched by include rules",
            Decision::Structural {
                reason: Structural::Unavailable { .. },
            } => "unavailable",
            Decision::Structural {
                reason: Structural::TooShort { .. },
            } => "too short",
            Decision::Structural {
                reason: Structural::TooLong { .. },
            } => "too long",
            Decision::Structural {
                reason: Structural::BeforeCutoff { .. },
            } => "published before the cutoff",
            Decision::Deferred { .. } => "checked before download",
            Decision::FilterError { .. } => "rule error",
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("decision serializes")
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decision::Keep { include: Some(i) } => write!(f, "keep (matches include[{i}])"),
            Decision::Keep { include: None } => f.write_str("keep (no include rules)"),
            Decision::Excluded { rule } => write!(f, "drop (matches exclude[{rule}])"),
            Decision::NotIncluded => f.write_str("drop (matches no include rule)"),
            Decision::Structural { reason } => match reason {
                Structural::Unavailable { availability } => {
                    write!(f, "drop ({} video)", availability.as_str())
                }
                Structural::TooShort { duration, min } => {
                    write!(f, "drop (too short: {duration}s < {min}s)")
                }
                Structural::TooLong { duration, max } => {
                    write!(f, "drop (too long: {duration}s > {max}s)")
                }
                Structural::BeforeCutoff {
                    published_at,
                    cutoff,
                } => {
                    write!(
                        f,
                        "drop (published {} before the cutoff, {})",
                        published_at.strftime("%Y-%m-%d"),
                        cutoff.strftime("%Y-%m-%d")
                    )
                }
            },
            Decision::Deferred { needs } => {
                write!(f, "deferred: needs {needs} (checked before download)")
            }
            Decision::FilterError {
                list,
                rule,
                message,
            } => write!(f, "held: {list}[{rule}] failed: {message}"),
        }
    }
}

/// A source's compiled filters.
#[derive(Debug, Clone)]
pub struct RuleSet {
    pub include: Vec<Rule>,
    pub exclude: Vec<Rule>,
    pub min_duration: Option<u64>,
    pub max_duration: Option<u64>,
    /// Videos published before this are skipped.
    pub cutoff: Option<Timestamp>,
}

impl RuleSet {
    /// Compile a source's filters. Patterns were validated at config load;
    /// a relative `cutoff` is measured back from `now`.
    pub fn compile(
        effective: &EffectiveSource,
        backtrack_limit: usize,
        now: Timestamp,
    ) -> Result<Self, String> {
        let compile = |list, rules: &[RuleConfig]| {
            rules
                .iter()
                .enumerate()
                .map(|(index, rule)| {
                    let (fields, source) = match rule {
                        RuleConfig::Pattern(p) => (effective.match_fields.clone(), p.clone()),
                        RuleConfig::Field(f) => (vec![f.field], f.pattern.clone()),
                    };
                    let re = RegexBuilder::new(&source)
                        .backtrack_limit(backtrack_limit)
                        .build()
                        .map_err(|e| format!("sources.{}.{list}[{index}]: {e}", effective.key))?;
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
        let cutoff = match &effective.cutoff.value {
            Cutoff::None => None,
            Cutoff::Relative(age) => SignedDuration::try_from(*age)
                .ok()
                .and_then(|d| now.checked_sub(d).ok()),
            Cutoff::Date(date) => date.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp()),
        };
        Ok(Self {
            include: compile(List::Include, &effective.include)?,
            exclude: compile(List::Exclude, &effective.exclude)?,
            min_duration: effective.min_duration.0.map(|d| d.as_secs()),
            max_duration: effective.max_duration.0.map(|d| d.as_secs()),
            cutoff,
        })
    }

    pub fn decide(&self, item: &Fields<'_>) -> Decision {
        let structural = |reason| Decision::Structural { reason };
        let mut deferred: Option<Needs> = None;

        if let Some(availability) = item.availability.filter(|a| !a.downloadable()) {
            return structural(Structural::Unavailable { availability });
        }
        if self.min_duration.is_some() || self.max_duration.is_some() {
            match item.duration {
                None => {
                    deferred.get_or_insert(Needs::Duration);
                }
                Some(duration) => {
                    if let Some(min) = self.min_duration.filter(|min| duration < *min) {
                        return structural(Structural::TooShort { duration, min });
                    }
                    if let Some(max) = self.max_duration.filter(|max| duration > *max) {
                        return structural(Structural::TooLong { duration, max });
                    }
                }
            }
        }
        if let Some(cutoff) = self.cutoff {
            match item.published_at {
                None => {
                    deferred.get_or_insert(Needs::PublishedAt);
                }
                Some(published_at) if published_at < cutoff => {
                    return structural(Structural::BeforeCutoff {
                        published_at,
                        cutoff,
                    });
                }
                Some(_) => {}
            }
        }

        for rule in &self.exclude {
            match rule.test(item) {
                Match::Yes => return Decision::Excluded { rule: rule.index },
                Match::No => {}
                Match::Unknown(field) => {
                    deferred.get_or_insert(Needs::Field(field));
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

        let mut included_by = None;
        if !self.include.is_empty() {
            let mut unknown = false;
            for rule in &self.include {
                match rule.test(item) {
                    Match::Yes => {
                        included_by = Some(rule.index);
                        break;
                    }
                    Match::No => {}
                    Match::Unknown(field) => {
                        unknown = true;
                        deferred.get_or_insert(Needs::Field(field));
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
            if included_by.is_none() && !unknown {
                return Decision::NotIncluded;
            }
        }

        match deferred {
            Some(needs) => Decision::Deferred { needs },
            None => Decision::Keep {
                include: included_by,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_sources;
    use proptest::prelude::*;

    fn rules(toml: &str) -> RuleSet {
        let config: crate::config::Config = toml::from_str(&format!(
            "[sources.x]\nurl = 'https://www.youtube.com/playlist?list=PLx'\n{toml}"
        ))
        .unwrap();
        RuleSet::compile(&effective_sources(&config)[0], 1_000_000, now()).unwrap()
    }

    fn now() -> Timestamp {
        "2026-10-31T12:00:00Z".parse().unwrap()
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
            Decision::Deferred {
                needs: Needs::Field(RuleField::Description)
            }
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
            toml::from_str("[sources.x]\nurl = 'https://www.youtube.com/playlist?list=PLx'\ninclude = ['(a|a)*\\1b']").unwrap();
        let set = RuleSet::compile(&effective_sources(&config)[0], 100, now()).unwrap();
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

    fn video(
        duration: Option<u64>,
        published: Option<&str>,
        availability: Availability,
    ) -> Fields<'static> {
        Fields {
            title: Some("Lofi live radio"),
            id: Some("abc"),
            duration,
            published_at: published.map(|p| p.parse().unwrap()),
            availability: Some(availability),
            ..Fields::default()
        }
    }

    #[test]
    fn structural_filters() {
        let set = rules("min_duration = '60s'\nmax_duration = '4h'\ncutoff = '30d'");
        let recent = Some("2026-10-20T00:00:00Z");
        assert_eq!(
            set.decide(&video(Some(300), recent, Availability::Public)),
            Decision::Keep { include: None }
        );
        assert_eq!(
            set.decide(&video(Some(300), recent, Availability::Unlisted)),
            Decision::Keep { include: None }
        );
        assert_eq!(
            set.decide(&video(Some(30), recent, Availability::Public)),
            Decision::Structural {
                reason: Structural::TooShort {
                    duration: 30,
                    min: 60
                }
            }
        );
        assert_eq!(
            set.decide(&video(Some(5 * 3600), recent, Availability::Public)),
            Decision::Structural {
                reason: Structural::TooLong {
                    duration: 18000,
                    max: 14400
                }
            }
        );
        assert_eq!(
            set.decide(&video(
                Some(300),
                Some("2026-09-01T00:00:00Z"),
                Availability::Public
            )),
            Decision::Structural {
                reason: Structural::BeforeCutoff {
                    published_at: "2026-09-01T00:00:00Z".parse().unwrap(),
                    cutoff: "2026-10-01T12:00:00Z".parse().unwrap()
                }
            }
        );
        for availability in [
            Availability::Private,
            Availability::Deleted,
            Availability::Upcoming,
            Availability::Live,
            Availability::Restricted,
        ] {
            assert_eq!(
                set.decide(&video(None, None, availability)),
                Decision::Structural {
                    reason: Structural::Unavailable { availability }
                }
            );
        }
        // Unknown values defer; a definite drop elsewhere still wins.
        assert_eq!(
            set.decide(&video(None, recent, Availability::Public)),
            Decision::Deferred {
                needs: Needs::Duration
            }
        );
        assert_eq!(
            set.decide(&video(Some(300), None, Availability::Public)),
            Decision::Deferred {
                needs: Needs::PublishedAt
            }
        );
        assert!(matches!(
            set.decide(&video(Some(30), None, Availability::Public)),
            Decision::Structural { .. }
        ));
        let set = rules("min_duration = '60s'\nexclude = ['(?i)live']");
        assert_eq!(
            set.decide(&video(None, None, Availability::Public)),
            Decision::Excluded { rule: 0 }
        );
    }

    #[test]
    fn date_cutoff_and_no_cutoff() {
        let set = rules("cutoff = '2026-01-01'");
        assert!(matches!(
            set.decide(&video(
                Some(1),
                Some("2025-12-31T23:59:59Z"),
                Availability::Public
            )),
            Decision::Structural {
                reason: Structural::BeforeCutoff { .. }
            }
        ));
        assert_eq!(
            set.decide(&video(
                Some(1),
                Some("2026-01-01T00:00:00Z"),
                Availability::Public
            )),
            Decision::Keep { include: None }
        );
        // No limits configured: unknown duration/date don't matter.
        assert_eq!(
            rules("").decide(&video(None, None, Availability::Public)),
            Decision::Keep { include: None }
        );
    }

    #[test]
    fn decisions_serialize_for_storage() {
        assert_eq!(
            Decision::Keep { include: Some(1) }.to_json(),
            r#"{"decision":"keep","include":1}"#
        );
        assert_eq!(
            Decision::Structural {
                reason: Structural::TooShort {
                    duration: 30,
                    min: 60
                }
            }
            .to_json(),
            r#"{"decision":"structural","reason":{"kind":"too_short","duration":30,"min":60}}"#
        );
        assert_eq!(
            Decision::Deferred {
                needs: Needs::PublishedAt
            }
            .to_json(),
            r#"{"decision":"deferred","needs":"published_at"}"#
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
