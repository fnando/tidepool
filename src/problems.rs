//! Problems: recurring failures surfaced where you'll see them. One open row
//! per kind; repeats bump the count; a problem resolves itself when the same
//! kind of work succeeds again.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

/// Kinds of problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// yt-dlp can't extract videos (often outdated).
    Extraction,
    /// "Sign in to confirm you're not a bot".
    BotCheck,
    /// HTTP 429 from YouTube.
    RateLimited,
    /// HTTP 403 from YouTube.
    Forbidden,
    /// yt-dlp rejected an option.
    Options,
    /// yt-dlp printed something tidepool couldn't read.
    Output,
    /// yt-dlp couldn't be run at all.
    Missing,
    /// A source couldn't be read (deleted or private playlist, wrong URL).
    SourceFailed,
    /// The managed yt-dlp couldn't be updated.
    UpdateFailed,
    /// A rule hit the regex backtrack limit.
    RegexLimit,
    /// A hook failed.
    HookFailing,
    /// The managed Deno couldn't be installed or updated.
    DenoFailed,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Extraction,
        Kind::BotCheck,
        Kind::RateLimited,
        Kind::Forbidden,
        Kind::Options,
        Kind::Output,
        Kind::Missing,
        Kind::SourceFailed,
        Kind::UpdateFailed,
        Kind::RegexLimit,
        Kind::HookFailing,
        Kind::DenoFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Extraction => "ytdlp_extraction",
            Kind::BotCheck => "ytdlp_bot_check",
            Kind::RateLimited => "ytdlp_rate_limited",
            Kind::Forbidden => "ytdlp_forbidden",
            Kind::Options => "ytdlp_options",
            Kind::Output => "ytdlp_output",
            Kind::Missing => "ytdlp_missing",
            Kind::SourceFailed => "source_failed",
            Kind::UpdateFailed => "ytdlp_update_failed",
            Kind::RegexLimit => "regex_limit",
            Kind::HookFailing => "hook_failing",
            Kind::DenoFailed => "deno_failed",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn title(self) -> &'static str {
        match self {
            Kind::Extraction => "yt-dlp can't extract videos",
            Kind::BotCheck => "YouTube asks to confirm you're not a bot",
            Kind::RateLimited => "YouTube is rate-limiting downloads (HTTP 429)",
            Kind::Forbidden => "YouTube refuses downloads (HTTP 403)",
            Kind::Options => "yt-dlp rejects an option",
            Kind::Output => "yt-dlp's output can't be read",
            Kind::Missing => "yt-dlp can't be run",
            Kind::SourceFailed => "a source can't be read",
            Kind::UpdateFailed => "yt-dlp couldn't be updated",
            Kind::RegexLimit => "a rule is too slow (regex limit reached)",
            Kind::HookFailing => "a hook is failing",
            Kind::DenoFailed => {
                "Deno (the JavaScript runtime yt-dlp needs) couldn't be installed or updated"
            }
        }
    }

    /// Whether one occurrence is enough (vs. 3 in a run, or repeats).
    fn immediate(self) -> bool {
        matches!(
            self,
            Kind::Missing
                | Kind::SourceFailed
                | Kind::UpdateFailed
                | Kind::RegexLimit
                | Kind::HookFailing
                | Kind::DenoFailed
        )
    }

    /// Kinds an updated yt-dlp usually fixes.
    pub fn fixed_by_update(self) -> bool {
        matches!(
            self,
            Kind::Extraction | Kind::Forbidden | Kind::Options | Kind::Output
        )
    }

    /// Download problems a successful download resolves.
    fn resolved_by_download(self) -> bool {
        matches!(
            self,
            Kind::Extraction
                | Kind::BotCheck
                | Kind::RateLimited
                | Kind::Forbidden
                | Kind::Options
                | Kind::Output
                | Kind::Missing
        )
    }

    pub fn hint(self, ytdlp_version: Option<&str>, managed: bool) -> String {
        let version = ytdlp_version
            .map(|v| format!("yt-dlp {v}"))
            .unwrap_or_else(|| "yt-dlp".into());
        let update = if managed {
            "tidepool checks for a newer yt-dlp right away; failed videos are retried on later runs"
                .to_string()
        } else {
            "update it (`yt-dlp -U`, pip or brew), or set ytdlp.managed = true".to_string()
        };
        match self {
            Kind::Extraction | Kind::Output => format!("{version} may be outdated; {update}"),
            Kind::Forbidden => format!("YouTube refused the download; {version} may be outdated: {update}"),
            Kind::Options => format!("check the source's ytdlp_args; {version} may also be outdated: {update}"),
            Kind::BotCheck => "set cookies_file to a cookies.txt exported from a browser where you're logged in to YouTube, or wait and try again later".into(),
            Kind::RateLimited => "lower general.concurrency or run tidepool less often; downloads are retried later".into(),
            Kind::Missing => "install yt-dlp, or set ytdlp.managed = true so tidepool downloads it".into(),
            Kind::SourceFailed => "check the source's url: the playlist may have been deleted or made private (private playlists need cookies_file)".into(),
            Kind::UpdateFailed => format!("tidepool keeps using {version}; check your network, or run `tidepool tools update yt-dlp`"),
            Kind::RegexLimit => "simplify the rule, or raise general.regex_backtrack_limit; affected videos are held until then".into(),
            Kind::DenoFailed => "check your network, or run `tidepool tools update deno`; without Deno, YouTube downloads may fail. To use your own, set managed = false in [tools.deno]".into(),
            Kind::HookFailing => "see the hook's log in ~/.local/share/tidepool/logs/hooks/<video_id>.log; after fixing it, run `tidepool hooks run <video_id>` (videos whose download failed are retried on later runs)".into(),
        }
    }
}

