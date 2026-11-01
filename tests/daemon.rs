//! `tidepool run --daemon` end to end: the binary, the fake yt-dlp and a fake
//! thumbnail server.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;
use wiremock::matchers::path_regex;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bin")
}

const RICK_DIR: &str = "dl/2009-10-25 - Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster) [dQw4w9WgXcQ]";

struct Env {
    tmp: TempDir,
    thumbs: MockServer,
}

impl Env {
    async fn new() -> Self {
        let thumbs = MockServer::start().await;
        Mock::given(path_regex(r"/vi/.*"))
            .respond_with(ResponseTemplate::new(404))
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

    fn envs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("PATH", format!("{}:/usr/bin:/bin", test_bin().display())),
            ("HOME", self.path("home").display().to_string()),
            ("XDG_CONFIG_HOME", self.path("config").display().to_string()),
            ("XDG_DATA_HOME", self.path("data").display().to_string()),
            ("TIDEPOOL_TEST_THUMBNAIL_BASE", self.thumbs.uri()),
        ]
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("tidepool").unwrap();
        cmd.env_clear().envs(self.envs()).args(args);
        cmd
    }

    /// A config syncing `dl` every `interval`, plus `extra` at the end.
    fn config(&self, interval: &str, extra: &str) -> String {
        let text = format!(
            "[schedule]\ninterval = \"{interval}\"\n\n\
             [server]\nlisten = \"127.0.0.1:0\"\n\n\
             [category.default]\nmedia_dir = \"{}\"\n\n\
             [tools.deno]\nmanaged = false\n\n\
             [tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n\n\
             [sources.dl]\nurls = [\"https://www.youtube.com/playlist?list=PLdownloads\"]\n\n{extra}",
            self.path("media").display(),
            test_bin().join("yt-dlp").display()
        );
        let path = self.path("config.toml");
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.path("data/tidepool/state.db")).unwrap()
    }

    /// Import `config`, then start `tidepool run --daemon` with it, its log
    /// in `daemon.log`.
    fn daemon(&self, config: &str, extra_env: &[(&str, &str)]) -> Daemon {
        self.cmd(&["config", "import", config]).assert().success();
        let log = self.path("daemon.log");
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tidepool"));
        cmd.env_clear()
            .envs(self.envs())
            .envs(extra_env.iter().copied())
            .args(["run", "--daemon", "--config", config])
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap());
        Daemon {
            child: cmd.spawn().unwrap(),
            log,
        }
    }
}

