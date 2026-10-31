//! The sync pipeline's stages, shared by `index` and `run`.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::config::merge::EffectiveSource;
use crate::filter::{Fields, RuleSet};
use crate::index::ytdlp;
use crate::paths::{Dirs, expand};
use crate::state::State;
use crate::state::items::{IndexSummary, SourceRecord, record_index, status};

/// A hash of a source's effective filters, stored with the source so later
/// stages can tell when the rules changed.
pub fn rules_hash(source: &EffectiveSource) -> String {
    let filters = serde_json::json!({
        "include": source.include,
        "exclude": source.exclude,
        "match_fields": source.match_fields,
        "min_duration": source.min_duration,
        "max_duration": source.max_duration,
        "cutoff": source.cutoff,
    });
    Sha256::digest(filters.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `cookies_file`, expanded; `None` when unset.
pub fn cookies_file(source: &EffectiveSource, dirs: &Dirs) -> Result<Option<PathBuf>, String> {
    if source.cookies_file.is_empty() {
        return Ok(None);
    }
    expand(&source.cookies_file, &dirs.home).map(Some)
}

/// List one source with yt-dlp, apply its filters and record the result.
pub async fn index_source(
    state: &mut State,
    config: &Config,
    source: &EffectiveSource,
    dirs: &Dirs,
    ytdlp_path: &Path,
    now: Timestamp,
) -> Result<IndexSummary, String> {
    let cookies = cookies_file(source, dirs)?;
    let list = ytdlp::list(ytdlp_path, &source.url, cookies.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    let limit = usize::try_from(config.library.regex_backtrack_limit).unwrap_or(usize::MAX);
    let rules = RuleSet::compile(source, limit, now)?;
    let hash = rules_hash(source);
    let record = SourceRecord {
        key: &source.key,
        rules_hash: &hash,
        index_source: "ytdlp",
    };
    record_index(
        &mut state.conn,
        &record,
        &list,
        |item| rules.decide(&Fields::from_item(item)),
        now,
    )
    .map_err(|e| format!("cannot record the index: {e}"))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// One line per source: `lofi: 12 videos (3 new): 2 to download, 9 skipped (…)`.
pub fn describe_index(key: &str, summary: &IndexSummary) -> String {
    let mut head = format!("{key}: {}", plural(summary.total, "video", "videos"));
    let mut notes = Vec::new();
    if summary.new > 0 {
        notes.push(format!("{} new", summary.new));
    }
    if summary.removed > 0 {
        notes.push(format!("{} gone from the source", summary.removed));
    }
    if !notes.is_empty() {
        head.push_str(&format!(" ({})", notes.join(", ")));
    }

    let mut parts = Vec::new();
    let pending = summary.count(status::PENDING);
    if pending > 0 {
        parts.push(format!("{pending} to download"));
    }
    let check = summary.count(status::PENDING_CHECK);
    if check > 0 {
        parts.push(format!("{check} to check before download"));
    }
    let downloaded = summary.count(status::DONE) + summary.count(status::DOWNLOADED);
    if downloaded > 0 {
        parts.push(format!("{downloaded} already downloaded"));
    }
    let skipped = summary.count(status::FILTERED);
    if skipped > 0 {
        let mut reasons: Vec<(&String, &usize)> = summary.filtered.iter().collect();
        reasons.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.cmp(b)));
        let reasons: Vec<String> = reasons
            .iter()
            .map(|(label, n)| format!("{n} {label}"))
            .collect();
        parts.push(format!("{skipped} skipped ({})", reasons.join(", ")));
    }
    let errors = summary.count(status::FILTER_ERROR);
    if errors > 0 {
        parts.push(format!("{errors} held by a rule error"));
    }
    if parts.is_empty() {
        return head;
    }
    format!("{head}: {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::effective_sources;

    fn source(extra: &str) -> EffectiveSource {
        let config: Config = toml::from_str(&format!(
            "[sources.x]\nurl = 'https://www.youtube.com/playlist?list=PLx'\n{extra}"
        ))
        .unwrap();
        effective_sources(&config).remove(0)
    }

    #[test]
    fn rules_hash_changes_with_filters_only() {
        let base = rules_hash(&source(""));
        assert_eq!(base, rules_hash(&source("format = 'ba'")));
        assert_ne!(base, rules_hash(&source("exclude = ['x']")));
        assert_ne!(base, rules_hash(&source("cutoff = '30d'")));
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn describes_summaries() {
        let mut summary = IndexSummary {
            total: 1,
            ..Default::default()
        };
        assert_eq!(describe_index("a", &summary), "a: 1 video");
        summary.total = 3;
        summary.new = 2;
        summary.removed = 1;
        summary.by_status.insert("pending".into(), 1);
        summary.by_status.insert("done".into(), 1);
        summary.by_status.insert("filtered".into(), 1);
        summary.filtered.insert("too short".into(), 1);
        assert_eq!(
            describe_index("a", &summary),
            "a: 3 videos (2 new, 1 gone from the source): 1 to download, 1 already downloaded, 1 skipped (1 too short)"
        );
    }
}
