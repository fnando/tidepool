//! Downloading end to end: the binary, the fake yt-dlp and a fake thumbnail
//! server.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;
use wiremock::matchers::path_regex;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bin")
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        width,
        height,
        image::Rgb([10, 120, 200]),
    ));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

struct Env {
    tmp: TempDir,
    thumbs: MockServer,
}

impl Env {
    async fn new() -> Self {
        let thumbs = MockServer::start().await;
        Mock::given(path_regex(r"/vi/[^/]+/(maxresdefault|hq720)\.jpg$"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(1280, 720)))
            .mount(&thumbs)
            .await;
        Self {
            tmp: tempfile::tempdir().unwrap(),
            thumbs,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn media(&self) -> PathBuf {
        self.path("media")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("tidepool").unwrap();
        cmd.env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", test_bin().display()))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("TIDEPOOL_TEST_THUMBNAIL_BASE", self.thumbs.uri())
            .env("TIDEPOOL_FAKE_THUMB_BASE", self.thumbs.uri())
            .args(args);
        cmd
    }

    /// Config with a `dl` source (and optionally a second one, `dl2`, on the
    /// same playlist).
    fn config(&self, extra: &str) -> String {
        let text = format!(
            "[library]\nroot = \"{}\"\n{extra}\n\n[ytdlp]\nmanaged = false\npath = \"{}\"\n\n\
             [sources.dl]\nurl = \"https://www.youtube.com/playlist?list=PLdownloads\"\nytdlp_args = [\"--write-subs\"]\n",
            self.media().display(),
            test_bin().join("yt-dlp").display()
        );
        let path = self.path("config.toml");
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn statuses(&self, source: &str) -> Vec<(String, String, i64)> {
        let conn =
            rusqlite::Connection::open(self.path("data/tidepool/libraries/default/state.db"))
                .unwrap();
        let mut stmt = conn
            .prepare("SELECT video_id, status, attempts FROM items WHERE source_key = ?1 ORDER BY position")
            .unwrap();
        stmt.query_map([source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

const RICK_DIR: &str = "dl/2009-10-25 - Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster) [dQw4w9WgXcQ]";

#[tokio::test(flavor = "multi_thread")]
async fn run_indexes_and_downloads_into_the_library() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "--config", &config])
        .assert()
        .code(3)
        .stdout(
            contains("dl: 4 videos (4 new): 4 to download")
                .and(contains("dl: downloaded “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”"))
                .and(contains("dl: downloaded “Video okvideo01”"))
                .and(contains("downloaded 2 videos (")),
        )
        .stderr(
            contains("“Flaky upload” failed (will retry): unable to download video data: HTTP Error 429")
                .and(contains("“Members only stream” is unavailable: Join this channel")),
        );

    // The video's directory, assembled and moved into place.
    let rick = env.media().join(RICK_DIR);
    let mut names: Vec<String> = fs::read_dir(&rick)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)-poster.jpg",
            "media.en.vtt",
            "media.mkv",
            "metadata.json"
        ]
    );
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["source"]["name"], "dl");
    assert_eq!(metadata["source"]["title"], "Downloads");
    assert_eq!(metadata["source"]["position"], 1);
    assert_eq!(
        metadata["files"]["subtitles"],
        serde_json::json!(["media.en.vtt"])
    );
    assert_eq!(metadata["cover"]["width"], 1280);
    assert_eq!(metadata["tool"]["yt_dlp"], "2026.10.01");
    assert_eq!(metadata["channel"]["name"], "Rick Astley");
    assert!(
        env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/media.mkv")
            .is_file()
    );
    // Nothing left in the work area, and no raw info JSON kept by default.
    assert!(
        !env.media().join(".tidepool-tmp").exists()
            || fs::read_dir(env.media().join(".tidepool-tmp"))
                .unwrap()
                .next()
                .is_none()
    );
    assert!(!rick.join("ytdlp.info.json").exists());

    assert_eq!(
        env.statuses("dl"),
        vec![
            ("dQw4w9WgXcQ".into(), "done".into(), 0),
            ("okvideo01".into(), "done".into(), 0),
            ("flaky001".into(), "failed".into(), 1),
            ("members01".into(), "unavailable".into(), 0),
        ]
    );

