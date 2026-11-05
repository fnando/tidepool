//! Downloading end to end: the binary, the fake yt-dlp and a fake thumbnail
//! server.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
            "[category.default]\nmedia_dir = \"{}\"\n{extra}\n\n[tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n\n\
             [sources.dl]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads\"]\nytdlp_args = [\"--write-subs\"]\n",
            self.media().display(),
            test_bin().join("yt-dlp").display()
        );
        let path = self.path("config.toml");
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn statuses(&self, source: &str) -> Vec<(String, String, i64)> {
        let conn = rusqlite::Connection::open(self.path("data/tidepool/state.db")).unwrap();
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

    // The video's files, assembled and moved into place.
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
            "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).en.vtt",
            "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv",
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
        serde_json::json!([{
            "lang": "en",
            "path": "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).en.vtt"
        }])
    );
    assert_eq!(metadata["cover"]["width"], 1280);
    assert_eq!(metadata["tool"]["yt_dlp"], "2026.10.01");
    assert_eq!(metadata["channel"]["name"], "Rick Astley");
    assert!(
        env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv")
            .is_file()
    );
    // Nothing left in the work area, and no raw info JSON kept by default.
    let tmp = env.path("data/tidepool/tmp");
    assert!(!tmp.exists() || fs::read_dir(&tmp).unwrap().next().is_none());
    assert!(!rick.join("metadata.ytdlp.json").exists());

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
async fn videos_that_cant_be_downloaded_can_be_ignored() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "--config", &config]).assert().code(3);

    env.cmd(&["ignore", "flaky001", "members01", "okvideo01", "nope"])
        .assert()
        .code(3)
        .stdout(
            "dl: ignored flaky001\ndl: ignored members01\n\
             okvideo01: nothing to ignore (only videos that couldn't be downloaded can be)\n\
             nope: nothing to ignore (only videos that couldn't be downloaded can be)\n",
        );
    assert_eq!(
        env.statuses("dl")[2..],
        [
            ("flaky001".into(), "ignored".into(), 1),
            ("members01".into(), "ignored".into(), 0),
        ]
    );
    // Not tried again.
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains("nothing to download"));

    env.cmd(&["unignore", "flaky001", "-s", "dl"])
        .assert()
        .success()
        .stdout("dl: flaky001 will be downloaded on the next sync\n");
    env.cmd(&["unignore", "flaky001"])
        .assert()
        .code(3)
        .stdout("flaky001: not ignored\n");
    assert_eq!(
        env.statuses("dl")[2],
        ("flaky001".into(), "pending".into(), 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicates_are_linked_and_reset_downloads_again() {
    let env = Env::new().await;
    let config = env.config("");
    let config_text = fs::read_to_string(&config).unwrap()
        + "\n[sources.dl2]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads2\"]\nytdlp_args = [\"--write-subs\"]\n";
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
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv"),
    )
    .unwrap();
    let b = fs::metadata(
        env.media()
            .join("dl2/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv"),
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
        .stderr(contains("no video nope in any source"));
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_a_download_puts_it_back_in_the_queue() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["index", "--config", &config]).assert().code(0);
    // Something that isn't tidepool's in the work area survives the cleanup.
    fs::create_dir_all(env.path("data/tidepool/tmp/keep-me")).unwrap();

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
    let tmp = env.path("data/tidepool/tmp");
    let left: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["keep-me"], "work folders cleared, nothing else");
    assert!(!env.media().join(RICK_DIR).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_processing_doesnt_hold_up_downloads() {
    let env = Env::new().await;
    // The first video's processing waits for the next download, one at a
    // time each: that only works if downloading goes on meanwhile.
    let marker = env.path("first");
    let wait = script(
        &env,
        "wait.sh",
        &format!(
            r#"cat > /dev/null
[ -e '{m}' ] && exit 0
touch '{m}'
tmp=$(dirname "$(dirname "$TIDEPOOL_OUTPUT_DIR")")
mine=$(basename "$(dirname "$TIDEPOOL_OUTPUT_DIR")")
for i in $(seq 100); do
  for f in "$tmp"/*/fetched/media.*; do
    case "$f" in "$tmp/$mine/"*) ;; *) [ -e "$f" ] && exit 0 ;; esac
  done
  sleep 0.05
done
echo "no other download while processing" >&2; exit 1"#,
            m = marker.display()
        ),
    );
    let config = hooks_config(
        &env,
        "hooks = [\"wait\"]",
        &format!(
            "[hooks.wait]\ntype = \"downloaded\"\ncommand = [\"{}\"]\n",
            wait.display()
        ),
    );
    let text = fs::read_to_string(&config).unwrap().replace(
        "[tools.deno]",
        "[general]\nconcurrency = 1\nprocess_concurrency = 1\n\n[tools.deno]",
    );
    fs::write(&config, text).unwrap();
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3)
        .stdout(contains("downloaded 2 videos"));
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_processing_resumes_without_downloading_again() {
    let env = Env::new().await;
    let marker = env.path("stopped-once");
    // The first time: stop tidepool while it processes Rick's video.
    let stop = script(
        &env,
        "stop.sh",
        &format!(
            r#"cat > /dev/null
[ "$TIDEPOOL_VIDEO_ID" = dQw4w9WgXcQ ] || exit 0
[ -e '{m}' ] && exit 0
touch '{m}'; kill -TERM $PPID; sleep 5"#,
            m = marker.display()
        ),
    );
    let config = hooks_config(
        &env,
        "hooks = [\"stop\"]",
        &format!(
            "[hooks.stop]\ntype = \"downloaded\"\ncommand = [\"{}\"]\n",
            stop.display()
        ),
    );
    let log = env.path("ytdlp.log");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .env("TIDEPOOL_FAKE_YTDLP_LOG", &log)
        .assert()
        .code(143);
    let rick = env
        .statuses("dl")
        .into_iter()
        .find(|(id, _, _)| id == "dQw4w9WgXcQ")
        .unwrap();
    assert_eq!(rick.1, "fetched");
    assert!(
        env.path("data/tidepool/tmp/dQw4w9WgXcQ.dl/fetched")
            .is_dir()
    );
    assert!(!env.media().join(RICK_DIR).exists());

    fs::remove_file(&log).unwrap();
    env.cmd(&["download"])
        .env("TIDEPOOL_FAKE_YTDLP_LOG", &log)
        .assert()
        .stdout(contains("dl: downloaded “Rick Astley"));
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !calls.contains("dQw4w9WgXcQ"),
        "not downloaded again: {calls}"
    );
    assert!(env.media().join(RICK_DIR).is_dir());
    assert!(!env.path("data/tidepool/tmp/dQw4w9WgXcQ.dl").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetched_video_without_its_download_is_downloaded_again() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let db = rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
    db.execute(
        "UPDATE items SET status = 'fetched' WHERE video_id = 'dQw4w9WgXcQ'",
        [],
    )
    .unwrap();
    let log = env.path("ytdlp.log");
    env.cmd(&["download"])
        .env("TIDEPOOL_FAKE_YTDLP_LOG", &log)
        .assert()
        .stdout(contains("dl: downloaded “Rick Astley"));
    assert!(fs::read_to_string(&log).unwrap().contains("dQw4w9WgXcQ"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_waits_for_finished_hooks() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let db = rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
    for video in ["dQw4w9WgXcQ", "okvideo01"] {
        db.execute(
            "INSERT INTO hook_runs (source_key, video_id, hook, type, status, retries, next_attempt_at)
             VALUES ('dl', ?1, 'notify', 'finished', 'retrying', 1, '2999-01-01T00:00:00Z')",
            [video],
        )
        .unwrap();
    }
    let config = hooks_config(&env, "[category.default.cleanup]\nmax_items = 1", "");
    env.cmd(&["cleanup", "--config", &config])
        .assert()
        .success()
        .stdout(contains("kept for now: hook notify hasn't run yet"));
    assert!(env.media().join(RICK_DIR).join("metadata.json").is_file());
    assert!(
        env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/metadata.json")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recurring_failures_become_problems_until_work_succeeds_again() {
    let env = Env::new().await;
    let config = env.config("");
    let text = fs::read_to_string(&config).unwrap()
        + "\n[sources.dl2]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads2\"]\n";
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
        + "\n[sources.broken]\nurls = [\"https://www.youtube.com/playlist?list=PLdoesnotexist\"]\n";
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
    ) + "\n[category.default.youtube]\napi_key = \"AIzaSyD-1234567890x1Qw\"\n\n[server]\ntoken = \"my-long-secret-token\"\n";
    fs::write(&config, text).unwrap();

    let assert = env
        .cmd(&["run", "-v", "--config", &config])
        .assert()
        .code(3);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    for expected in [
        "config revision 1, imported from",
        &format!(
            "media in {}, work area {}",
            env.media().display(),
            env.path("data/tidepool/tmp").display()
        ),
        "yt-dlp 2026.10.01 at ",
        "dl: https://www.youtube.com/playlist?list=PLdownloads (category default), format bv*+ba/b/bv*/ba, output_dir {media_dir}/{source}/{upload_date} - {title} [{id}]",
        "dl: filters: min_duration 250s",
        "dl: listing https://www.youtube.com/playlist?list=PLdownloads",
        "dl: listed 4 videos in ",
        "dl: new: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” [dQw4w9WgXcQ]: drop (too short: 213s < 250s)",
        "dl: new: “Video okvideo01” [okvideo01]: keep (no include rules)",
        "dl: downloading “Video okvideo01” [okvideo01]",
        "dl: “Video okvideo01”: 50% of 20 B",
        "dl: “Video okvideo01”: 100% of 20 B",
        "dl: “Video okvideo01”: cover 1280×720 from ",
        &format!(
            "dl: “Video okvideo01”: saved as {} (",
            env.media()
                .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv")
                .display()
        ),
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
        stderr.contains("effective config (secrets redacted):\n[general]"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "[sources.dl]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads\"]"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("api_key = \"AIza…x1Qw\""), "{stderr}");
    assert!(stderr.contains("token = \"my-l…oken\""), "{stderr}");
    assert!(
        !stderr.contains("AIzaSyD-1234567890x1Qw") && !stderr.contains("my-long-secret-token"),
        "secrets never appear in full:\n{stderr}"
    );
    assert!(
        !stderr.contains("connecting to"),
        "only tidepool's own logs, not its libraries':\n{stderr}"
    );

    // Quiet by default.
    env.cmd(&["reset", "okvideo01"]).assert().success();
    env.cmd(&["run"])
        .assert()
        .success()
        .stderr(contains("downloading “").not().and(contains("DEBUG").not()));
}

#[tokio::test(flavor = "multi_thread")]
async fn categories_send_sources_to_their_own_folders() {
    let env = Env::new().await;
    let config = env.config("");
    let music = env.path("music");
    let text = fs::read_to_string(&config).unwrap()
        + &format!(
            "\n[category.music]\nmedia_dir = \"{}\"\noutput_dir = \"{{media_dir}}/{{category}}/{{title}} [{{id}}]\"\n\
             \n[sources.tunes]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads2\"]\ncategory = \"music\"\n",
            music.display()
        );
    fs::write(&config, text).unwrap();

    env.cmd(&["run", "-s", "tunes", "--config", &config])
        .assert()
        .success()
        .stdout(contains("tunes: downloaded “Video okvideo01”"));
    let tune = music.join("music/Video okvideo01 [okvideo01]");
    assert!(
        tune.join("Video okvideo01.mkv").is_file(),
        "in the music category's media_dir, with its output_dir template"
    );
    assert!(!env.media().join("tunes").exists());
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tune.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["source"]["name"], "tunes");

    // The same video and format in a source of the default category: linked
    // across roots instead of downloaded again.
    env.cmd(&["run", "-s", "dl"])
        .assert()
        .code(3)
        .stdout(contains("dl: linked from another source “Video okvideo01”"));
    let linked = env
        .media()
        .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv");
    assert_eq!(
        fs::metadata(&linked).unwrap().ino(),
        fs::metadata(tune.join("Video okvideo01.mkv"))
            .unwrap()
            .ino(),
        "hardlinked"
    );

    // reset --delete-files finds files in any media_dir.
    env.cmd(&["reset", "okvideo01", "--delete-files"])
        .assert()
        .success()
        .stdout(contains(format!("deleted {}", tune.display())));
    assert!(!tune.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn files_go_anywhere_and_folders_can_be_shared() {
    let env = Env::new().await;
    let config = env.config(
        "output_dir = \"{media_dir}/{source}\"\n\
         cover_file = \"{data_dir}/covers/{id}.webp\"\n\
         metadata_file = \"{output_dir}/{id}.json\"\n\
         keep_raw_info = true",
    );
    env.cmd(&["run", "--config", &config]).assert().code(3);

    let dir = env.media().join("dl");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).en.vtt",
            "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv",
            "Video okvideo01.en.vtt",
            "Video okvideo01.mkv",
            "dQw4w9WgXcQ.json",
            "dQw4w9WgXcQ.ytdlp.json",
            "okvideo01.json",
            "okvideo01.ytdlp.json",
        ],
        "two videos side by side in one folder"
    );
    let cover = env.path("data/tidepool/covers/okvideo01.webp");
    let bytes = fs::read(&cover).unwrap();
    assert_eq!(&bytes[8..12], b"WEBP", "the extension picks the format");
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("okvideo01.json")).unwrap()).unwrap();
    assert_eq!(meta["files"]["media"], "Video okvideo01.mkv");
    assert_eq!(meta["files"]["cover"], cover.display().to_string());

    // Deleting one video's files leaves the other, and the shared folder.
    env.cmd(&["reset", "okvideo01", "--delete-files"])
        .assert()
        .success()
        .stdout(contains(format!("deleted {}", cover.display())));
    assert!(!cover.exists() && !dir.join("Video okvideo01.mkv").exists());
    assert!(!dir.join("okvideo01.json").exists() && !dir.join("okvideo01.ytdlp.json").exists());
    assert!(dir.join("dQw4w9WgXcQ.json").is_file());
}

