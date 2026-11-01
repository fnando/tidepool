//! Indexing through the YouTube Data API, end to end: the binary against a
//! fake API serving real recorded responses, with the fake yt-dlp to fall
//! back to.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;
use wiremock::{Mock, MockServer};

#[path = "../src/index/youtube_fake.rs"]
mod youtube_fake;

const KEY: &str = "AIzaSyD-test-key-0000000000x1Qw";

fn test_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bin")
}

struct Env {
    tmp: TempDir,
    api: MockServer,
}

impl Env {
    async fn new(api: MockServer) -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
            api,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    /// A config with the unlisted playlist as a source, using the API key.
    fn config(&self, youtube: &str) -> String {
        let text = format!(
            "[category.default]\nmedia_dir = \"{}\"\n\n[category.default.youtube]\napi_key = \"{KEY}\"\n{youtube}\n\n\
             [tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n\n\
             [sources.scatolove]\nurl = \"https://www.youtube.com/playlist?list={}\"\n",
            self.path("media").display(),
            test_bin().join("yt-dlp").display(),
            youtube_fake::PLAYLIST,
        );
        let path = self.path("config.toml");
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("tidepool").unwrap();
        cmd.env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", test_bin().display()))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("TIDEPOOL_TEST_YOUTUBE_API", self.api.uri())
            .args(args);
        cmd
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.path("data/tidepool/state.db")).unwrap()
    }

    fn units_today(&self) -> i64 {
        self.db()
            .query_row("SELECT COALESCE(SUM(units), 0) FROM quota_usage", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn indexes_through_the_api_and_spends_little() {
    let env = Env::new(youtube_fake::start().await).await;
    let config = env.config("");
    env.cmd(&["index", "-v", "--config", &config])
        .assert()
        .success()
        .stdout(contains("scatolove: 10 videos (10 new): 10 to download"))
        .stderr(
            contains("scatolove: listed 10 videos in")
                .and(contains("(YouTube API)"))
                .and(contains("2 API units (key AIza…x1Qw: 2 of 10000 today)")),
        );
    let (description, duration): (String, i64) = env
        .db()
        .query_row(
            "SELECT description, duration_s FROM videos WHERE title = 'SCATOLOVE - O TIRO (Lyric Video)'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        !description.is_empty(),
        "the API path brings full descriptions"
    );
    assert_eq!(duration, 144);

    // Unchanged: only the page is read again.
    env.cmd(&["index"]).assert().success();
    assert_eq!(env.units_today(), 3);
    let key_id: String = env
        .db()
        .query_row("SELECT key_id FROM quota_usage LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        !key_id.contains("AIza"),
        "the key itself is never stored there"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn falls_back_to_ytdlp_when_the_quota_runs_out() {
    let env = Env::new(youtube_fake::start().await).await;
    // One unit: the page, then nothing left for the details.
    let config = env.config("daily_quota = 1");
    env.cmd(&["index", "-v", "--config", &config])
        .assert()
        .success()
        .stdout(contains("scatolove: 10 videos (10 new)"))
        .stderr(
            contains(
                "the YouTube API quota is used up for today; using yt-dlp for the rest of this run",
            )
            .and(contains("(yt-dlp)")),
        );
    env.cmd(&["problems"]).assert().stdout(
        contains("your YouTube API quota is used up for today").and(contains("midnight Pacific")),
    );

    // Without the fallback: exit 4, and nothing indexed.
    let text = fs::read_to_string(&config).unwrap().replace(
        "daily_quota = 1",
        "daily_quota = 1\nfallback_to_ytdlp = false",
    );
    fs::write(&config, text).unwrap();
    env.cmd(&["index", "--config", &config])
        .assert()
        .code(4)
        .stderr(contains(
            "not using yt-dlp instead (fallback_to_ytdlp = false)",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_key_becomes_a_problem() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(youtube_fake::key_invalid())
        .mount(&server)
        .await;
    let env = Env::new(server).await;
    let config = env.config("");
    env.cmd(&["index", "--config", &config])
        .assert()
        .success()
        .stdout(contains("scatolove: 10 videos"))
        .stderr(contains("YouTube rejected the API key: API key not valid"));
    env.cmd(&["problems"]).assert().stdout(
        contains("YouTube rejected your API key")
            .and(contains("key AIza…x1Qw"))
            .and(contains("replace youtube.api_key")),
    );
    env.cmd(&["config", "--show-secrets"]).assert().success();
    let problems: String = env
        .db()
        .query_row("SELECT group_concat(sample) FROM problems", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!problems.contains(KEY), "only the masked key is kept");
}

/// A config with a channel source.
fn channel_config(env: &Env, url: &str, with_key: bool) -> String {
    let key = if with_key {
        format!("[category.default.youtube]\napi_key = \"{KEY}\"\nrequests_per_second = 1000\n\n")
    } else {
        String::new()
    };
    let text = format!(
        "[category.default]\nmedia_dir = \"{}\"\n\n{key}\
         [tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n\n\
         [sources.supercombo]\nurl = \"{url}\"\n",
        env.path("media").display(),
        test_bin().join("yt-dlp").display(),
    );
    let path = env.path("config.toml");
    fs::write(&path, text).unwrap();
    path.display().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_through_the_api() {
    let env = Env::new(youtube_fake::start().await).await;
    let config = channel_config(&env, "https://www.youtube.com/@supercombooficial", true);
    env.cmd(&["index", "-v", "--config", &config])
        .assert()
        .success()
        .stdout(contains("supercombo: 587 videos (587 new)"))
        .stderr(
            contains("channel @supercombooficial is UC5eeDHYjTWF9pPWYDKpVMYQ; listing its uploads (UU5eeDHYjTWF9pPWYDKpVMYQ)")
                .and(contains("(YouTube API)")),
        );
    assert_eq!(
        env.units_today(),
        25,
        "1 lookup + 12 pages + 12 detail batches"
    );
    let title: String = env
        .db()
        .query_row(
            "SELECT title FROM sources WHERE key = 'supercombo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        title, "Supercombo",
        "the channel's name, for {{source_title}}"
    );

    // The lookup is remembered and known videos cost nothing more; the 2
    // scheduled streams are looked at again (1 batch) until they've aired.
    env.cmd(&["index"]).assert().success();
    assert_eq!(env.units_today(), 25 + 12 + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_without_a_key() {
    let env = Env::new(MockServer::start().await).await;
    for url in [
        "https://www.youtube.com/@supercombooficial/videos",
        "https://www.youtube.com/channel/UC5eeDHYjTWF9pPWYDKpVMYQ",
    ] {
        let config = channel_config(&env, url, false);
        env.cmd(&["index", "-v", "--config", &config])
            .assert()
            .success()
            .stdout(contains("supercombo: 20 videos"))
            .stderr(
                contains("listing its uploads (UU5eeDHYjTWF9pPWYDKpVMYQ)")
                    .and(contains("(yt-dlp)")),
            );
    }
    // A channel that doesn't exist.
    let config = channel_config(&env, "https://www.youtube.com/@nobody-here", false);
    env.cmd(&["index", "--config", &config])
        .assert()
        .code(3)
        .stderr(contains(
            "supercombo: yt-dlp failed: ERROR: [youtube:tab] @nobody-here",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn kind_rules_skip_shorts_and_live_streams() {
    let env = Env::new(youtube_fake::start().await).await;
    let shorts = youtube_fake::shorts().await;
    let config = channel_config(&env, "https://www.youtube.com/@supercombooficial", true);
    // The source is last in the file: its rules go at the end.
    let text = fs::read_to_string(&config).unwrap()
        + "exclude = [{ field = \"kind\", value = \"short\" }, { field = \"kind\", value = \"live\" }]\n";
    fs::write(&config, &text).unwrap();
    env.cmd(&["index", "--config", &config])
        .env("TIDEPOOL_TEST_SHORTS_BASE", shorts.uri())
        .assert()
        .success()
        .stdout(contains("supercombo: 587 videos (587 new)"));
    let count = |sql: &str| -> i64 { env.db().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM videos WHERE kind = 'short'"),
        109
    );
    assert_eq!(count("SELECT COUNT(*) FROM videos WHERE kind = 'live'"), 19);
    assert_eq!(
        count("SELECT COUNT(*) FROM items WHERE status = 'pending'"),
        587 - 109 - 19,
        "Shorts and streams skipped"
    );
    assert_eq!(
        shorts.received_requests().await.unwrap().len(),
        179,
        "only videos of 3 minutes or less that aren't streams are checked"
    );

    // Known kinds aren't checked again.
    env.cmd(&["index"])
        .env("TIDEPOOL_TEST_SHORTS_BASE", shorts.uri())
        .assert()
        .success();
    assert_eq!(shorts.received_requests().await.unwrap().len(), 179);
}
