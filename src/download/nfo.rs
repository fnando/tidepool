//! `embed.nfo`: an `.nfo` file next to the video, named after it, with the
//! same details as the tags (Kodi's `<musicvideo>` format, which Jellyfin
//! reads too). Media servers trust it over the file's own tags, and read it
//! again when it changes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::info::VideoInfo;

/// Where a video's `.nfo` goes: next to `media`, named after it.
#[must_use]
pub fn path_for(media: &Path) -> PathBuf {
    media.with_extension("nfo")
}

/// The `.nfo` for a video tagged `tags` (as embedded, names in any case).
#[must_use]
pub fn render(tags: &BTreeMap<String, String>, info: &VideoInfo) -> String {
    let tag = |name: &str| {
        tags.iter()
            .find(|(k, v)| k.eq_ignore_ascii_case(name) && !v.trim().is_empty())
            .map(|(_, v)| v.trim())
    };
    let mut fields: Vec<(&str, String)> = Vec::new();
    let mut put = |name: &'static str, value: Option<&str>| {
        if let Some(value) = value {
            fields.push((name, value.to_string()));
        }
    };
    put("title", tag("title").or(Some(info.title.as_str())));
    put("artist", tag("artist"));
    put("albumartist", tag("album_artist"));
    put("album", tag("album"));
    put("genre", tag("genre"));
    let date = tag("date");
    put("premiered", date.filter(|d| d.len() == 10));
    put("year", date.and_then(|d| d.get(..4)));
    put("plot", tag("description").or_else(|| tag("synopsis")));
    put("track", tag("track"));
    let runtime = info
        .duration
        .filter(|d| *d > 0.0)
        .map(|d| format!("{}", (d / 60.0).round().max(1.0)));
    put("runtime", runtime.as_deref());
    put("musicbrainzartistid", tag("MUSICBRAINZ_ARTISTID"));
    put("musicbrainzalbumid", tag("MUSICBRAINZ_ALBUMID"));
    put(
        "musicbrainzreleasegroupid",
        tag("MUSICBRAINZ_RELEASEGROUPID"),
    );
    put("musicbrainztrackid", tag("MUSICBRAINZ_TRACKID"));

    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<musicvideo>\n",
    );
    for (name, value) in &fields {
        let _ = writeln!(out, "  <{name}>{}</{name}>", escape(value));
    }
    // Files that aren't on YouTube have no address.
    if info.webpage_url.as_deref() != Some("") {
        let _ = writeln!(
            out,
            "  <uniqueid type=\"youtube\" default=\"true\">{}</uniqueid>",
            escape(&info.id)
        );
    }
    out.push_str("</musicvideo>\n");
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Not allowed in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => out.push(c),
        }
    }
    out
}

/// Write the `.nfo` to `tmp/media.nfo`, ready to be moved into place.
///
/// # Errors
///
/// A message when it can't be written.
pub fn write(
    tmp: &Path,
    tags: &BTreeMap<String, String>,
    info: &VideoInfo,
) -> Result<PathBuf, String> {
    let path = tmp.join("media.nfo");
    std::fs::write(&path, render(tags, info))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(json: &str) -> VideoInfo {
        VideoInfo::parse(json).unwrap()
    }

    #[test]
    fn has_what_the_tags_say() {
        let tags: BTreeMap<String, String> = [
            ("title", "Be Sweet"),
            ("ARTIST", "The Bombpops"),
            ("album_artist", "The Bombpops"),
            ("album", "Death in Venice Beach"),
            ("genre", "Music"),
            ("date", "2018-05-03"),
            ("description", "Fat & <loud>"),
            ("track", "3"),
            ("comment", "https://www.youtube.com/watch?v=abc"),
            ("MUSICBRAINZ_ARTISTID", "artist-id"),
            ("MUSICBRAINZ_ALBUMID", "album-id"),
            ("genre_extra", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let nfo = render(
            &tags,
            &info(r#"{"id": "dQw4w9WgXcQ", "title": "YouTube's title", "duration": 212.0}"#),
        );
        assert_eq!(
            nfo,
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<musicvideo>\n\
             \x20 <title>Be Sweet</title>\n\
             \x20 <artist>The Bombpops</artist>\n\
             \x20 <albumartist>The Bombpops</albumartist>\n\
             \x20 <album>Death in Venice Beach</album>\n\
             \x20 <genre>Music</genre>\n\
             \x20 <premiered>2018-05-03</premiered>\n\
             \x20 <year>2018</year>\n\
             \x20 <plot>Fat &amp; &lt;loud&gt;</plot>\n\
             \x20 <track>3</track>\n\
             \x20 <runtime>4</runtime>\n\
             \x20 <musicbrainzartistid>artist-id</musicbrainzartistid>\n\
             \x20 <musicbrainzalbumid>album-id</musicbrainzalbumid>\n\
             \x20 <uniqueid type=\"youtube\" default=\"true\">dQw4w9WgXcQ</uniqueid>\n\
             </musicvideo>\n"
        );
    }

    #[test]
    fn without_tags_it_has_the_title() {
        let nfo = render(
            &BTreeMap::new(),
            &info(r#"{"id": "abcdefghijk", "title": "Song", "webpage_url": ""}"#),
        );
        assert!(nfo.contains("<title>Song</title>"), "{nfo}");
        assert!(!nfo.contains("uniqueid"), "not on YouTube: {nfo}");
        assert!(!nfo.contains("runtime"));
    }

    #[test]
    fn sits_next_to_the_video() {
        assert_eq!(
            path_for(Path::new("/m/The Bombpops/Be Sweet.mkv")),
            Path::new("/m/The Bombpops/Be Sweet.nfo")
        );
    }
}