/// The problem kind of a yt-dlp failure message, if it's a systemic one.
pub fn classify(message: &str) -> Option<Kind> {
    let m = message.to_lowercase();
    if m.contains("cannot run") {
        Some(Kind::Missing)
    } else if m.contains("not a bot") {
        Some(Kind::BotCheck)
    } else if m.contains("http error 429") || m.contains("too many requests") {
        Some(Kind::RateLimited)
    } else if m.contains("http error 403") {
        Some(Kind::Forbidden)
    } else if m.contains("no such option")
        || m.contains("unrecognized arguments")
        || m.contains("unsupported arguments")
    {
        Some(Kind::Options)
    } else if m.contains("unexpected yt-dlp") || m.contains("didn't report the downloaded file") {
        Some(Kind::Output)
    } else if m.contains("unable to extract")
        || m.contains("extraction failed")
        || m.contains("signature")
        || m.contains("nsig")
    {
        Some(Kind::Extraction)
    } else {
        None
    }
}

/// A stored problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub id: i64,
    pub kind: Kind,
    pub first_seen: String,
    pub last_seen: String,
    pub count: i64,
    pub sample: Option<String>,
    pub ytdlp_version: Option<String>,
    pub sources: Vec<String>,
    pub hint: Option<String>,
    pub shown_cli_at: Option<String>,
    pub acknowledged_at: Option<String>,
    pub resolved_at: Option<String>,
}

const COLUMNS: &str = "id, kind, first_seen, last_seen, count, sample, ytdlp_version, sources_json, hint, \
                       shown_cli_at, acknowledged_at, resolved_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Problem> {
    let kind: String = r.get(1)?;
    let sources: Option<String> = r.get(7)?;
    Ok(Problem {
        id: r.get(0)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Output),
        first_seen: r.get(2)?,
        last_seen: r.get(3)?,
        count: r.get(4)?,
        sample: r.get(5)?,
        ytdlp_version: r.get(6)?,
        sources: sources
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        hint: r.get(8)?,
        shown_cli_at: r.get(9)?,
        acknowledged_at: r.get(10)?,
        resolved_at: r.get(11)?,
    })
}

fn open_problem(conn: &Connection, kind: Kind) -> rusqlite::Result<Option<Problem>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM problems WHERE kind = ?1 AND resolved_at IS NULL"),
        [kind.as_str()],
        row,
    )
    .optional()
}