impl Env {
    /// Start `tidepool run --daemon --config <config> <extra>` as it is.
    fn daemon_without_import(&self, config: &str, extra: &[&str]) -> Daemon {
        let log = self.path("daemon.log");
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tidepool"));
        cmd.env_clear()
            .envs(self.envs())
            .args(["run", "--daemon", "--config", config])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap());
        Daemon {
            child: cmd.spawn().unwrap(),
            log,
        }
    }
}

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Wait (up to 20 s) until `ready` says so.
    fn wait_for(&self, what: &str, ready: impl Fn() -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}; log:\n{}", self.log());
    }

    fn wait_for_log(&self, text: &str) {
        self.wait_for(&format!("{text:?} in the log"), || {
            self.log().contains(text)
        });
    }

    /// SIGTERM, and its exit code.
    fn stop(mut self) -> (Option<i32>, String) {
        std::process::Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        let status = self.child.wait().unwrap();
        (status.code(), self.log())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn syncs_right_away_then_on_schedule_and_stops_cleanly() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    let daemon = env.daemon(&config, &[]);
    let rick = env.path("media").join(RICK_DIR);
    daemon.wait_for("Rick's video saved", || {
        rick.join("metadata.json").is_file()
    });
    daemon.wait_for_log("next sync at");

    let next: String = env
        .db()
        .query_row(
            "SELECT value FROM daemon_state WHERE key = 'next_sync_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let next: jiff::Timestamp = next.parse().unwrap();
    let wait = next.duration_since(jiff::Timestamp::now()).as_secs();
    assert!(
        (3500..=3800).contains(&wait),
        "about an hour from now: {wait}s"
    );

    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
    assert!(log.contains("tidepool daemon started"), "{log}");
    assert!(log.contains("synced 1 source"), "{log}");
    assert!(log.contains("dl: downloaded “Rick Astley"), "{log}");
    assert!(log.contains("tidepool daemon stopped"), "{log}");
    let statuses: Vec<String> = env
        .db()
        .prepare("SELECT status FROM daemon_services ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(statuses, ["stopped", "stopped", "stopped"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_daemon_at_a_time() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("tidepool daemon started");
    env.cmd(&["run", "--daemon"])
        .assert()
        .code(1)
        .stderr(contains(format!(
            "another tidepool daemon is running (pid {})",
            daemon.child.id()
        )));
    let (code, _) = daemon.stop();
    assert_eq!(code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_needs_a_schedule_or_the_web_ui() {
    let env = Env::new().await;
    let config = env.config("", "");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("[server]\n", "[server]\nenabled = false\n");
    fs::write(&config, text).unwrap();
    env.cmd(&["config", "import", &config]).assert().success();
    env.cmd(&["run", "--daemon", "--config", &config])
        .assert()
        .code(1)
        .stderr(contains("schedule.interval is empty and the web UI is off"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_service_is_restarted() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    let daemon = env.daemon(&config, &[("TIDEPOOL_TEST_PANIC", "processor")]);
    daemon.wait_for_log("processor stopped unexpectedly");
    // The syncer goes on; the restarted processor saves what it downloaded.
    let rick = env.path("media").join(RICK_DIR);
    daemon.wait_for("Rick's video saved", || {
        rick.join("metadata.json").is_file()
    });
    let problem: String = env
        .db()
        .query_row(
            "SELECT sample FROM problems WHERE kind = 'daemon_service_failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(problem.starts_with("processor: crashed"), "{problem}");
    let (restarts, status): (i64, String) = env
        .db()
        .query_row(
            "SELECT restarts, status FROM daemon_services WHERE name = 'processor'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(restarts, 1);
    assert_ne!(status, "restarting");
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_hooks_run_in_the_processor() {
    let env = Env::new().await;
    let ran = env.path("ran.log");
    let hook = env.path("notify.sh");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat > /dev/null\necho \"$TIDEPOOL_VIDEO_ID\" >> '{}'\n\
             [ \"$TIDEPOOL_VIDEO_ID\" = dQw4w9WgXcQ ] && exit 1\nexit 0\n",
            ran.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let config = env.config(
        "1h",
        &format!("[hooks.notify]\ncommand = [\"{}\"]\n", hook.display()),
    );
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("[sources.dl]\n", "[sources.dl]\nhooks = [\"notify\"]\n");
    fs::write(&config, text).unwrap();
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for("both hooks ran", || {
        let ran = fs::read_to_string(&ran).unwrap_or_default();
        ran.contains("dQw4w9WgXcQ") && ran.contains("okvideo01")
    });
    let status = |video: &str| -> String {
        env.db()
            .query_row(
                "SELECT status FROM hook_runs WHERE video_id = ?1",
                [video],
                |r| r.get(0),
            )
            .unwrap()
    };
    daemon.wait_for("hook statuses", || {
        status("dQw4w9WgXcQ") == "retrying" && status("okvideo01") == "ok"
    });
    let (code, _) = daemon.stop();
    assert_eq!(code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_is_skipped_while_another_tidepool_runs() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    env.cmd(&["config", "import", &config]).assert().success();
    // The next sync is a few seconds away.
    let soon = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_secs(4))
        .unwrap();
    env.db()
        .execute(
            "INSERT INTO daemon_state (key, value) VALUES ('next_sync_at', ?1)",
            [soon.to_string()],
        )
        .unwrap();
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("tidepool daemon started");
    // Another tidepool (here, a stand-in) holds the library meanwhile.
    let mut holder = std::process::Command::new("python3")
        .args([
            "-c",
            "import fcntl, sys, time\nf = open(sys.argv[1], 'a')\nfcntl.flock(f, fcntl.LOCK_EX)\nprint('locked', flush=True)\ntime.sleep(8)",
        ])
        .arg(env.path("data/tidepool/lock"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(holder.stdout.as_mut().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line.trim(), "locked");
    daemon.wait_for_log("skipped: another tidepool is running");
    daemon.wait_for_log("sync skipped; next sync at");
    assert!(!env.path("media").join(RICK_DIR).exists());
    let _ = holder.kill();
    let _ = holder.wait();
    let (code, _) = daemon.stop();
    assert_eq!(code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_commands_without_a_daemon() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    env.cmd(&["config", "import", &config]).assert().success();
    env.cmd(&["daemon", "status"]).assert().success().stdout(
        "daemon: not running\nsyncer: not running\nprocessor: not running\nserver: not running\n",
    );
    env.cmd(&["daemon", "pause", "syncer"])
        .assert()
        .success()
        .stdout("paused syncer; it stays paused when the daemon starts\n");
    env.cmd(&["daemon", "pause", "syncer"])
        .assert()
        .success()
        .stdout("syncer is already paused\n");
    env.cmd(&["daemon", "status"])
        .assert()
        .success()
        .stdout(contains("syncer: paused\n"));
    env.cmd(&["daemon", "sync"])
        .assert()
        .code(1)
        .stderr(contains("the syncer is paused"));
    env.cmd(&["daemon", "resume"]).assert().success().stdout(
        "resumed syncer; it runs when the daemon starts\nprocessor is already not paused\n\
         server is already not paused\n",
    );
    env.cmd(&["daemon", "restart"])
        .assert()
        .code(1)
        .stderr(contains("the daemon isn't running"));
    env.cmd(&["daemon", "pause", "nope"]).assert().code(2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_syncer_stays_paused_until_resumed() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    env.cmd(&["config", "import", &config]).assert().success();
    env.cmd(&["daemon", "pause", "syncer"]).assert().success();
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("tidepool daemon started");
    daemon.wait_for("the processor running", || {
        let out = env.cmd(&["daemon", "status"]).output().unwrap();
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        out.contains("syncer: paused") && out.contains("processor: idle")
    });
    std::thread::sleep(Duration::from_secs(1));
    let rick = env.path("media").join(RICK_DIR);
    assert!(!rick.exists(), "nothing synced while paused");

    env.cmd(&["daemon", "resume", "syncer"])
        .assert()
        .success()
        .stdout("resumed syncer\n");
    daemon.wait_for("Rick's video saved", || {
        rick.join("metadata.json").is_file()
    });

    env.cmd(&["daemon", "pause", "processor"])
        .assert()
        .success()
        .stdout("paused processor\n");
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
    assert!(log.contains("pausing processor"), "{log}");
    let paused: bool = env
        .db()
        .query_row(
            "SELECT paused FROM daemon_services WHERE name = 'processor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(paused, "still paused for the next start");
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_now_and_restart_on_request() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    env.cmd(&["config", "import", &config]).assert().success();
    let later = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(1))
        .unwrap();
    env.db()
        .execute(
            "INSERT INTO daemon_state (key, value) VALUES ('next_sync_at', ?1)",
            [later.to_string()],
        )
        .unwrap();
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("tidepool daemon started");
    let rick = env.path("media").join(RICK_DIR);
    assert!(!rick.exists(), "the next sync is an hour away");

    env.cmd(&["daemon", "sync"])
        .assert()
        .success()
        .stdout("syncing now\n");
    daemon.wait_for("Rick's video saved", || {
        rick.join("metadata.json").is_file()
    });

    env.cmd(&["daemon", "restart", "processor"])
        .assert()
        .success()
        .stdout("restarting processor\n");
    daemon.wait_for("the processor back", || {
        let out = env.cmd(&["daemon", "status"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("processor: idle")
    });
    let out = env.cmd(&["daemon", "status"]).output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.starts_with(&format!("daemon: running (pid {})\n", daemon.child.id())),
        "{out}"
    );
    assert!(out.contains("syncer: idle, next sync at "), "{out}");
    assert!(out.contains("last sync: "), "{out}");
    assert!(
        !out.contains("restarted"),
        "asked for, not a failure: {out}"
    );
    let problems: i64 = env
        .db()
        .query_row(
            "SELECT COUNT(*) FROM problems WHERE kind = 'daemon_service_failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(problems, 0);
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_config_file_must_match_the_stored_config() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    // Nothing stored yet: it says how to start.
    env.cmd(&["run", "--daemon", "--config", &config])
        .assert()
        .code(1)
        .stderr(contains("no stored config yet"));
    env.cmd(&["config", "import", &config]).assert().success();

    // A comment changes nothing.
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("# mine\n{text}")).unwrap();
    let daemon = env.daemon_without_import(&config, &[]);
    daemon.wait_for_log("tidepool daemon started");
    let (code, _) = daemon.stop();
    assert_eq!(code, Some(0));

    // A real change: stopped, with what differs and how to fix it, whether
    // the file is named or was the last one imported.
    fs::write(
        &config,
        text.replace("interval = \"1h\"", "interval = \"2h\""),
    )
    .unwrap();
    for args in [
        vec!["run", "--daemon", "--config", config.as_str()],
        vec!["run", "--daemon"],
    ] {
        env.cmd(&args).assert().code(1).stderr(
            contains("differs from the stored config (revision 1, from import)")
                .and(contains("schedule.interval: \"1h\" → \"2h\""))
                .and(contains("tidepool config import"))
                .and(contains("--on-config-mismatch import")),
        );
    }

    // Export: the stored config goes to the file, the edit to config.toml.bak.
    let daemon = env.daemon_without_import(&config, &["--on-config-mismatch", "export"]);
    daemon.wait_for_log("tidepool daemon started");
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0));
    assert!(log.contains("wrote config revision 1 to"), "{log}");
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains("interval = \"1h\"")
    );
    assert!(
        fs::read_to_string(format!("{config}.bak"))
            .unwrap()
            .contains("interval = \"2h\"")
    );

    // Import: the file becomes revision 2.
    fs::write(
        &config,
        text.replace("interval = \"1h\"", "interval = \"2h\""),
    )
    .unwrap();
    let daemon = env.daemon_without_import(&config, &["--on-config-mismatch", "import"]);
    daemon.wait_for_log("syncing every 2h");
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0));
    assert!(log.contains("as config revision 2"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_web_ui_shows_the_daemon_and_syncs_on_request() {
    let env = Env::new().await;
    // No schedule: only the web UI (or `tidepool daemon sync`) starts a sync.
    let config = env.config("", "");
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("web UI at http://127.0.0.1:");
    let url = daemon
        .log()
        .lines()
        .find_map(|l| {
            l.split("web UI at ")
                .nth(1)
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap();
    let http = reqwest::Client::new();
    let page = http.get(&url).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("No schedule"), "{page}");
    assert!(page.contains("<strong>dl</strong>"), "{page}");

    let answer: serde_json::Value = http
        .post(format!("{url}/api/v1/sync"))
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer["message"], "Syncing now.");
    daemon.wait_for("both videos saved", || {
        env.db()
            .query_row(
                "SELECT COUNT(*) FROM items WHERE status = 'done'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 2
    });
    let status: serde_json::Value = http
        .get(format!("{url}/api/v1/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sources"][0]["counts"]["downloaded"], 2);
    let out = env.cmd(&["daemon", "status"]).output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.contains(&format!("server: running, listening on {url}")),
        "{out}"
    );

    // Switched off in the config: the web UI stops, the rest goes on.
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("[server]\n", "[server]\nenabled = false\n");
    fs::write(&config, text).unwrap();
    env.cmd(&["config", "import", &config]).assert().success();
    daemon.wait_for_log("server is off");
    assert!(
        http.get(&url).send().await.is_err(),
        "not listening anymore"
    );
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn work_asked_on_a_video_page_is_done_by_the_processor() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("web UI at http://127.0.0.1:");
    let url = daemon
        .log()
        .lines()
        .find_map(|l| {
            l.split("web UI at ")
                .nth(1)
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap();
    let rick = env.path("media").join(RICK_DIR);
    daemon.wait_for("Rick's video saved", || {
        rick.join("metadata.json").is_file()
    });

    let http = reqwest::Client::new();
    let page = http
        .get(format!("{url}/sources/dl/videos/dQw4w9WgXcQ"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("Download again"), "{page}");
    let csrf = page
        .split("\"X-CSRF-Token\": \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();
    let answer = http
        .post(format!("{url}/sources/dl/videos/dQw4w9WgXcQ/reset_delete"))
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert!(answer.status().is_success());
    assert!(answer.text().await.unwrap().contains("Deleting its files"));
    let result = || -> Option<(bool, String)> {
        env.db()
            .query_row(
                "SELECT ok, result FROM daemon_jobs WHERE done_at IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    };
    daemon.wait_for("the job done", || result().is_some());
    let (ok, text) = result().unwrap();
    assert!(ok, "{text}");
    assert!(text.contains("reset dQw4w9WgXcQ in dl"), "{text}");
    assert!(!rick.join("metadata.json").exists(), "files deleted");
    let page = http
        .get(format!("{url}/sources/dl/videos/dQw4w9WgXcQ"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("pending") && page.contains("reset dQw4w9WgXcQ in dl"),
        "{page}"
    );
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_saved_in_the_web_ui_applies_and_the_next_start_agrees() {
    let env = Env::new().await;
    let config = env.config("1h", "");
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("web UI at http://127.0.0.1:");
    let url = daemon
        .log()
        .lines()
        .find_map(|l| {
            l.split("web UI at ")
                .nth(1)
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap();
    let http = reqwest::Client::new();
    let page = http
        .get(format!("{url}/config"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = page
        .split("\"X-CSRF-Token\": \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();
    let edited = fs::read_to_string(&config)
        .unwrap()
        .replace("interval = \"1h\"", "interval = \"3h\"");
    let answer = http
        .post(format!("{url}/config/save"))
        .header("x-csrf-token", &csrf)
        .header("hx-request", "true")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("base=1&text={}", encode(&edited)))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.headers()["hx-redirect"], "/config?saved=2");
    daemon.wait_for_log("applying config revision 2");
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        edited,
        "written to the file"
    );
    let (code, _) = daemon.stop();
    assert_eq!(code, Some(0));

    // The file and the stored config match: the next start is fine.
    let daemon = env.daemon_without_import(&config, &[]);
    daemon.wait_for_log("syncing every 3h");
    let (code, log) = daemon.stop();
    assert_eq!(code, Some(0), "{log}");
}

/// Percent-encode a form value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn live_events_follow_a_sync() {
    use futures_util::StreamExt;
    let env = Env::new().await;
    let config = env.config("", "");
    let daemon = env.daemon(&config, &[]);
    daemon.wait_for_log("web UI at http://127.0.0.1:");
    let url = daemon
        .log()
        .lines()
        .find_map(|l| {
            l.split("web UI at ")
                .nth(1)
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap();
    let http = reqwest::Client::new();
    let response = http
        .get(format!("{url}/api/v1/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.bytes_stream();
    http.post(format!("{url}/api/v1/sync"))
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();

    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(seen.contains(r#""type":"sync_finished""#)
        && seen.matches(r#""type":"saved""#).count() == 2)
    {
        assert!(Instant::now() < deadline, "events so far:\n{seen}");
        let chunk = tokio::time::timeout(Duration::from_secs(20), stream.next())
            .await
            .expect("an event")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    for kind in [
        "sync_started",
        "checking",
        "downloading",
        "fetched",
        "finishing",
        "failed",
    ] {
        assert!(
            seen.contains(&format!(r#""type":"{kind}""#)),
            "{kind}:\n{seen}"
        );
    }
    assert!(seen.contains(r#""step":"saving""#), "{seen}");

    // The stream ends with the daemon, which stops right away.
    let started = Instant::now();
    let (code, _) = daemon.stop();
    assert_eq!(code, Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "not held up by the stream"
    );
}
