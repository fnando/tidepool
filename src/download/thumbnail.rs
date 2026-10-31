//! The biggest cover image: gather candidates, fetch them largest first,
//! check real dimensions, convert to `thumbnail_format`.

use std::io::Cursor;

use image::{ImageFormat, ImageReader};

use crate::config::ThumbnailFormat;
use crate::index::Thumbnail;

/// Where YouTube serves thumbnails.
pub const YTIMG: &str = "https://i.ytimg.com";

/// The cover that was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
    pub width: u32,
    pub height: u32,
    pub source_url: String,
}

/// Candidate URLs, largest known size first, unknown sizes last; on equal
/// size the wanted format comes first.
pub fn candidates(
    video_id: &str,
    thumbnails: &[Thumbnail],
    base: &str,
    format: ThumbnailFormat,
) -> Vec<String> {
    let mut all: Vec<(String, Option<u64>)> = thumbnails
        .iter()
        .map(|t| {
            (
                t.url.clone(),
                t.width
                    .zip(t.height)
                    .map(|(w, h)| u64::from(w) * u64::from(h)),
            )
        })
        .collect();
    for url in [
        format!("{base}/vi/{video_id}/maxresdefault.jpg"),
        format!("{base}/vi_webp/{video_id}/maxresdefault.webp"),
    ] {
        if !all.iter().any(|(u, _)| *u == url) {
            all.push((url, None));
        }
    }
    let prefers = |url: &str| match format {
        ThumbnailFormat::Webp => url.contains(".webp"),
        _ => !url.contains(".webp"),
    };
    all.sort_by(|(a, x), (b, y)| {
        y.is_some()
            .cmp(&x.is_some())
            .then(y.cmp(x))
            .then(prefers(b).cmp(&prefers(a)))
    });
    all.into_iter().map(|(url, _)| url).collect()
}

fn extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::WebP => "webp",
        _ => "jpg",
    }
}

/// Convert fetched bytes to the wanted format (`original` keeps them).
pub fn convert(
    bytes: Vec<u8>,
    found: ImageFormat,
    want: ThumbnailFormat,
) -> Result<(Vec<u8>, &'static str), String> {
    let target = match want {
        ThumbnailFormat::Original => return Ok((bytes, extension(found))),
        ThumbnailFormat::Jpg => ImageFormat::Jpeg,
        ThumbnailFormat::Webp => ImageFormat::WebP,
    };
    if found == target {
        return Ok((bytes, extension(found)));
    }
    let image = image::load_from_memory_with_format(&bytes, found).map_err(|e| e.to_string())?;
    let mut out = Cursor::new(Vec::new());
    match target {
        ImageFormat::Jpeg => {
            image::DynamicImage::ImageRgb8(image.to_rgb8()).write_to(&mut out, target)
        }
        _ => image.write_to(&mut out, target),
    }
    .map_err(|e| e.to_string())?;
    Ok((out.into_inner(), extension(target)))
}