/// Something that happened `count` times; creates or bumps the open problem.
/// A repeat un-hides an acknowledged problem and shows it again.
#[allow(clippy::too_many_arguments)]
pub fn record(
    conn: &Connection,
    kind: Kind,
    count: i64,
    sample: &str,
    ytdlp_version: Option<&str>,
    sources: &BTreeSet<String>,
    hint: &str,
    now: Timestamp,
) -> rusqlite::Result<i64> {
    let now = now.to_string();
    match open_problem(conn, kind)? {
        Some(existing) => {
            let mut all: BTreeSet<String> = existing.sources.into_iter().collect();
            all.extend(sources.iter().cloned());
            conn.execute(
                "UPDATE problems SET count = count + ?2, last_seen = ?3, sample = ?4,
                   ytdlp_version = COALESCE(?5, ytdlp_version), sources_json = ?6, hint = ?7,
                   shown_cli_at = NULL, acknowledged_at = NULL
                 WHERE id = ?1",
                params![
                    existing.id,
                    count,
                    now,
                    sample,
                    ytdlp_version,
                    serde_json::to_string(&all).unwrap(),
                    hint
                ],
            )?;
            Ok(existing.id)
        }
        None => {
            conn.execute(
                "INSERT INTO problems (kind, first_seen, last_seen, count, sample, ytdlp_version, sources_json, hint)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![kind.as_str(), now, count, sample, ytdlp_version, serde_json::to_string(sources).unwrap(), hint],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

/// The open problem of this kind is fixed.
pub fn resolve(conn: &Connection, kind: Kind, now: Timestamp) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE problems SET resolved_at = ?2 WHERE kind = ?1 AND resolved_at IS NULL",
        params![kind.as_str(), now.to_string()],
    )?;
    Ok(changed > 0)
}

/// A source is fine again: drop it from the problem, resolving it when no
/// source is left.
pub fn resolve_source(
    conn: &Connection,
    kind: Kind,
    source: &str,
    now: Timestamp,
) -> rusqlite::Result<()> {
    let Some(problem) = open_problem(conn, kind)? else {
        return Ok(());
    };
    if !problem.sources.iter().any(|s| s == source) {
        return Ok(());
    }
    let left: Vec<&String> = problem.sources.iter().filter(|s| *s != source).collect();
    if left.is_empty() {
        resolve(conn, kind, now)?;
    } else {
        conn.execute(
            "UPDATE problems SET sources_json = ?2 WHERE id = ?1",
            params![problem.id, serde_json::to_string(&left).unwrap()],
        )?;
    }
    Ok(())
}

/// Open problems, oldest first.
pub fn open(conn: &Connection) -> rusqlite::Result<Vec<Problem>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM problems WHERE resolved_at IS NULL ORDER BY id"
    ))?;
    stmt.query_map([], row)?.collect()
}

/// The most recently resolved problems.
pub fn recently_resolved(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<Problem>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM problems WHERE resolved_at IS NOT NULL ORDER BY resolved_at DESC LIMIT ?1"
    ))?;
    stmt.query_map([limit as i64], row)?.collect()
}

/// Hide an open problem until it happens again. False if there's no such
/// open problem.
pub fn acknowledge(conn: &Connection, id: i64, now: Timestamp) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE problems SET acknowledged_at = ?2 WHERE id = ?1 AND resolved_at IS NULL",
        params![id, now.to_string()],
    )?;
    Ok(changed > 0)
}

pub fn mark_shown(conn: &Connection, ids: &[i64], now: Timestamp) -> rusqlite::Result<()> {
    for id in ids {
        conn.execute(
            "UPDATE problems SET shown_cli_at = ?2 WHERE id = ?1",
            params![id, now.to_string()],
        )?;
    }
    Ok(())
}

fn short_time(t: &str) -> String {
    t.parse::<Timestamp>()
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| t.to_string())
}

/// A problem as shown by `tidepool problems` and in notices.
pub fn describe(problem: &Problem) -> String {
    let times = if problem.count == 1 {
        "once".to_string()
    } else {
        format!("{} times", problem.count)
    };
    let mut out = format!(
        "#{} {}{}\n   seen {times}, {} → {}",
        problem.id,
        problem.kind.title(),
        if problem.acknowledged_at.is_some() {
            " (acknowledged)"
        } else {
            ""
        },
        short_time(&problem.first_seen),
        short_time(&problem.last_seen),
    );
    if let Some(version) = &problem.ytdlp_version {
        out.push_str(&format!(", yt-dlp {version}"));
    }
    if !problem.sources.is_empty() {
        out.push_str(&format!("\n   sources: {}", problem.sources.join(", ")));
    }
    if let Some(sample) = &problem.sample {
        out.push_str(&format!("\n   last error: {sample}"));
    }
    if let Some(hint) = &problem.hint {
        out.push_str(&format!("\n   hint: {hint}"));
    }
    out
}

/// What one run of indexing and downloading ran into.
#[derive(Debug, Default)]
pub struct Tracker {
    failures: BTreeMap<Kind, Seen>,
    downloads_succeeded: bool,
    hooks_succeeded: bool,
    sources_ok: BTreeSet<String>,
    regex_ok: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct Seen {
    count: i64,
    repeated: bool,
    sample: String,
    sources: BTreeSet<String>,
}

impl Tracker {
    fn seen(&mut self, kind: Kind, message: &str, source: &str, repeated: bool) {
        let seen = self.failures.entry(kind).or_default();
        seen.count += 1;
        seen.repeated |= repeated;
        seen.sample = message.to_string();
        seen.sources.insert(source.to_string());
    }

