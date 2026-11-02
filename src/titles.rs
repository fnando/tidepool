//! Cleaning video titles (`clean_title`).
//!
//! `music_video` turns a YouTube music video title into the song's title:
//! "Dead Fish - Sonho Médio (Clipe Oficial)" → "Sonho Médio". It's a port of
//! `extract_clip_name` from the mediabox project, and passes the same litmus
//! suite (`tests/fixtures/music_video_titles.json`).

use std::sync::LazyLock;

use fancy_regex::Regex;
use unicode_normalization::UnicodeNormalization;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}"))
}

/// Whole bracket groups that are credits or annotations rather than part of
/// the title: "(feat. X)", "(from <movie>)", "(starring Y)", "(Sample: Z)",
/// "(DVD … Ao Vivo)", remaster/mono/mix notes and bare "[HD]"/"[4K]".
static GROUP_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?xi)
        [(\[{]\s*(?:feat\.?|ft\.?|featuring|from|starring|sample|dvd)[^)\]}]*[)\]}] |
        [(\[{][^)\]}]*(?:remaster(?:ed)?|\bmono\b|\bmix\b)[^)\]}]*[)\]}] |
        [(\[{]\s*(?:hd|4k|official|video)\s*[)\]}]
    ")
});

/// Video/release descriptors, removed wherever they appear. "Version",
/// "Cut" and year "Mix" qualifiers stay on purpose.
static DESCRIPTORS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?xi)
        official(?:\s+(?:music|hd|4k|lyric|lyrics|vertical))?\s+videos? |
        (?:music|lyric|hd|vertical|promo)\s+video |
        dance\s+performance\s+video |
        restored\s+hd\s+video |
        concert\s+film |
        documentary |
        closed\s+caption(?:ed|s)? |
        (?:web\s?|v[ií]deo\s?)?clipe?\s+oficial |
        v[ií]deo\s+oficial |
        lyric\s+oficial |
        v[ií]deo\s?clipe? |
        (?:official\s+)?visuali[sz]er |
        web\s?clipe? |
        clipe |
        (?:4k|hd)\s+(?:upgrade|upscale) |
        full\s+album\s+stream |
        album\s+stream
    ")
});

