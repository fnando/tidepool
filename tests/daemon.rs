//! `tidepool run --daemon` end to end: the binary, the fake yt-dlp and a fake
//! thumbnail server.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
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

    /// Start `tidepool run --daemon`, its log in `daemon.log`.
    fn daemon(&self, config: &str, extra_env: &[(&str, &str)]) -> Daemon {
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
    assert_eq!(statuses, ["stopped", "stopped"]);
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
async fn the_daemon_needs_a_schedule() {
    let env = Env::new().await;
    let config = env.config("", "");
    env.cmd(&["run", "--daemon", "--config", &config])
        .assert()
        .code(1)
        .stderr(contains("schedule.interval is empty"));
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