/// A directory whose `ffmpeg`/`ffprobe` are the real ones and whose `yt-dlp`
/// is the fake; `None` when ffmpeg isn't installed.
fn real_ffmpeg_bin(env: &Env) -> Option<PathBuf> {
    let find = |tool: &str| {
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .ok()?;
        let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!path.is_empty()).then(|| PathBuf::from(path))
    };
    let (ffmpeg, ffprobe) = (find("ffmpeg")?, find("ffprobe")?);
    let bin = env.path("real-bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(ffmpeg, bin.join("ffmpeg")).unwrap();
    std::os::unix::fs::symlink(ffprobe, bin.join("ffprobe")).unwrap();
    std::os::unix::fs::symlink(test_bin().join("yt-dlp"), bin.join("yt-dlp")).unwrap();
    Some(bin)
}

fn ffprobe_json(file: &Path) -> serde_json::Value {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-show_chapters",
            "-of",
            "json",
        ])
        .arg(file)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn embeds_tags_cover_chapters_and_subtitles() {
    let env = Env::new().await;
    let Some(bin) = real_ffmpeg_bin(&env) else {
        eprintln!("skipped: ffmpeg not installed");
        return;
    };
    let sample = env.path("sample.mkv");
    assert!(
        std::process::Command::new(bin.join("ffmpeg"))
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x36:rate=10:duration=1")
            .args(["-f", "lavfi", "-i", "sine=duration=1"])
            .arg(&sample)
            .status()
            .unwrap()
            .success()
    );
    let config = env.config(
        "[category.default.embed]\nmetadata = true\ncover = true\nchapters = true\nsubtitles = true",
    );
    let config_text = fs::read_to_string(&config).unwrap()
        + "\n[sources.dl2]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads2\"]\nytdlp_args = [\"--write-subs\"]\n";
    fs::write(&config, config_text).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let run = |args: &[&str]| {
        let mut cmd = env.cmd(args);
        cmd.env("PATH", &path).env("TIDEPOOL_FAKE_MEDIA", &sample);
        cmd
    };
    run(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);

    let rick = env.media().join(RICK_DIR);
    let title = "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)";
    let json = ffprobe_json(&rick.join(format!("{title}.mkv")));
    let tags = &json["format"]["tags"];
    assert_eq!(tags["title"], title);
    assert_eq!(tags["ARTIST"], "Rick Astley");
    assert_eq!(tags["ALBUM_ARTIST"], "Rick Astley");
    assert_eq!(tags["DATE"], "2009-10-25");
    assert_eq!(
        tags["COMMENT"],
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    );
    assert_eq!(tags["ALBUM"], "Downloads");
    assert_eq!(tags["track"], "1");
    let streams = json["streams"].as_array().unwrap();
    assert!(
        streams
            .iter()
            .any(|s| s["tags"]["mimetype"] == "image/jpeg"),
        "cover attached"
    );
    assert!(
        streams.iter().any(|s| s["codec_type"] == "subtitle"),
        "subtitles embedded"
    );
    assert!(
        !json["chapters"].as_array().unwrap().is_empty(),
        "chapters embedded"
    );
    assert!(
        rick.join(format!("{title}.en.vtt")).is_file(),
        "subtitle files are kept too"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(
        meta["embedded"],
        serde_json::json!({ "metadata": true, "cover": true, "chapters": true, "subtitles": true })
    );
    assert_eq!(meta["embed_error"], serde_json::Value::Null);

    // The same video in another source: still one hardlinked file, carrying
    // the tags of the source that embedded last.
    run(&["run", "-s", "dl2"])
        .assert()
        .success()
        .stdout(contains(
            "dl2: linked from another source “Video okvideo01”",
        ));
    let a = env
        .media()
        .join("dl/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv");
    let b = env
        .media()
        .join("dl2/2026-09-01 - Video okvideo01 [okvideo01]/Video okvideo01.mkv");
    assert_eq!(
        fs::metadata(&a).unwrap().ino(),
        fs::metadata(&b).unwrap().ino()
    );
    assert_eq!(ffprobe_json(&a)["format"]["tags"]["ALBUM"], "Downloads 2");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_embedding_keeps_the_download() {
    let env = Env::new().await;
    // The fake media isn't a real video, so ffprobe rejects it.
    let config = env.config("[category.default.embed]\nmetadata = true");
    env.cmd(&["run", "--config", &config])
        .assert()
        .code(3)
        .stderr(contains(
            "embedding failed, keeping the file as downloaded: ffprobe can't read",
        ));
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(env.media().join(RICK_DIR).join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["embedded"]["metadata"], false);
    assert!(
        meta["embed_error"]
            .as_str()
            .unwrap()
            .contains("Invalid data")
    );
}

/// Write an executable shell script into the test's temp dir.
fn script(env: &Env, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = env.path(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A config with `hooks = [...]` on the default category, plus hook tables.
fn hooks_config(env: &Env, category_extra: &str, hooks: &str) -> String {
    let config = env.config(category_extra);
    let text = fs::read_to_string(&config).unwrap() + "\n" + hooks;
    fs::write(&config, text).unwrap();
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn embed_hooks_rewrite_what_gets_embedded() {
    let env = Env::new().await;
    let Some(bin) = real_ffmpeg_bin(&env) else {
        eprintln!("skipped: ffmpeg not installed");
        return;
    };
    let sample = env.path("sample.mkv");
    assert!(
        std::process::Command::new(bin.join("ffmpeg"))
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x36:rate=10:duration=1")
            .arg(&sample)
            .status()
            .unwrap()
            .success()
    );
    // "Artist - Title" → artist + title; then a second hook, chained, sees
    // the first one's output.
    let split = script(
        &env,
        "split.sh",
        r#"cat > /dev/null
artist="${TIDEPOOL_TITLE%% - *}"; title="${TIDEPOOL_TITLE#* - }"
printf '{"tags": {"title": "%s", "artist": "%s", "genre": null}}' "$title" "$artist""#,
    );
    let shout = script(
        &env,
        "shout.sh",
        r#"input=$(cat)
case "$input" in *'"artist":"Rick Astley"'*) printf '{"tags": {"album_artist": "RICK"}}' ;; esac"#,
    );
    let config = hooks_config(
        &env,
        "hooks = [\"split\", \"shout\"]\n[category.default.embed]\nmetadata = true",
        &format!(
            "[hooks.split]\ntype = \"embed\"\ncommand = [\"{}\"]\n\n[hooks.shout]\ntype = \"embed\"\ncommand = [\"{}\"]\n",
            split.display(),
            shout.display()
        ),
    );
    let mut cmd = env.cmd(&["run", "-s", "dl", "--config", &config]);
    cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("TIDEPOOL_FAKE_MEDIA", &sample);
    cmd.assert().code(3);

    let rick = env.media().join(RICK_DIR);
    let media =
        rick.join("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv");
    let tags = &ffprobe_json(&media)["format"]["tags"];
    assert_eq!(
        tags["title"],
        "Never Gonna Give You Up (Official Video) (4K Remaster)"
    );
    assert_eq!(tags["ARTIST"], "Rick Astley");
    assert_eq!(
        tags["ALBUM_ARTIST"], "RICK",
        "the second hook saw the first one's output"
    );
    assert!(tags.get("GENRE").is_none(), "null removed the tag");
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["embedded_tags"]["artist"], "Rick Astley");
    assert_eq!(
        meta["title"],
        "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn musicbrainz_ids_are_looked_up_from_the_artist() {
    let env = Env::new().await;
    let Some(bin) = real_ffmpeg_bin(&env) else {
        eprintln!("skipped: ffmpeg not installed");
        return;
    };
    let sample = env.path("sample.mkv");
    assert!(
        std::process::Command::new(bin.join("ffmpeg"))
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x36:rate=10:duration=1")
            .arg(&sample)
            .status()
            .unwrap()
            .success()
    );
    // Rick's releases: the 1987 album, and a 2009 compilation.
    let musicbrainz = MockServer::start().await;
    let release = |id: &str, title: &str, date: &str, secondary: &[&str]| {
        serde_json::json!({
            "id": id, "title": title, "date": date,
            "release-group": {"id": format!("{id}-group"), "primary-type": "Album", "secondary-types": secondary},
            "media": [{"tracks": [
                {"id": format!("{id}-track"), "title": "Never Gonna Give You Up", "recording": {"id": "the-recording"}},
            ]}]
        })
    };
    Mock::given(path_regex("/ws/2/release"))
        .and(wiremock::matchers::query_param("artist", "rick-artist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "release-count": 2,
            "releases": [
                release("whenever", "Whenever You Need Somebody", "1987-11-16", &[]),
                release("greatest", "Greatest Hits", "2009", &["Compilation"]),
            ]
        })))
        .expect(1)
        .mount(&musicbrainz)
        .await;
    Mock::given(path_regex("/ws/2/artist/rick-artist"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id": "rick-artist", "name": "Rick Astley (MB)"}),
            ),
        )
        .mount(&musicbrainz)
        .await;
    let config = hooks_config(
        &env,
        &format!(
            "[category.default.embed]\nmetadata = true\ntags = {{ MUSICBRAINZ_ARTISTID = \"rick-artist\" }}\n\
             musicbrainz = {{ server = \"{}\" }}",
            musicbrainz.uri()
        ),
        "",
    );
    let cmd = |args: &[&str]| {
        let mut cmd = env.cmd(args);
        cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("TIDEPOOL_FAKE_MEDIA", &sample);
        cmd
    };
    cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);

    let rick = env.media().join(RICK_DIR);
    let media =
        rick.join("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv");
    let tags = &ffprobe_json(&media)["format"]["tags"];
    assert_eq!(
        tags["MUSICBRAINZ_ALBUMID"], "whenever",
        "not the compilation: {tags}"
    );
    assert_eq!(tags["MUSICBRAINZ_RELEASEGROUPID"], "whenever-group");
    assert_eq!(tags["MUSICBRAINZ_TRACKID"], "the-recording");
    assert_eq!(tags["MUSICBRAINZ_RELEASETRACKID"], "whenever-track");
    // The artist's name, not the channel's.
    assert_eq!(tags["ARTIST"], "Rick Astley (MB)", "{tags}");
    assert_eq!(tags["ALBUM_ARTIST"], "Rick Astley (MB)");
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["musicbrainz"]["release"], "Whenever You Need Somebody");
    let other = env
        .media()
        .join("dl/2026-09-01 - Video okvideo01 [okvideo01]");
    let other: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(other.join("metadata.json")).unwrap()).unwrap();
    assert!(
        other["musicbrainz"]["no_match"]
            .as_str()
            .unwrap()
            .contains("no track called “Video okvideo01”"),
        "{other}"
    );

    // Embedding again keeps the ids, without asking again (one request in
    // all: the mock expects it).
    cmd(&["embed", "dQw4w9WgXcQ"]).assert().success();
    let tags = &ffprobe_json(&media)["format"]["tags"];
    assert_eq!(tags["MUSICBRAINZ_TRACKID"], "the-recording");
}