/// A bracket group mentioning a video descriptor is only an annotation when
/// it has none of these qualifiers, which mark a distinct cut.
static KEEP_QUALIFIER: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(?:version|cut|mix|live|acoustic|demo|unplugged|sessions?|remix|edit|intro|reprise|instrumental)\b",
    )
});
static VIDEO_GROUP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(?:official[^)\]}]*video|(?:music|lyric|hd|vertical|promo|performance)\s+video|v[ií]deo\s?clipe?|visuali[sz]er|v[ií]deo\s+oficial|clipe?\s+oficial|concert\s+film|from\s+the\s+motion\s+picture)",
    )
});
static GROUP: LazyLock<Regex> = LazyLock::new(|| re(r"[(\[{][^)\]}]*[)\]}]"));
/// "Live Performance (Video)" says the same as "Live".
static LIVE_PERFORMANCE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\blive\s+performance(?:\s+video)?\b"));
static LYRIC_VIDEO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\blyric\s+video\b"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));
static EMPTY_GROUP: LazyLock<Regex> = LazyLock::new(|| re(r"[(\[{]\s*[)\]}]"));
static SEPARATOR_BEFORE_CLOSE: LazyLock<Regex> = LazyLock::new(|| re(r"\s*[-–—|:]+\s*(?=[)\]}])"));
static SEPARATOR_AFTER_OPEN: LazyLock<Regex> = LazyLock::new(|| re(r"(?<=[(\[{])\s*[-–—|:]+\s*"));
static LEADING_NOISE: LazyLock<Regex> = LazyLock::new(|| re(r"\A[\s\-–—|:]+"));
static TRAILING_NOISE: LazyLock<Regex> = LazyLock::new(|| re(r"[\s\-–—|:]+\z"));
static COLON_ARTIST: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)\A([^:]+?):\s+(.+)\z"));
static PREFIX_NOT_ARTIST: LazyLock<Regex> = LazyLock::new(|| re(r"[-–—(\[{]"));
static CREDIT_START: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\A(?:feat\.?|ft\.?|featuring|part\.)\s"));
static OPENS_TITLE: LazyLock<Regex> = LazyLock::new(|| re(r#"\A["'(\[{]"#));
static TRAILING_CREDIT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\s+(?:feat\.?|ft\.?|featuring|part\.|starring)\s+.*\z"));
static DOUBLE_QUOTED: LazyLock<Regex> = LazyLock::new(|| re(r#"(?s)"(.*)""#));
static SINGLE_QUOTED: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)\A'(.+)'\z"));
static MEDLEY_SPLIT: LazyLock<Regex> = LazyLock::new(|| re(r"\s*⧸\s*"));

fn replace_all(regex: &Regex, text: &str, with: &str) -> String {
    regex.replace_all(text, with).into_owned()
}

fn is_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text).unwrap_or(false)
}

fn nfkc(text: &str) -> String {
    text.nfkc().collect()
}

/// Remove a whole "(… Music Video)" style group when it's only an
/// annotation; keep groups with a real qualifier ("(Alternate Version - Official
/// HD Video)" keeps "(Alternate Version)" once the descriptors go).
fn strip_video_groups(name: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for found in GROUP.find_iter(name).flatten() {
        let group = found.as_str();
        let inner: String = {
            let chars: Vec<char> = group.chars().collect();
            chars[1..chars.len() - 1].iter().collect()
        };
        out.push_str(&name[last..found.start()]);
        if is_match(&VIDEO_GROUP, &inner) && !is_match(&KEEP_QUALIFIER, &inner) {
            out.push(' ');
        } else {
            out.push_str(group);
        }
        last = found.end();
    }
    out.push_str(&name[last..]);
    out
}

fn normalize_for_match(text: &str) -> String {
    let lowered = nfkc(text).to_lowercase();
    let dashed: String = lowered
        .chars()
        .map(|c| {
            if "‐‑‒–—―".contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    replace_all(&SPACES, &dashed, " ").trim().to_string()
}

/// Whether `segment` names the artist (a whole word, ignoring case and dash
/// styles), e.g. "Alesso & Anitta" for Anitta. What's in brackets doesn't
/// count.
fn credit_segment(segment: &str, artist: &str) -> bool {
    let seg = normalize_for_match(&replace_all(&GROUP, segment, " "));
    let art = normalize_for_match(artist);
    if art.is_empty() {
        return false;
    }
    let pattern = format!(
        r"(?:\A|[^\p{{Alphabetic}}\p{{N}}]){}(?:[^\p{{Alphabetic}}\p{{N}}]|\z)",
        fancy_regex::escape(&art)
    );
    is_match(&re(&pattern), &seg)
}

fn droppable_credit(segment: &str, artist: &str) -> bool {
    credit_segment(segment, artist) || is_match(&CREDIT_START, segment.trim())
}

fn cleanup_noise(name: &str) -> String {
    let name = replace_all(&EMPTY_GROUP, name, " ");
    let name = replace_all(&SEPARATOR_BEFORE_CLOSE, &name, "");
    let name = replace_all(&SEPARATOR_AFTER_OPEN, &name, "");
    let name = replace_all(&SPACES, &name, " ");
    let name = replace_all(&LEADING_NOISE, &name, "");
    let name = replace_all(&TRAILING_NOISE, &name, "");
    name.trim().to_string()
}

/// Drop an "Artist: " prefix that names the artist. The prefix must be a
/// plain name (no dash or bracket), so "(Sample: …)" stays.
fn strip_colon_artist(name: &str, artist: &str) -> String {
    let Ok(Some(found)) = COLON_ARTIST.captures(name) else {
        return name.to_string();
    };
    let prefix = &found[1];
    let rest = &found[2];
    if is_match(&PREFIX_NOT_ARTIST, prefix) {
        return name.to_string();
    }
    if credit_segment(prefix, artist) {
        rest.to_string()
    } else {
        name.to_string()
    }
}

/// Split on separators outside brackets: " - "/" – "/" — " (a dash followed
/// by a space, so "Heart-Shaped" stays whole) and "|".
fn split_top_level(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut parts = Vec::new();
    let mut buffer = String::new();
    let mut depth = 0usize;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if "([{".contains(c) {
            depth += 1;
        }
        if ")]}".contains(c) && depth > 0 {
            depth -= 1;
        }
        let separator = c == '|' || ("-–—".contains(c) && chars.get(index + 1) == Some(&' '));
        if depth == 0 && separator {
            parts.push(buffer.trim().to_string());
            buffer.clear();
            index += 1;
            while chars.get(index) == Some(&' ') {
                index += 1;
            }
            continue;
        }
        buffer.push(c);
        index += 1;
    }
    parts.push(buffer.trim().to_string());
    parts.retain(|p| !p.is_empty());
    parts
}

/// A collaboration medley ("A - song ⧸ B - song ⧸ C - song") credits the
/// last song; a slashed mashup title ("Numb ⧸ Encore") stays whole.
fn reduce_medley(name: &str) -> String {
    if !name.contains('⧸') {
        return name.to_string();
    }
    let mut pieces: Vec<&str> = MEDLEY_SPLIT.split(name).flatten().collect();
    while pieces.last().is_some_and(|p| p.is_empty()) {
        pieces.pop();
    }
    let last = pieces.last().copied().unwrap_or_default();
    if split_top_level(last).len() > 1 {
        last.to_string()
    } else {
        name.to_string()
    }
}

/// Drop the artist credit before or after the title (split on dashes). The
/// title itself may have " - ", so only end segments go.
fn strip_dash_artist(name: &str, artist: &str) -> String {
    let mut segments = split_top_level(name);
    if segments.len() < 2 {
        return name.to_string();
    }
    let mut dropped = false;
    while segments.len() > 1 && droppable_credit(&segments[0], artist) {
        segments.remove(0);
        dropped = true;
    }
    while segments.len() > 1 && droppable_credit(segments.last().expect("non-empty"), artist) {
        segments.pop();
        dropped = true;
    }
    // "Primary Artist - Title" where the artist was only a guest (its
    // "(feat. …)" already gone): the first segment is the artist, unless it
    // opens with a bracket or quote (then it's part of the title).
    if !dropped && segments.len() > 1 && !is_match(&OPENS_TITLE, &segments[0]) {
        segments.remove(0);
    }
    segments.join(" - ")
}

/// The song's title in a music video's `title`, given the `artist`'s name.
pub fn music_video(title: &str, artist: &str) -> String {
    let name: String = nfkc(title)
        .chars()
        .map(|c| match c {
            '‘' | '’' => '\'',
            '“' | '”' => '"',
            other => other,
        })
        .collect();
    let mut name = replace_all(&SPACES, &name, " ").trim().to_string();

    let lyric_video = is_match(&LYRIC_VIDEO, &name);
    name = replace_all(&LIVE_PERFORMANCE, &name, "Live");

    // Credit/annotation groups first, then descriptor phrases.
    name = replace_all(&GROUP_NOISE, &name, " ");
    name = strip_video_groups(&name);
    name = cleanup_noise(&replace_all(&DESCRIPTORS, &name, ""));
    name = reduce_medley(&name);

    // A double-quoted title is the whole title ('Artist - "Song"').
    if name.matches('"').count() >= 2 {
        name = DOUBLE_QUOTED
            .captures(name.as_str())
            .ok()
            .flatten()
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
            .unwrap_or_default();
    }

    name = strip_colon_artist(&name, artist);
    name = strip_dash_artist(&name, artist);
    name = replace_all(&TRAILING_CREDIT, &name, "");
    if let Some(quoted) = SINGLE_QUOTED
        .captures(name.as_str())
        .ok()
        .flatten()
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    {
        name = quoted;
    }

    // Safe as a file name: look-alikes that NFKC keeps as they are.
    let name: String = name
        .replace('/', "⧸")
        .replace('\\', "⧹")
        .replace(':', "꞉")
        .chars()
        .filter(|c| !"?|*<>\"".contains(*c))
        .collect();

    let result = cleanup_noise(&name);
    if result.is_empty() {
        nfkc(title).trim().to_string()
    } else if lyric_video {
        format!("{result} (Lyric Video)")
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Case {
        input: String,
        artist: String,
        clip_name: String,
    }

    #[test]
    fn every_fixture_gives_the_expected_title() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/music_video_titles.json"
        );
        let cases: Vec<Case> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|case| {
                let actual = music_video(&case.input, &case.artist);
                (actual != case.clip_name).then(|| {
                    format!(
                        "  artist={:?} input={:?}\n    expected: {:?}\n      actual: {:?}",
                        case.artist, case.input, case.clip_name, actual
                    )
                })
            })
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} fixtures failed:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }
}