    /// A download failed; `previous_attempts` is how often it failed before.
    pub fn download_failed(&mut self, source: &str, message: &str, previous_attempts: i64) {
        if let Some(kind) = classify(message) {
            self.seen(kind, message, source, previous_attempts > 0);
        }
    }

    pub fn download_succeeded(&mut self) {
        self.downloads_succeeded = true;
    }

    /// A hook failed for a video of `source`.
    pub fn hook_failed(&mut self, source: &str, hook: &str, message: &str) {
        self.seen(
            Kind::HookFailing,
            &format!("{hook}: {message}"),
            source,
            true,
        );
    }

    /// A hook ran fine.
    pub fn hook_succeeded(&mut self) {
        self.hooks_succeeded = true;
    }

    /// A source couldn't be indexed.
    pub fn index_failed(&mut self, source: &str, message: &str) {
        let kind = classify(message).unwrap_or(Kind::SourceFailed);
        self.seen(kind, message, source, true);
    }

    pub fn index_succeeded(&mut self, source: &str) {
        self.sources_ok.insert(source.to_string());
    }

    /// Videos of a source held by a rule that hit the regex limit (0 = none).
    pub fn regex_limit(&mut self, source: &str, held: usize) {
        if held > 0 {
            self.seen(
                Kind::RegexLimit,
                &format!("{held} videos held"),
                source,
                true,
            );
        } else {
            self.regex_ok.insert(source.to_string());
        }
    }

    /// Kinds an updated yt-dlp could fix that this run ran into.
    pub fn wants_update(&self) -> bool {
        self.failures.keys().any(|k| k.fixed_by_update())
    }