    // Nothing due: the failed one waits for its retry time.
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains("nothing to download"));
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicates_are_linked_and_reset_downloads_again() {
    let env = Env::new().await;
    let config = env.config("");
    let config_text = fs::read_to_string(&config).unwrap()
        + "\n[sources.dl2]\nurl = \"https://www.youtube.com/playlist?list=PLdownloads2\"\nytdlp_args = [\"--write-subs\"]\n";
    fs::write(&config, config_text).unwrap();

    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    env.cmd(&["run", "-s", "dl2"])
        .assert()
        .success()
        .stdout(contains(
            "dl2: linked from another source “Video okvideo01”",
        ));
    let a = fs::metadata(
        env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/media.mkv"),
    )
    .unwrap();
    let b = fs::metadata(
        env.media()
            .join("dl2/2026-09-01 - Video okvideo01 [okvideo01]/media.mkv"),
    )
    .unwrap();
    assert_eq!(a.ino(), b.ino(), "hardlinked");
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            env.media()
                .join("dl2/2026-09-01 - Video okvideo01 [okvideo01]/metadata.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        meta["source"]["name"], "dl2",
        "each source gets its own metadata.json"
    );

    // reset forgets the download everywhere; --delete-files removes the dirs.
    env.cmd(&["reset", "okvideo01", "--delete-files"])
        .assert()
        .success()
        .stdout(contains("reset okvideo01 in dl, dl2"));
    assert!(
        !env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]")
            .exists()
    );
    assert_eq!(
        env.statuses("dl")[1],
        ("okvideo01".into(), "pending".into(), 0)
    );
    env.cmd(&["download", "-s", "dl", "--limit", "1"])
        .assert()
        .success()
        .stdout(contains("dl: downloaded “Video okvideo01”"));
    env.cmd(&["reset", "nope"])
        .assert()
        .code(1)
        .stderr(contains("no video nope in this library"));
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_a_download_puts_it_back_in_the_queue() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["index", "--config", &config]).assert().code(0);

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("tidepool"));
    child
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", test_bin().display()))
        .env("HOME", env.path("home"))
        .env("XDG_CONFIG_HOME", env.path("config"))
        .env("XDG_DATA_HOME", env.path("data"))
        .env("TIDEPOOL_TEST_THUMBNAIL_BASE", env.thumbs.uri())
        .env("TIDEPOOL_FAKE_YTDLP_SLEEP", "30")
        .args(["download", "--limit", "1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = child.spawn().unwrap();
    // Wait until the download has started.
    for _ in 0..100 {
        if env.statuses("dl")[0].1 == "downloading" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(env.statuses("dl")[0].1, "downloading");
    std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert_eq!(env.statuses("dl")[0].1, "pending");
    let tmp = env.media().join(".tidepool-tmp");
    assert!(
        !tmp.exists() || fs::read_dir(&tmp).unwrap().next().is_none(),
        "work area cleared"
    );
    assert!(!env.media().join(RICK_DIR).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn recurring_failures_become_problems_until_work_succeeds_again() {
    let env = Env::new().await;
    let config = env.config("");
    let text = fs::read_to_string(&config).unwrap()
        + "\n[sources.dl2]\nurl = \"https://www.youtube.com/playlist?list=PLdownloads2\"\n";
    fs::write(&config, text).unwrap();

    // Every download fails the same way: a systemic yt-dlp problem.
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .env(
            "TIDEPOOL_FAKE_YTDLP_FAIL",
            "ERROR: [youtube] x: Signature extraction failed: Some formats may be missing",
        )
        .assert()
        .code(3);

    // The next command shows it in full, once; then a one-line reminder.
    env.cmd(&["index", "-s", "dl"]).assert().success().stderr(
        contains("tidepool problem #1 yt-dlp can't extract videos")
            .and(contains("seen 4 times"))
            .and(contains("sources: dl"))
            .and(contains(
                "last error: Signature extraction failed: Some formats may be missing",
            ))
            .and(contains(
                "hint: yt-dlp 2026.10.01 may be outdated; update it (`yt-dlp -U`, pip or brew)",
            ))
            .and(contains("`tidepool problems ack 1` hides this one")),
    );
    env.cmd(&["index", "-s", "dl"]).assert().success().stderr(
        contains("1 open problem (see `tidepool problems`)")
            .and(contains("tidepool problem #1").not()),
    );

    env.cmd(&["problems"])
        .assert()
        .success()
        .stdout(contains("#1 yt-dlp can't extract videos").and(contains("sources: dl")));
    env.cmd(&["problems", "ack", "1"])
        .assert()
        .success()
        .stdout(contains("problem #1 is hidden until it happens again"));
    env.cmd(&["problems", "ack", "7"])
        .assert()
        .code(1)
        .stderr(contains("no open problem #7"));
    env.cmd(&["index", "-s", "dl"])
        .assert()
        .success()
        .stderr(contains("open problem").not());
    env.cmd(&["problems"])
        .assert()
        .success()
        .stdout(contains("(acknowledged)"));

    // A download that works again resolves it.
    env.cmd(&["run", "-s", "dl2"]).assert().success();
    env.cmd(&["problems"]).assert().success().stdout(
        contains("no open problems")
            .and(contains("recently resolved:"))
            .and(contains("#1 yt-dlp can't extract videos (resolved")),
    );

    // A source that can't be read is a problem right away.
    let text = fs::read_to_string(&config).unwrap()
        + "\n[sources.broken]\nurl = \"https://www.youtube.com/playlist?list=PLdoesnotexist\"\n";
    fs::write(&config, text).unwrap();
    env.cmd(&["index", "-s", "broken", "--config", &config])
        .assert()
        .code(3);
    env.cmd(&["problems"]).assert().success().stdout(
        contains("#2 a source can't be read")
            .and(contains("sources: broken"))
            .and(contains("The playlist does not exist")),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn verbose_shows_config_and_progress() {
    let env = Env::new().await;
    let config = env.config("");
    let text = fs::read_to_string(&config).unwrap().replace(
        "ytdlp_args = [\"--write-subs\"]",
        "ytdlp_args = [\"--write-subs\"]\nmin_duration = \"250s\"",
    );
    fs::write(&config, text).unwrap();

    let assert = env
        .cmd(&["run", "-v", "--config", &config])
        .assert()
        .code(3);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    for expected in [
        "library default: config revision 1, imported from",
        &format!(
            "media in {}, work area {}",
            env.media().display(),
            env.media().join(".tidepool-tmp").display()
        ),
        "yt-dlp 2026.10.01 at ",
        "dl: https://www.youtube.com/playlist?list=PLdownloads → dl/, format bv*+ba/b, output {source_dir}/{upload_date} - {title} [{id}]",
        "dl: filters: min_duration 250s",
        "dl: listing https://www.youtube.com/playlist?list=PLdownloads",
        "dl: listed 4 videos in ",
        "dl: new: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” [dQw4w9WgXcQ]: drop (too short: 213s < 250s)",
        "dl: new: “Video okvideo01” [okvideo01]: keep (no include rules)",
        "dl: downloading “Video okvideo01” [okvideo01]",
        "dl: “Video okvideo01”: 50% of 20 B",
        "dl: “Video okvideo01”: 100% of 20 B",
        "dl: “Video okvideo01”: cover 1280×720 from ",
        "dl: “Video okvideo01”: moved into dl/2026-09-01 - Video okvideo01 [okvideo01] (",
    ] {
        assert!(
            stderr.contains(expected),
            "missing {expected:?} in:\n{stderr}"
        );
    }
    assert!(
        !stderr.contains('\u{1b}'),
        "no terminal colours when stderr isn't a terminal:\n{stderr}"
    );
    assert!(!stderr.contains("yt-dlp command:"), "commands only at -vv");

    // -vv adds the commands and the full effective config.
    env.cmd(&["reset", "okvideo01"]).assert().success();
    let assert = env.cmd(&["run", "-vv"]).assert().success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("yt-dlp command: "), "{stderr}");
    assert!(
        stderr.contains(
            "dl: effective config:\nurl = \"https://www.youtube.com/playlist?list=PLdownloads\""
        ),
        "{stderr}"
    );

    // Quiet by default.
    env.cmd(&["reset", "okvideo01"]).assert().success();
    env.cmd(&["run"])
        .assert()
        .success()
        .stderr(contains("downloading “").not().and(contains("DEBUG").not()));
}