/// Fetch candidates in order and return the first real image (not the
/// 120×90 placeholder YouTube serves for missing sizes), converted.
pub async fn fetch_largest(
    client: &reqwest::Client,
    urls: &[String],
    want: ThumbnailFormat,
) -> Option<Cover> {
    for url in urls {
        let Ok(response) = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        else {
            continue;
        };
        let Ok(bytes) = response.bytes().await else {
            continue;
        };
        let bytes = bytes.to_vec();
        let Ok(reader) = ImageReader::new(Cursor::new(&bytes)).with_guessed_format() else {
            continue;
        };
        let Some(found) = reader.format() else {
            continue;
        };
        let Ok((width, height)) = reader.into_dimensions() else {
            continue;
        };
        if (width, height) == (120, 90) {
            continue;
        }
        match convert(bytes, found, want) {
            Ok((bytes, extension)) => {
                return Some(Cover {
                    bytes,
                    extension,
                    width,
                    height,
                    source_url: url.clone(),
                });
            }
            Err(err) => tracing::debug!("cover {url}: {err}"),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn image_bytes(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
        let mut out = Cursor::new(Vec::new());
        let image = image::DynamicImage::ImageRgb8(image);
        if format == ImageFormat::WebP {
            image
                .write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut out))
                .unwrap();
        } else {
            image.write_to(&mut out, format).unwrap();
        }
        out.into_inner()
    }

    fn dims(bytes: &[u8]) -> (ImageFormat, (u32, u32)) {
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap();
        (reader.format().unwrap(), reader.into_dimensions().unwrap())
    }

    fn thumb(url: &str, size: Option<(u32, u32)>) -> Thumbnail {
        Thumbnail {
            url: url.into(),
            width: size.map(|s| s.0),
            height: size.map(|s| s.1),
        }
    }

    #[test]
    fn orders_candidates() {
        let thumbs = [
            thumb("https://i.ytimg.com/vi/x/hqdefault.jpg", None),
            thumb("https://i.ytimg.com/vi/x/hq720.jpg", Some((1280, 720))),
            thumb(
                "https://i.ytimg.com/vi_webp/x/maxresdefault.webp",
                Some((1920, 1080)),
            ),
            thumb(
                "https://i.ytimg.com/vi/x/maxresdefault.jpg",
                Some((1920, 1080)),
            ),
        ];
        assert_eq!(
            candidates("x", &thumbs, YTIMG, ThumbnailFormat::Jpg),
            vec![
                "https://i.ytimg.com/vi/x/maxresdefault.jpg",
                "https://i.ytimg.com/vi_webp/x/maxresdefault.webp",
                "https://i.ytimg.com/vi/x/hq720.jpg",
                "https://i.ytimg.com/vi/x/hqdefault.jpg",
            ]
        );
        let webp_first = candidates("x", &thumbs, YTIMG, ThumbnailFormat::Webp);
        assert_eq!(
            webp_first[0],
            "https://i.ytimg.com/vi_webp/x/maxresdefault.webp"
        );
        // The well-known URLs are added when the indexer didn't list them.
        let few = candidates("x", &[], "http://test", ThumbnailFormat::Jpg);
        assert_eq!(
            few,
            vec![
                "http://test/vi/x/maxresdefault.jpg",
                "http://test/vi_webp/x/maxresdefault.webp"
            ]
        );
    }

    #[test]
    fn converts_formats() {
        let webp = image_bytes(64, 36, ImageFormat::WebP);
        let (jpg, ext) = convert(webp.clone(), ImageFormat::WebP, ThumbnailFormat::Jpg).unwrap();
        assert_eq!((ext, dims(&jpg)), ("jpg", (ImageFormat::Jpeg, (64, 36))));
        let (same, ext) =
            convert(webp.clone(), ImageFormat::WebP, ThumbnailFormat::Original).unwrap();
        assert_eq!((ext, same), ("webp", webp));
        let png = image_bytes(8, 8, ImageFormat::Png);
        let (webp, ext) = convert(png, ImageFormat::Png, ThumbnailFormat::Webp).unwrap();
        assert_eq!((ext, dims(&webp).0), ("webp", ImageFormat::WebP));
    }

    #[tokio::test]
    async fn fetches_the_largest_real_image() {
        let server = MockServer::start().await;
        let serve = |p: &str, bytes: Vec<u8>| {
            Mock::given(path(p.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        };
        // maxresdefault.jpg is the 120×90 placeholder, the webp is real but 1080p.
        serve(
            "/vi/x/maxresdefault.jpg",
            image_bytes(120, 90, ImageFormat::Jpeg),
        )
        .mount(&server)
        .await;
        serve(
            "/vi_webp/x/maxresdefault.webp",
            image_bytes(1920, 1080, ImageFormat::WebP),
        )
        .mount(&server)
        .await;
        serve("/vi/x/hq720.jpg", image_bytes(1280, 720, ImageFormat::Jpeg))
            .mount(&server)
            .await;

        let base = server.uri();
        // As in real info JSON: the maxres entries are listed with sizes.
        let thumbs = [
            thumb(&format!("{base}/vi/x/hq720.jpg"), Some((1280, 720))),
            thumb(&format!("{base}/missing.jpg"), Some((4000, 3000))),
            thumb(
                &format!("{base}/vi/x/maxresdefault.jpg"),
                Some((1920, 1080)),
            ),
            thumb(
                &format!("{base}/vi_webp/x/maxresdefault.webp"),
                Some((1920, 1080)),
            ),
        ];
        let urls = candidates("x", &thumbs, &base, ThumbnailFormat::Jpg);
        let cover = fetch_largest(&reqwest::Client::new(), &urls, ThumbnailFormat::Jpg)
            .await
            .unwrap();
        assert_eq!(
            (cover.width, cover.height, cover.extension),
            (1920, 1080, "jpg")
        );
        assert_eq!(
            cover.source_url,
            format!("{base}/vi_webp/x/maxresdefault.webp")
        );
        assert_eq!(dims(&cover.bytes), (ImageFormat::Jpeg, (1920, 1080)));

        // Nothing usable: no cover, no error.
        assert_eq!(
            fetch_largest(
                &reqwest::Client::new(),
                &[format!("{base}/nope")],
                ThumbnailFormat::Jpg
            )
            .await,
            None
        );
    }
}