    /// Record what crossed a threshold and resolve what works again. Returns
    /// the kinds recorded.
    pub fn finish(
        self,
        conn: &Connection,
        ytdlp_version: Option<&str>,
        managed: bool,
        now: Timestamp,
    ) -> rusqlite::Result<Vec<Kind>> {
        let mut recorded = Vec::new();
        for (kind, seen) in &self.failures {
            if kind.immediate() || seen.repeated || seen.count >= 3 {
                record(
                    conn,
                    *kind,
                    seen.count,
                    &seen.sample,
                    ytdlp_version,
                    &seen.sources,
                    &kind.hint(ytdlp_version, managed),
                    now,
                )?;
                recorded.push(*kind);
            }
        }
        if self.downloads_succeeded {
            for kind in Kind::ALL
                .into_iter()
                .filter(|k| k.resolved_by_download() && !self.failures.contains_key(k))
            {
                resolve(conn, kind, now)?;
            }
        }
        for source in &self.sources_ok {
            resolve_source(conn, Kind::SourceFailed, source, now)?;
            if !self.failures.contains_key(&Kind::Missing) {
                resolve(conn, Kind::Missing, now)?;
            }
        }
        for source in &self.regex_ok {
            resolve_source(conn, Kind::RegexLimit, source, now)?;
        }
        if self.hooks_succeeded && !self.failures.contains_key(&Kind::HookFailing) {
            resolve(conn, Kind::HookFailing, now)?;
        }
        Ok(recorded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    fn at(s: &str) -> Timestamp {
        format!("2026-10-31T{s}:00Z").parse().unwrap()
    }

    #[test]
    fn classifies_messages() {
        assert_eq!(
            classify("unable to download video data: HTTP Error 429: Too Many Requests"),
            Some(Kind::RateLimited)
        );
        assert_eq!(
            classify("Sign in to confirm you’re not a bot. Use --cookies"),
            Some(Kind::BotCheck)
        );
        assert_eq!(
            classify("Signature extraction failed: Some formats may be missing"),
            Some(Kind::Extraction)
        );
        assert_eq!(
            classify("nsig extraction failed: You may experience throttling"),
            Some(Kind::Extraction)
        );
        assert_eq!(classify("HTTP Error 403: Forbidden"), Some(Kind::Forbidden));
        assert_eq!(
            classify("yt-dlp: error: no such option: --bogus"),
            Some(Kind::Options)
        );
        assert_eq!(
            classify("cannot run /x/yt-dlp: No such file"),
            Some(Kind::Missing)
        );
        assert_eq!(
            classify("Private video. Sign in if you've been granted access"),
            None
        );
    }

    #[test]
    fn records_after_three_in_a_run_or_a_repeat() {
        let state = State::open_in_memory().unwrap();
        let mut run = Tracker::default();
        run.download_failed("a", "HTTP Error 429", 0);
        run.download_failed("b", "HTTP Error 429", 0);
        assert!(
            run.finish(&state.conn, Some("2026.10.01"), true, at("10:00"))
                .unwrap()
                .is_empty(),
            "two is noise"
        );

        let mut run = Tracker::default();
        run.download_failed("a", "HTTP Error 429", 1);
        assert_eq!(
            run.finish(&state.conn, Some("2026.10.01"), true, at("11:00"))
                .unwrap(),
            vec![Kind::RateLimited]
        );

        let mut run = Tracker::default();
        for source in ["a", "b", "b"] {
            run.download_failed(source, "HTTP Error 429: Too Many Requests", 0);
        }
        run.finish(&state.conn, None, true, at("12:00")).unwrap();
        let problems = open(&state.conn).unwrap();
        assert_eq!(problems.len(), 1, "one row per kind");
        let p = &problems[0];
        assert_eq!(
            (p.kind, p.count, p.sources.clone()),
            (Kind::RateLimited, 4, vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            (p.first_seen.as_str(), p.last_seen.as_str()),
            ("2026-10-31T11:00:00Z", "2026-10-31T12:00:00Z")
        );
        assert_eq!(
            p.ytdlp_version.as_deref(),
            Some("2026.10.01"),
            "kept when unknown later"
        );
        assert!(describe(p).contains("seen 4 times, 2026-10-31 11:00 UTC → 2026-10-31 12:00 UTC"));
    }

    #[test]
    fn resolves_when_work_succeeds_again() {
        let state = State::open_in_memory().unwrap();
        let mut run = Tracker::default();
        run.index_failed(
            "a",
            "ERROR: [youtube:tab] PLx: The playlist does not exist.",
        );
        run.index_failed(
            "b",
            "ERROR: [youtube:tab] PLy: The playlist does not exist.",
        );
        for _ in 0..3 {
            run.download_failed("c", "HTTP Error 429", 0);
        }
        run.regex_limit("c", 2);
        let recorded = run.finish(&state.conn, None, false, at("10:00")).unwrap();
        assert_eq!(
            recorded,
            vec![Kind::RateLimited, Kind::SourceFailed, Kind::RegexLimit]
        );

        let mut run = Tracker::default();
        run.index_succeeded("a");
        run.download_succeeded();
        run.regex_limit("c", 0);
        run.finish(&state.conn, None, false, at("11:00")).unwrap();
        let open_now = open(&state.conn).unwrap();
        assert_eq!(open_now.len(), 1);
        assert_eq!(
            (open_now[0].kind, open_now[0].sources.clone()),
            (Kind::SourceFailed, vec!["b".to_string()])
        );
        let resolved = recently_resolved(&state.conn, 10).unwrap();
        assert_eq!(resolved.len(), 2);

        // Happening again after it was resolved starts a new problem.
        let mut run = Tracker::default();
        run.download_failed("c", "HTTP Error 429", 2);
        run.finish(&state.conn, None, false, at("12:00")).unwrap();
        let again = open(&state.conn)
            .unwrap()
            .into_iter()
            .find(|p| p.kind == Kind::RateLimited)
            .unwrap();
        assert_eq!(
            (again.count, again.first_seen.as_str()),
            (1, "2026-10-31T12:00:00Z")
        );
    }

    #[test]
    fn acknowledged_problems_come_back_when_they_happen_again() {
        let state = State::open_in_memory().unwrap();
        let id = record(
            &state.conn,
            Kind::Missing,
            1,
            "cannot run",
            None,
            &BTreeSet::new(),
            "install",
            at("10:00"),
        )
        .unwrap();
        mark_shown(&state.conn, &[id], at("10:01")).unwrap();
        assert!(acknowledge(&state.conn, id, at("10:02")).unwrap());
        assert!(!acknowledge(&state.conn, 999, at("10:02")).unwrap());
        let p = &open(&state.conn).unwrap()[0];
        assert!(p.acknowledged_at.is_some() && p.shown_cli_at.is_some());
        record(
            &state.conn,
            Kind::Missing,
            1,
            "cannot run",
            None,
            &BTreeSet::new(),
            "install",
            at("11:00"),
        )
        .unwrap();
        let p = &open(&state.conn).unwrap()[0];
        assert_eq!(
            (p.acknowledged_at.clone(), p.shown_cli_at.clone(), p.count),
            (None, None, 2)
        );
    }

    #[test]
    fn hints_depend_on_managed_ytdlp() {
        assert!(
            Kind::Extraction
                .hint(Some("2025.09.01"), true)
                .contains("yt-dlp 2025.09.01 may be outdated; tidepool checks for a newer yt-dlp")
        );
        assert!(
            Kind::Extraction
                .hint(Some("2025.09.01"), false)
                .contains("`yt-dlp -U`")
        );
        assert!(Kind::BotCheck.hint(None, true).contains("cookies_file"));
    }
}