/// A fake MusicBrainz knowing Rick Astley (`rick-artist`); `artist_status`
/// answers his artist lookup.
async fn fake_musicbrainz(artist_status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path_regex("/ws/2/artist/rick-artist"))
        .respond_with(
            ResponseTemplate::new(artist_status)
                .set_body_json(serde_json::json!({"id": "rick-artist", "name": "Rick Astley"})),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn music_video_titles_are_cleaned_up_before_anything_else() {
    let env = Env::new().await;
    let musicbrainz = fake_musicbrainz(200).await;
    let config = hooks_config(
        &env,
        &format!(
            "clean_title = \"music_video\"\n[category.default.embed]\n\
             tags = {{ MUSICBRAINZ_ARTISTID = \"rick-artist\" }}\n\
             musicbrainz = {{ server = \"{}\", enabled = false }}",
            musicbrainz.uri()
        ),
        "",
    );
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let dir = env
        .media()
        .join("dl/2009-10-25 - Never Gonna Give You Up [dQw4w9WgXcQ]");
    assert!(
        dir.join("Never Gonna Give You Up.mkv").is_file(),
        "named after the song: {:?}",
        fs::read_dir(env.media().join("dl"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["title"], "Never Gonna Give You Up");
    assert_eq!(
        meta["original"]["title"],
        "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"
    );
    // The other video's title has nothing to clean.
    assert!(
        env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]")
            .is_dir()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn files_in_a_sources_backfill_folder_are_imported() {
    let env = Env::new().await;
    let musicbrainz = fake_musicbrainz(200).await;
    let config = env.path("config.toml");
    fs::write(
        &config,
        format!(
            "[category.default]\nmedia_dir = \"{}\"\nclean_title = \"music_video\"\n\
             [category.default.embed]\ntags = {{ MUSICBRAINZ_ARTISTID = \"rick-artist\" }}\n\
             musicbrainz = {{ server = \"{}\", enabled = false }}\n\n\
             [tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n\n\
             [sources.rick]\nurls = []\n",
            env.media().display(),
            musicbrainz.uri(),
            test_bin().join("yt-dlp").display()
        ),
    )
    .unwrap();
    let config = config.display().to_string();
    let folder = env.path("data/tidepool/backfill/rick");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("Rick Astley - Never Gonna Give You Up (Official Video).mp4"),
        "video",
    )
    .unwrap();
    fs::write(folder.join("notes.txt"), "not a video").unwrap();
    // Only a source's own folder: not the one above.
    fs::write(env.path("data/tidepool/backfill/elsewhere.mp4"), "x").unwrap();

    env.cmd(&["run", "--config", &config])
        .assert()
        .success()
        .stdout(contains(
            "rick: imported Rick Astley - Never Gonna Give You Up (Official Video).mp4 from the backfill folder",
        ));
    let placed: Vec<PathBuf> = walk(&env.media().join("rick"));
    let media = placed
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "mp4"))
        .unwrap_or_else(|| panic!("placed: {placed:?}"));
    assert_eq!(
        media.file_name().unwrap().to_string_lossy(),
        "Never Gonna Give You Up.mp4"
    );
    assert_eq!(fs::read_to_string(media).unwrap(), "video");
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(media.parent().unwrap().join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["title"], "Never Gonna Give You Up");
    assert_eq!(meta["channel"]["name"], "Rick Astley");
    assert_eq!(
        fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>(),
        ["notes.txt"]
    );
    assert!(env.path("data/tidepool/backfill/elsewhere.mp4").exists());
    let statuses = env.statuses("rick");
    assert_eq!(statuses.len(), 1);
    assert_eq!((statuses[0].0.len(), statuses[0].1.as_str()), (11, "done"));

    // Still there on the next run: a source without URLs has nothing to lose.
    env.cmd(&["run", "--config", &config])
        .assert()
        .success()
        .stdout(contains("rick: 1 video: 1 already downloaded"));
    assert_eq!(env.statuses("rick"), statuses);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backfilled_file_named_after_a_listed_video_is_used_for_it() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["index", "--config", &config]).assert().success();
    let folder = env.path("data/tidepool/backfill/dl");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("my copy [dQw4w9WgXcQ].mkv"), "mine").unwrap();
    env.cmd(&["download", "-s", "dl"]).assert().code(3).stdout(
        contains("dl: imported my copy [dQw4w9WgXcQ].mkv from the backfill folder")
            // Finished from the file (4 bytes), not downloaded.
            .and(contains("(4K Remaster)” (4 B)")),
    );
    // YouTube's details, the file from the folder.
    let media = env
        .media()
        .join(RICK_DIR)
        .join("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv");
    assert_eq!(fs::read_to_string(media).unwrap(), "mine");
    assert_eq!(env.statuses("dl").len(), 4, "no new video");
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn titles_of_videos_already_downloaded_are_cleaned_up_on_request() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let old = env.media().join(RICK_DIR);
    assert!(old.is_dir());

    let musicbrainz = fake_musicbrainz(200).await;
    let config = hooks_config(
        &env,
        &format!(
            "clean_title = \"music_video\"\n[category.default.embed]\n\
             tags = {{ MUSICBRAINZ_ARTISTID = \"rick-artist\" }}\n\
             musicbrainz = {{ server = \"{}\" }}",
            musicbrainz.uri()
        ),
        "",
    );
    env.cmd(&["config", "import", &config]).assert().success();
    env.cmd(&["clean-titles", "--all", "--dry-run"])
        .assert()
        .success()
        .stdout(
            contains("“Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” → “Never Gonna Give You Up” (dry run)")
                .and(contains("would rename 1 video; 1 already clean")),
        );
    assert!(old.is_dir(), "a dry run moves nothing");

    env.cmd(&["clean-titles", "--all"])
        .assert()
        .success()
        .stdout(contains("renamed 1 video; 1 already clean"));
    let new = env
        .media()
        .join("dl/2009-10-25 - Never Gonna Give You Up [dQw4w9WgXcQ]");
    assert!(new.join("Never Gonna Give You Up.mkv").is_file());
    assert!(new.join("Never Gonna Give You Up-poster.jpg").is_file());
    assert!(new.join("Never Gonna Give You Up.en.vtt").is_file());
    assert!(!old.exists(), "the old folder is gone");
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(new.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["title"], "Never Gonna Give You Up");
    assert_eq!(
        meta["original"]["title"],
        "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"
    );
    assert_eq!(meta["files"]["media"], "Never Gonna Give You Up.mkv");
    // The stored paths follow: nothing counts as missing.
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains("missing").not());
    env.cmd(&["clean-titles", "--all"])
        .assert()
        .success()
        .stdout(contains("renamed 0 videos; 2 already clean"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cleaning_up_a_title_never_overwrites_another_file() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let musicbrainz = fake_musicbrainz(200).await;
    let config = hooks_config(
        &env,
        &format!(
            "clean_title = \"music_video\"\n[category.default.embed]\n\
             tags = {{ MUSICBRAINZ_ARTISTID = \"rick-artist\" }}\n\
             musicbrainz = {{ server = \"{}\" }}",
            musicbrainz.uri()
        ),
        "",
    );
    env.cmd(&["config", "import", &config]).assert().success();
    let taken = env
        .media()
        .join("dl/2009-10-25 - Never Gonna Give You Up [dQw4w9WgXcQ]/Never Gonna Give You Up.mkv");
    fs::create_dir_all(taken.parent().unwrap()).unwrap();
    fs::write(&taken, "someone else's").unwrap();
    env.cmd(&["clean-titles", "dQw4w9WgXcQ"])
        .assert()
        .code(3)
        .stdout(contains("skipped:").and(contains("already exists")));
    assert_eq!(fs::read_to_string(&taken).unwrap(), "someone else's");
    assert!(env.media().join(RICK_DIR).is_dir(), "left where it was");
}

#[tokio::test(flavor = "multi_thread")]
async fn sources_with_downloads_off_are_only_checked() {
    let env = Env::new().await;
    let config = env.config("download = false");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .success()
        .stdout(
            contains("dl: 4 videos (4 new): 4 to download")
                .and(contains(
                    "dl: downloads are off (download = false); only checked",
                ))
                .and(contains("nothing to download")),
        );
    assert!(!env.media().join("dl").exists());
    assert!(env.statuses("dl").iter().all(|(_, s, _)| s == "pending"));

    // Turned back on: what's waiting downloads.
    let config = env.config("");
    env.cmd(&["download", "--config", &config])
        .assert()
        .code(3)
        .stdout(contains("dl: downloaded “Rick Astley"));
}

#[tokio::test(flavor = "multi_thread")]
async fn titles_without_an_artist_id_stay_as_they_are() {
    let env = Env::new().await;
    let config = hooks_config(&env, "clean_title = \"music_video\"", "");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    assert!(env.media().join(RICK_DIR).is_dir(), "YouTube's title");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_isnt_cleaned_up_without_musicbrainz_so_the_video_waits() {
    let env = Env::new().await;
    let musicbrainz = fake_musicbrainz(500).await;
    let config = hooks_config(
        &env,
        &format!(
            "clean_title = \"music_video\"\n[category.default.embed]\n\
             tags = {{ MUSICBRAINZ_ARTISTID = \"rick-artist\" }}\n\
             musicbrainz = {{ server = \"{}\" }}",
            musicbrainz.uri()
        ),
        "",
    );
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3)
        .stderr(contains("cannot clean up the title"));
    assert!(!env.media().join(RICK_DIR).exists());
    let rick = env
        .statuses("dl")
        .into_iter()
        .find(|(id, _, _)| id == "dQw4w9WgXcQ")
        .unwrap();
    assert_eq!(
        (rick.1.as_str(), rick.2),
        ("failed", 1),
        "tried again later"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn downloaded_hooks_can_replace_the_media() {
    let env = Env::new().await;
    let remux = script(
        &env,
        "remux.sh",
        r#"cat > /dev/null
new="${TIDEPOOL_OUTPUT_FILE%.mkv}.mp4"
mv "$TIDEPOOL_OUTPUT_FILE" "$new"
printf '{"files": {"media": "%s"}}' "$new""#,
    );
    let config = hooks_config(
        &env,
        "hooks = [\"remux\"]",
        &format!(
            "[hooks.remux]\ntype = \"downloaded\"\ncommand = [\"{}\"]\n",
            remux.display()
        ),
    );
    env.cmd(&["run", "--config", &config]).assert().code(3);
    let rick = env.media().join(RICK_DIR);
    let title = "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)";
    assert!(
        rick.join(format!("{title}.mp4")).is_file(),
        "{{ext}} follows the new file"
    );
    assert!(!rick.join(format!("{title}.mkv")).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_embed_hooks_fail_the_download_unless_warn() {
    let env = Env::new().await;
    let broken = script(&env, "broken.sh", "echo 'no such tag database' >&2; exit 2");
    let hooks = |on_failure: &str| {
        format!(
            "[hooks.broken]\ntype = \"embed\"\non_failure = \"{on_failure}\"\ncommand = [\"{}\"]\n",
            broken.display()
        )
    };
    let config = hooks_config(
        &env,
        "hooks = [\"broken\"]\n[category.default.embed]\nmetadata = true",
        &hooks("fail"),
    );
    env.cmd(&["run", "--config", &config])
        .assert()
        .code(3)
        .stderr(contains(
            "hook broken failed: exited with exit status: 2: no such tag database",
        ));
    assert_eq!(
        env.statuses("dl")[0],
        ("dQw4w9WgXcQ".into(), "failed".into(), 1)
    );
    assert!(!env.media().join(RICK_DIR).exists(), "nothing placed");
    let log = fs::read_to_string(env.path("data/tidepool/logs/hooks/dQw4w9WgXcQ.log")).unwrap();
    assert!(
        log.contains("hook broken (embed) ==\nno such tag database"),
        "{log}"
    );

    // warn: downloaded anyway, without the hook's changes.
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("\"fail\"", "\"warn\"");
    fs::write(&config, text).unwrap();
    env.cmd(&["reset", "dQw4w9WgXcQ", "--config", &config])
        .assert()
        .success();
    env.cmd(&["download"])
        .assert()
        .stderr(contains("hook broken failed").and(contains("continuing without it")));
    assert_eq!(env.statuses("dl")[0].1, "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_finished_hooks_keep_the_video_and_run_again() {
    let env = Env::new().await;
    let flag = env.path("fixed");
    let notify = script(
        &env,
        "notify.sh",
        &format!(
            "[ -f '{}' ] || {{ echo 'server down' >&2; exit 1; }}\necho \"$TIDEPOOL_OUTPUT_FILE\" >> '{}'",
            flag.display(),
            env.path("notified").display()
        ),
    );
    let config = hooks_config(
        &env,
        "hooks = [\"notify\"]",
        &format!("[hooks.notify]\ncommand = [\"{}\"]\n", notify.display()),
    );
    env.cmd(&["run", "-v", "--config", &config])
        .assert()
        .code(3)
        .stdout(contains("2 hooks failed"))
        .stderr(
            contains("hook notify failed: exited with exit status: 1: server down")
                .and(contains("hook notify: server down")),
        );
    assert_eq!(
        env.statuses("dl")[0].1,
        "done",
        "the video stays downloaded"
    );
    env.cmd(&["problems"])
        .assert()
        .stdout(contains("a hook is failing").and(contains("tidepool hooks run")));

    fs::write(&flag, "").unwrap();
    env.cmd(&["hooks", "run", "--all", "--failed"])
        .assert()
        .success()
        .stdout(
            contains("dl: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”: ran notify")
                .and(contains("ran hooks for 2 videos")),
        );
    let notified = fs::read_to_string(env.path("notified")).unwrap();
    assert!(
        notified
            .contains("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv"),
        "{notified}"
    );
    env.cmd(&["hooks", "run", "--all", "--failed"])
        .assert()
        .success()
        .stdout(contains("no hooks to run for the chosen videos"));
    env.cmd(&["problems"])
        .assert()
        .stdout(contains("no open problems").and(contains("a hook is failing (resolved")));
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_run_again_on_downloaded_videos() {
    let env = Env::new().await;
    let Some(bin) = real_ffmpeg_bin(&env) else {
        eprintln!("skipped: ffmpeg not installed");
        return;
    };
    let sample = env.path("sample.mkv");
    assert!(
        std::process::Command::new(bin.join("ffmpeg"))
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x36:rate=10:duration=1")
            .arg(&sample)
            .status()
            .unwrap()
            .success()
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let config = env.config("[category.default.embed]\nmetadata = true");
    let text = fs::read_to_string(&config).unwrap()
        + "\n[sources.dl2]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads2\"]\n";
    fs::write(&config, &text).unwrap();
    let cmd = |args: &[&str]| {
        let mut cmd = env.cmd(args);
        cmd.env("PATH", &path).env("TIDEPOOL_FAKE_MEDIA", &sample);
        cmd
    };
    cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    cmd(&["run", "-s", "dl2"]).assert().success();
    let okvideo = |source: &str| {
        env.media()
            .join(format!("{source}/2026-09-01 - Video okvideo01 [okvideo01]"))
    };
    let (a, b) = (
        okvideo("dl").join("Video okvideo01.mkv"),
        okvideo("dl2").join("Video okvideo01.mkv"),
    );
    assert_eq!(
        fs::metadata(&a).unwrap().ino(),
        fs::metadata(&b).unwrap().ino()
    );
    assert_eq!(
        ffprobe_json(&a)["format"]["tags"]["title"],
        "Video okvideo01"
    );

    // Hooks added after the download: an embed hook and a finished one.
    let rename = script(
        &env,
        "rename.sh",
        r#"cat > /dev/null; printf '{"tags": {"title": "Renamed: %s"}}' "$TIDEPOOL_VIDEO_ID""#,
    );
    let log = env.path("finished.log");
    let finished = script(
        &env,
        "finished.sh",
        &format!(
            "echo \"$TIDEPOOL_SOURCE $TIDEPOOL_OUTPUT_FILE\" >> '{}'",
            log.display()
        ),
    );
    let text = text.replace(
        "[category.default.embed]",
        "hooks = [\"rename\", \"finished\"]\n[category.default.embed]",
    ) + &format!(
        "\n[hooks.rename]\ntype = \"embed\"\ncommand = [\"{}\"]\n\n[hooks.finished]\ncommand = [\"{}\"]\n",
        rename.display(),
        finished.display()
    );
    fs::write(&config, &text).unwrap();
    cmd(&["config", "import", &config]).assert().success();

    // Nothing picked: an error, not "every video".
    cmd(&["hooks", "run"])
        .assert()
        .code(1)
        .stderr(contains("pick the videos"));
    cmd(&["hooks", "run", "--all", "--hook", "nope"])
        .assert()
        .code(1)
        .stderr(contains(
            "there's no hook \"nope\" (hooks: finished, rename)",
        ));

    // Only one source, only the finished hook: files untouched.
    cmd(&["hooks", "run", "-s", "dl2", "--hook", "finished"])
        .assert()
        .success()
        .stdout(
            contains("dl2: “Video okvideo01”: ran finished").and(contains("ran hooks for 1 video")),
        );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("dl2 {}\n", b.display())
    );
    assert_eq!(
        ffprobe_json(&a)["format"]["tags"]["title"],
        "Video okvideo01"
    );

    // Every video of the default category: re-embedded with the hook's tags.
    cmd(&["hooks", "run", "--category", "default"])
        .assert()
        .success()
        .stdout(
            contains("dl: “Video okvideo01”: ran rename, finished")
                .and(contains("ran hooks for 3 videos")),
        );
    assert_eq!(
        ffprobe_json(&a)["format"]["tags"]["title"],
        "Renamed: okvideo01"
    );
    assert_eq!(
        fs::metadata(&a).unwrap().ino(),
        fs::metadata(&b).unwrap().ino(),
        "the linked copy follows (last wins)"
    );
    let streams = ffprobe_json(&a)["streams"].as_array().unwrap().len();
    cmd(&["hooks", "run", "okvideo01", "--hook", "rename"])
        .assert()
        .success();
    assert_eq!(
        ffprobe_json(&a)["streams"].as_array().unwrap().len(),
        streams,
        "running again doesn't pile up streams"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(okvideo("dl").join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(meta["embedded_tags"]["title"], "Renamed: okvideo01");
    assert_eq!(
        meta["title"], "Video okvideo01",
        "the video's own title is kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_run_can_convert_downloaded_files() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let remux = script(
        &env,
        "remux.sh",
        r#"cat > /dev/null
new="${TIDEPOOL_OUTPUT_FILE%.mkv}.mp4"
mv "$TIDEPOOL_OUTPUT_FILE" "$new"
printf '{"files": {"media": "%s"}}' "$new""#,
    );
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("[tools.deno]", "hooks = [\"remux\"]\n\n[tools.deno]")
        + &format!(
            "\n[hooks.remux]\ntype = \"downloaded\"\ncommand = [\"{}\"]\n",
            remux.display()
        );
    fs::write(&config, text).unwrap();
    env.cmd(&["config", "import", &config]).assert().success();
    env.cmd(&["hooks", "run", "dQw4w9WgXcQ"])
        .assert()
        .success()
        .stdout(contains("ran remux"));
    let rick = env.media().join(RICK_DIR);
    let title = "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)";
    assert!(rick.join(format!("{title}.mp4")).is_file());
    assert!(
        !rick.join(format!("{title}.mkv")).exists(),
        "the old file is gone"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["files"]["media"], format!("{title}.mp4"));
    // The stored location follows: reset --delete-files finds the new file.
    env.cmd(&["reset", "dQw4w9WgXcQ", "--delete-files"])
        .assert()
        .success()
        .stdout(contains(format!("{title}.mp4")));
}

#[tokio::test(flavor = "multi_thread")]
async fn ytdlp_gets_the_configured_deno() {
    let env = Env::new().await;
    let log = env.path("ytdlp.log");
    // Your own Deno at a path: every yt-dlp call is told about it.
    let config = env.config("");
    let text = fs::read_to_string(&config).unwrap().replace(
        "[tools.deno]\nmanaged = false",
        "[tools.deno]\nmanaged = false\npath = \"/opt/deno/bin/deno\"",
    );
    fs::write(&config, &text).unwrap();
    let mut cmd = env.cmd(&["run", "-s", "dl", "--config", &config]);
    cmd.env("TIDEPOOL_FAKE_YTDLP_LOG", &log);
    cmd.assert().code(3);
    let calls = fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().filter(|l| !l.contains("--version")).collect();
    assert!(!calls.is_empty());
    for call in &calls {
        assert!(
            call.starts_with("--js-runtimes deno:/opt/deno/bin/deno "),
            "{call}"
        );
    }

    // `path = "deno"` (the default): yt-dlp finds it on PATH by itself.
    fs::write(&config, text.replace("path = \"/opt/deno/bin/deno\"", "")).unwrap();
    fs::remove_file(&log).unwrap();
    env.cmd(&["reset", "okvideo01", "--config", &config])
        .assert()
        .success();
    let mut cmd = env.cmd(&["download"]);
    cmd.env("TIDEPOOL_FAKE_YTDLP_LOG", &log);
    cmd.assert().success();
    let calls = fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("--js-runtimes"), "{calls}");
}

#[tokio::test(flavor = "multi_thread")]
async fn prepare_hooks_change_details_and_paths() {
    let env = Env::new().await;
    let artist = env.media().join("Rick Astley");
    // Rick's: a cleaner title and channel, and an artist folder; a second
    // hook, chained, sees the first one's result.
    let tidy = script(
        &env,
        "tidy.sh",
        &format!(
            r#"cat > /dev/null
[ "$TIDEPOOL_VIDEO_ID" = dQw4w9WgXcQ ] || exit 0
printf '{{"details": {{"title": "Never Gonna Give You Up", "channel": "Rick", "tags": null}}, "paths": {{"output_dir": "{}"}}}}'"#,
            artist.display()
        ),
    );
    let seen = env.path("seen.json");
    let peek = script(
        &env,
        "peek.sh",
        &format!(
            r#"input=$(cat)
[ "$TIDEPOOL_VIDEO_ID" = dQw4w9WgXcQ ] && printf '%s' "$input" > '{}'
exit 0"#,
            seen.display()
        ),
    );
    let config = hooks_config(
        &env,
        "hooks = [\"tidy\", \"peek\"]",
        &format!(
            "[hooks.tidy]\ntype = \"prepare\"\ncommand = [\"{}\"]\n\n[hooks.peek]\ntype = \"prepare\"\ncommand = [\"{}\"]\n",
            tidy.display(),
            peek.display()
        ),
    );
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3)
        .stdout(contains(
            "dl: downloaded “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”",
        ));

    let media = artist.join("Never Gonna Give You Up.mkv");
    assert!(media.is_file(), "the title and folder it set");
    assert!(artist.join("Never Gonna Give You Up.en.vtt").is_file());
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(artist.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["title"], "Never Gonna Give You Up");
    assert_eq!(meta["channel"]["name"], "Rick");
    assert_eq!(meta["tags"], serde_json::json!([]));
    assert_eq!(
        meta["original"]["title"],
        "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"
    );
    assert_eq!(meta["original"]["channel"], "Rick Astley");
    assert!(
        meta["original"].get("upload_date").is_none(),
        "only changes"
    );

    let seen: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(seen["details"]["title"], "Never Gonna Give You Up");
    assert_eq!(seen["context"]["video"]["title"], "Never Gonna Give You Up");
    assert_eq!(
        seen["paths"]["output_file"],
        artist
            .join("Never Gonna Give You Up.{ext}")
            .display()
            .to_string()
    );
    // Others are untouched.
    assert!(
        env.media()
            .join("dl/2026-09-01 - Video okvideo01 [okvideo01]")
            .is_dir()
    );

    // Only for new downloads.
    env.cmd(&["hooks", "run", "--all", "--hook", "tidy"])
        .assert()
        .code(1)
        .stderr(contains("it only runs for new downloads"));
    env.cmd(&["hooks", "run", "--all"])
        .assert()
        .success()
        .stdout("no hooks to run for the chosen videos\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_prepare_output_fails_the_download_unless_warn() {
    let env = Env::new().await;
    let escape = script(
        &env,
        "escape.sh",
        r#"cat > /dev/null
printf '{"paths": {"output_dir": "/tmp/elsewhere"}}'"#,
    );
    let hooks = |on_failure: &str| {
        format!(
            "[hooks.escape]\ntype = \"prepare\"\non_failure = \"{on_failure}\"\ncommand = [\"{}\"]\n",
            escape.display()
        )
    };
    let config = hooks_config(&env, "hooks = [\"escape\"]", &hooks("fail"));
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3)
        .stderr(contains("must be inside the media folder"));
    assert!(!env.media().join(RICK_DIR).exists(), "nothing placed");
    assert!(!Path::new("/tmp/elsewhere").exists());

    let config = hooks_config(&env, "hooks = [\"escape\"]", &hooks("warn"));
    env.cmd(&["reset", "dQw4w9WgXcQ", "--config", &config])
        .assert()
        .success();
    env.cmd(&["download", "-s", "dl"]).assert().success();
    assert!(env.media().join(RICK_DIR).is_dir(), "continued without it");
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_hook_output_is_ignored() {
    let env = Env::new().await;
    let hook = script(
        &env,
        "liar.sh",
        r#"cat > /dev/null
printf '{"files": {"media": null, "cover": null}}'"#,
    );
    let config = hooks_config(
        &env,
        "hooks = [\"liar\"]",
        &format!("[hooks.liar]\ncommand = [\"{}\"]\n", hook.display()),
    );
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let rick = env.media().join(RICK_DIR);
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert!(meta["files"]["media"].is_string());
    assert!(meta["files"]["cover"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_files_are_reported_or_restored() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    let rick = env.media().join(RICK_DIR);
    let title = "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)";
    let (media, cover, subs, meta) = (
        rick.join(format!("{title}.mkv")),
        rick.join(format!("{title}-poster.jpg")),
        rick.join(format!("{title}.en.vtt")),
        rick.join("metadata.json"),
    );
    let original_meta = fs::read_to_string(&meta).unwrap();
    for file in [&cover, &subs, &meta] {
        fs::remove_file(file).unwrap();
    }

    // Off (the default): reported, nothing touched.
    env.cmd(&["download"]).assert().success().stdout(contains(
        "1 video has missing files; set redownload_missing = true to restore them",
    ));
    assert!(!cover.exists() && !subs.exists() && !meta.exists());

    // On: only what's missing comes back.
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("[tools.deno]", "redownload_missing = true\n\n[tools.deno]");
    fs::write(&config, text).unwrap();
    env.cmd(&["config", "import", &config]).assert().success();
    let media_inode = fs::metadata(&media).unwrap().ino();
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains("restored 3 missing files"));
    assert!(cover.is_file());
    assert_eq!(
        fs::read_to_string(&subs).unwrap(),
        "WEBVTT\n\n00:00.000 --> 00:00.500\nRestored\n"
    );
    assert_eq!(fs::read_to_string(&meta).unwrap(), original_meta);
    assert_eq!(
        fs::metadata(&media).unwrap().ino(),
        media_inode,
        "the video wasn't downloaded again"
    );

    // The video itself: downloaded again.
    fs::remove_file(&media).unwrap();
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains(format!("dl: downloaded “{title}”")));
    assert!(media.is_file());
}

/// Start tidepool in the background, the way a terminal would.
fn spawn_tidepool(env: &Env, args: &[&str], vars: &[(&str, &str)]) -> std::process::Child {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tidepool"));
    cmd.env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", test_bin().display()))
        .env("HOME", env.path("home"))
        .env("XDG_CONFIG_HOME", env.path("config"))
        .env("XDG_DATA_HOME", env.path("data"))
        .env("TIDEPOOL_TEST_THUMBNAIL_BASE", env.thumbs.uri())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (key, value) in vars {
        cmd.env(key, value);
    }
    cmd.spawn().unwrap()
}

fn signal_int(child: &std::process::Child) {
    std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
}

/// The pids the fake yt-dlp recorded (itself and its sleeping child).
fn recorded_pids(file: &Path) -> Vec<i32> {
    fs::read_to_string(file)
        .unwrap_or_default()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn wait_for(what: &str, check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// A config whose processes get `grace` to stop.
fn config_with_grace(env: &Env, grace: &str) -> String {
    let config = env.config("");
    let text = format!("[general]\nshutdown_timeout = \"{grace}\"\n\n")
        + &fs::read_to_string(&config).unwrap();
    fs::write(&config, text).unwrap();
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_ctrl_c_stops_right_away() {
    let env = Env::new().await;
    let config = config_with_grace(&env, "60s");
    env.cmd(&["index", "--config", &config]).assert().code(0);
    let pids = env.path("pids");
    let pids_arg = pids.display().to_string();
    // yt-dlp ignores the polite request, so the first Ctrl-C waits for it.
    let mut child = spawn_tidepool(
        &env,
        &["download", "--limit", "1"],
        &[
            ("TIDEPOOL_FAKE_YTDLP_SLEEP", "30"),
            ("TIDEPOOL_FAKE_YTDLP_STUBBORN", "1"),
            ("TIDEPOOL_FAKE_YTDLP_PIDS", &pids_arg),
        ],
    );
    wait_for("the download to start", || recorded_pids(&pids).len() == 2);
    signal_int(&child);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "still giving yt-dlp time"
    );
    let started = std::time::Instant::now();
    signal_int(&child);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    for pid in recorded_pids(&pids) {
        assert!(!alive(pid), "yt-dlp and what it started are gone ({pid})");
    }
    assert_eq!(env.statuses("dl")[0].1, "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_gives_processes_the_shutdown_timeout() {
    let env = Env::new().await;
    let config = config_with_grace(&env, "1s");
    env.cmd(&["index", "--config", &config]).assert().code(0);
    let pids = env.path("pids");
    let pids_arg = pids.display().to_string();
    let mut child = spawn_tidepool(
        &env,
        &["download", "--limit", "1"],
        &[
            ("TIDEPOOL_FAKE_YTDLP_SLEEP", "30"),
            ("TIDEPOOL_FAKE_YTDLP_STUBBORN", "1"),
            ("TIDEPOOL_FAKE_YTDLP_PIDS", &pids_arg),
        ],
    );
    wait_for("the download to start", || recorded_pids(&pids).len() == 2);
    let started = std::time::Instant::now();
    signal_int(&child);
    let status = child.wait().unwrap();
    let took = started.elapsed();
    assert_eq!(status.code(), Some(130));
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(6),
        "{took:?}"
    );
    for pid in recorded_pids(&pids) {
        assert!(!alive(pid), "killed after the timeout ({pid})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_while_checking_sources() {
    let env = Env::new().await;
    let config = config_with_grace(&env, "10s");
    env.cmd(&["config", "import", &config]).assert().success();
    let pids = env.path("pids");
    let pids_arg = pids.display().to_string();
    let child = spawn_tidepool(
        &env,
        &["run"],
        &[
            ("TIDEPOOL_FAKE_INDEX_SLEEP", "30"),
            ("TIDEPOOL_FAKE_YTDLP_PIDS", &pids_arg),
        ],
    );
    wait_for("the listing to start", || recorded_pids(&pids).len() == 2);
    let started = std::time::Instant::now();
    signal_int(&child);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("stopped; the rest will be checked on the next run")
    );
    for pid in recorded_pids(&pids) {
        assert!(!alive(pid), "nothing left running ({pid})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn embed_applies_the_current_settings_to_downloaded_videos() {
    let env = Env::new().await;
    let Some(bin) = real_ffmpeg_bin(&env) else {
        eprintln!("skipped: ffmpeg not installed");
        return;
    };
    let sample = env.path("sample.mkv");
    assert!(
        std::process::Command::new(bin.join("ffmpeg"))
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x36:rate=10:duration=1")
            .args(["-f", "lavfi", "-i", "sine=duration=1"])
            .arg(&sample)
            .status()
            .unwrap()
            .success()
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let run = |args: &[&str]| {
        let mut cmd = env.cmd(args);
        cmd.env("PATH", &path).env("TIDEPOOL_FAKE_MEDIA", &sample);
        cmd
    };
    // Downloaded with nothing embedded.
    let config = env.config("");
    run(&["run", "-s", "dl", "--config", &config])
        .assert()
        .code(3);
    run(&["embed"])
        .assert()
        .failure()
        .stderr(contains("pick the videos"));
    run(&["embed", "dQw4w9WgXcQ"]).assert().success().stdout(contains(
        "dl: nothing to embed (every embed option is off; --remove-disabled takes out what's in the files)",
    ));

    // Turned on later, with an embed hook: backfilled.
    let upper = script(
        &env,
        "upper.sh",
        r#"cat > /dev/null; printf '{"tags": {"title": "LOUD"}}'"#,
    );
    let config = hooks_config(
        &env,
        "hooks = [\"upper\"]\n[category.default.embed]\nmetadata = true\ncover = true\nchapters = true\n\
         tags = { MUSICBRAINZ_ARTISTID = \"abc-123\", artist = \"{channel}\", album_artist = \"{channel} (YouTube)\", GENRE = \"\" }",
        &format!(
            "[hooks.upper]\ntype = \"embed\"\ncommand = [\"{}\"]\n",
            upper.display()
        ),
    );
    run(&["embed", "dQw4w9WgXcQ", "--config", &config])
        .assert()
        .success()
        .stdout(contains(
            "dl: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”: embedded tags, cover, chapters",
        ))
        .stdout(contains("embedded 1 video"));
    let rick = env.media().join(RICK_DIR);
    let media =
        rick.join("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv");
    // Covers, and the video tracks a cover must not turn into.
    let covers = |json: &serde_json::Value| {
        json["streams"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["tags"]["mimetype"] == "image/jpeg")
            .count()
    };
    let video_tracks = |json: &serde_json::Value| {
        json["streams"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
            .count()
    };
    let json = ffprobe_json(&media);
    let tags = &json["format"]["tags"];
    assert_eq!(tags["title"], "LOUD", "the embed hook ran");
    assert_eq!(tags["MUSICBRAINZ_ARTISTID"], "abc-123", "embed.tags added");
    assert_eq!(
        tags["ALBUM_ARTIST"], "Rick Astley (YouTube)",
        "with placeholders"
    );
    assert!(tags.get("GENRE").is_none(), "an empty value removes it");
    assert_eq!(covers(&json), 1);
    assert!(!json["chapters"].as_array().unwrap().is_empty());

    // Again: one cover, not two.
    run(&["embed", "dQw4w9WgXcQ"]).assert().success();
    assert_eq!(covers(&ffprobe_json(&media)), 1, "covers don't pile up");

    // Turned off: kept unless asked to remove.
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("cover = true", "cover = false");
    fs::write(&config, text).unwrap();
    run(&["embed", "dQw4w9WgXcQ", "--config", &config])
        .assert()
        .success();
    let json = ffprobe_json(&media);
    assert_eq!(covers(&json), 1, "kept, still an attachment");
    assert_eq!(video_tracks(&json), 1, "not a second video track");
    run(&["embed", "--all", "--remove-disabled"])
        .assert()
        .success()
        .stdout(contains("embedded tags, chapters; removed cover"));
    let json = ffprobe_json(&media);
    assert_eq!(covers(&json), 0, "the cover was taken out");
    assert!(
        !json["chapters"].as_array().unwrap().is_empty(),
        "chapters stay"
    );
    assert_eq!(
        json["streams"].as_array().unwrap().len(),
        2,
        "video and audio"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rick.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(
        meta["embedded"],
        serde_json::json!({ "metadata": true, "cover": false, "chapters": true, "subtitles": false })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_finished_hooks_are_tried_again_until_they_give_up() {
    let env = Env::new().await;
    let flag = env.path("fixed");
    let notify = script(
        &env,
        "notify.sh",
        &format!(
            "[ -f '{}' ] || {{ echo 'server down' >&2; exit 1; }}",
            flag.display()
        ),
    );
    let config = hooks_config(
        &env,
        "hooks = [\"notify\"]",
        &format!("[hooks.notify]\ncommand = [\"{}\"]\n", notify.display()),
    );
    env.cmd(&["run", "--config", &config]).assert().code(3);
    let db = || rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
    let rick = "dQw4w9WgXcQ";
    let state = |video: &str| -> (String, i64, Option<String>) {
        db().query_row(
            "SELECT status, retries, next_attempt_at FROM hook_runs WHERE video_id = ?1",
            [video],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    let (status, retries, next) = state(rick);
    assert_eq!((status.as_str(), retries), ("retrying", 0));
    assert!(next.is_some());

    // Not due yet: left alone.
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains("nothing to download").and(contains("tried again").not()));

    // Due: tried again, and counted.
    let due = || {
        db().execute(
            "UPDATE hook_runs SET next_attempt_at = '2000-01-01T00:00:00Z' WHERE status = 'retrying'",
            [],
        )
        .unwrap();
    };
    due();
    env.cmd(&["download"])
        .assert()
        .code(3)
        .stdout(
            contains("dl: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”: tried again: hook notify failed")
                .and(contains("nothing to download; 2 hooks failed")),
        );
    assert_eq!(state(rick).1, 1);

    // Retries 2 to 7 fail too; the 8th is the last.
    for _ in 2..8 {
        due();
        env.cmd(&["download"]).assert().code(3);
    }
    due();
    env.cmd(&["download"]).assert().code(3).stdout(contains(
        "(no more retries for notify; `tidepool hooks run dQw4w9WgXcQ --failed` tries again)",
    ));
    let (status, retries, next) = state(rick);
    assert_eq!((status.as_str(), retries, next), ("failed", 8, None));
    due();
    env.cmd(&["download"])
        .assert()
        .success()
        .stdout(contains("tried again").not());
    let attempts: i64 = db()
        .query_row(
            "SELECT COUNT(*) FROM hook_attempts WHERE video_id = ?1",
            [rick],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 9, "the first run and 8 retries, each kept");

    // Fixed: `hooks run --failed` picks up the ones that gave up.
    fs::write(&flag, "").unwrap();
    env.cmd(&["hooks", "run", "--all", "--failed"])
        .assert()
        .success()
        .stdout(contains("ran hooks for 2 videos"));
    assert_eq!(state(rick).0, "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_moves_videos_to_the_trash_and_back() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "--config", &config]).assert().code(3);
    let done = |env: &Env| {
        env.statuses("dl")
            .into_iter()
            .filter(|(_, s, _)| s == "done")
            .count()
    };
    assert_eq!(done(&env), 2);
    env.cmd(&["cleanup"])
        .assert()
        .success()
        .stdout(contains("nothing to clean up"));

    // One video at most, keeping its metadata.
    let config = hooks_config(&env, "[category.default.cleanup]\nmax_items = 1", "");
    env.cmd(&["cleanup", "--dry-run", "--config", &config])
        .assert()
        .success()
        .stdout(
            contains("would remove (beyond max_items = 1)")
                .and(contains("dl: 2 videos: 1 would be removed ("))
                .and(contains("), 1 stay ("))
                .and(contains("would remove 1 video")),
        );
    assert_eq!(done(&env), 2, "a dry run changes nothing");
    let out = env
        .cmd(&["cleanup", "--config", &config])
        .assert()
        .success()
        .stdout(contains("to the trash (beyond max_items = 1)").and(contains("removed 1 video")))
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let gone = if out.contains("Rick Astley") {
        "dQw4w9WgXcQ"
    } else {
        "okvideo01"
    };
    assert_eq!(done(&env), 1);
    let db = || rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
    let (files, reason): (String, String) = db()
        .query_row(
            "SELECT files, cleanup_reason FROM items WHERE video_id = ?1 AND status = 'cleaned_up'",
            [gone],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(reason, "beyond max_items = 1");
    let files: serde_json::Value = serde_json::from_str(&files).unwrap();
    let media = PathBuf::from(files["media"].as_str().unwrap());
    let metadata = PathBuf::from(files["metadata"].as_str().unwrap());
    assert!(!media.exists() && metadata.exists(), "keep = \"metadata\"");
    let trash = env.path("data/tidepool/trash/dl");
    assert!(
        fs::read_dir(&trash).unwrap().next().is_some(),
        "in the trash"
    );

    // Not downloaded again, not reported missing.
    env.cmd(&["run"]).assert().stdout(contains("missing").not());
    assert!(!media.exists());

    env.cmd(&["trash", "list"])
        .assert()
        .success()
        .stdout(contains(format!("[{gone}]")).and(contains("deleted for good on")));
    env.cmd(&["trash", "restore", gone, "nope"])
        .assert()
        .code(3)
        .stdout(contains("restored").and(contains("nope: not in the trash")));
    assert!(media.exists());
    assert_eq!(done(&env), 2);
    env.cmd(&["trash", "list"])
        .assert()
        .stdout(contains("the trash is empty"));

    // A before_cleanup hook can keep it; one that crashes keeps it too.
    let guard = script(
        &env,
        "guard.sh",
        r#"cat > /dev/null; echo '{"keep": true}'"#,
    );
    let broken = script(&env, "broken.sh", "echo 'oops' >&2; exit 2");
    let config = hooks_config(
        &env,
        "hooks = [\"guard\", \"broken\"]\n[category.default.cleanup]\nmax_items = 1",
        &format!(
            "[hooks.guard]\ntype = \"before_cleanup\"\ncommand = [\"{}\"]\n\n\
             [hooks.broken]\ntype = \"before_cleanup\"\ncommand = [\"{}\"]\n",
            guard.display(),
            broken.display()
        ),
    );
    env.cmd(&["cleanup", "--config", &config])
        .assert()
        .code(3)
        .stdout(contains(
            "kept: hook guard said keep; hook broken failed: exited with exit status: 2: oops",
        ));
    assert_eq!(done(&env), 2);

    // Deleted right away, then the trash empties itself when it's time.
    let config = hooks_config(
        &env,
        "[category.default.cleanup]\nmax_items = 1\ndelete = \"now\"",
        "",
    );
    env.cmd(&["cleanup", "--config", &config])
        .assert()
        .success()
        .stdout(contains("deleted (beyond max_items = 1)"));
    assert_eq!(done(&env), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trash_empties_itself() {
    let env = Env::new().await;
    let config = env.config("");
    env.cmd(&["run", "--config", &config]).assert().code(3);
    let config = hooks_config(&env, "[category.default.cleanup]\nmax_items = 1", "");
    env.cmd(&["cleanup", "--config", &config])
        .assert()
        .success();
    let trash = env.path("data/tidepool/trash");
    assert!(trash.join("dl").is_dir());
    // Not yet.
    env.cmd(&["cleanup"])
        .assert()
        .stdout(contains("emptied").not());
    let db = rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
    db.execute("UPDATE trash SET delete_after = '2000-01-01T00:00:00Z'", [])
        .unwrap();
    env.cmd(&["download"])
        .assert()
        .stdout(contains("emptied 1 video from the trash"));
    assert!(!trash.join("dl").exists());
    env.cmd(&["trash", "list"])
        .assert()
        .stdout(contains("the trash is empty"));
}

#[tokio::test(flavor = "multi_thread")]
async fn max_items_limits_what_gets_downloaded() {
    let env = Env::new().await;
    let config = hooks_config(&env, "[category.default.cleanup]\nmax_items = 1", "");
    env.cmd(&["index", "-v", "--config", &config])
        .assert()
        .success()
        .stdout(contains("1 to download").and(contains("beyond max_items")))
        .stderr(contains(
            "drop (beyond max_items: only the first 1 are kept)",
        ));
    let statuses = env.statuses("dl");
    assert_eq!(statuses[0].1, "pending", "the first in the playlist");
    assert!(statuses[1..].iter().all(|(_, s, _)| s == "filtered"));

    // A bigger limit lets the next ones in.
    let config = hooks_config(&env, "[category.default.cleanup]\nmax_items = 2", "");
    env.cmd(&["index", "--config", &config])
        .assert()
        .success()
        .stdout(contains("2 to download"));
}
