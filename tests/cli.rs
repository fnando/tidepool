//! End-to-end: run the binary against a temp library.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

fn test_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bin")
}

/// Fake yt-dlp/ffmpeg/ffprobe first, then the system basics.
fn test_path() -> String {
    format!("{}:/usr/bin:/bin", test_bin().display())
}

struct Env {
    tmp: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("tidepool").unwrap();
        cmd.env_clear()
            .env("PATH", test_path())
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .args(args);
        cmd
    }

    /// A valid config file using the fake yt-dlp and a temp media root.
    fn write_config(&self, name: &str, extra: &str) -> String {
        let ytdlp = test_bin().join("yt-dlp");
        let text = format!(
            "# my config\n[library]\nroot = \"{}\"\n\n[ytdlp]\nmanaged = false\npath = \"{}\"\n\n\
             [youtube]\napi_key = \"AIzaSyD-1234567890x1Qw\"\n\n\
             [sources.lofi]\nurl = \"https://www.youtube.com/playlist?list=PLaaaaaaaaaaaa\"\ninclude = ['(?i)\\blive\\b']\nexclude = ['(?i)trailer']\n{extra}",
            self.path("media").display(),
            ytdlp.display()
        );
        let path = self.path(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        path.display().to_string()
    }
}

#[test]
fn commands_without_a_config_explain_how_to_import_one() {
    let env = Env::new();
    env.cmd(&["config"]).assert().code(1).stderr(contains(
        "library \"default\" has no config yet; run `tidepool init` (or `tidepool config import <path>`) first",
    ));
}

#[test]
fn import_history_rollback_export_round_trip() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    let original = fs::read_to_string(&file).unwrap();

    env.cmd(&["config", "import", &file])
        .assert()
        .success()
        .stdout(contains("as revision 1"));
    env.cmd(&["config", "import", &file])
        .assert()
        .success()
        .stdout(contains("unchanged"));

    // Shown with comments, keys masked unless --show-secrets.
    env.cmd(&["config"]).assert().success().stdout(
        contains("# my config")
            .and(contains("AIza…x1Qw"))
            .and(contains("1234567890").not()),
    );
    env.cmd(&["config", "--show-secrets"])
        .assert()
        .success()
        .stdout(contains("AIzaSyD-1234567890x1Qw"));

    env.write_config("c.toml", "enabled = false\n");
    env.cmd(&["config", "import", &file])
        .assert()
        .success()
        .stdout(contains("as revision 2"));
    env.cmd(&["config", "rollback", "1"])
        .assert()
        .success()
        .stdout(contains("as revision 3"));
    env.cmd(&["config", "export", "--show-secrets"])
        .assert()
        .success()
        .stdout(original.clone());
    env.cmd(&["config", "export", "--rev", "2"])
        .assert()
        .success()
        .stdout(contains("enabled = false"));

    env.cmd(&["config", "history"]).assert().success().stdout(
        contains("     1  2")
            .and(contains("*    3  2"))
            .and(contains(" UTC "))
            .and(contains("rollback"))
            .and(contains("rev:1")),
    );
    env.cmd(&["config", "rollback", "42"])
        .assert()
        .code(1)
        .stderr(contains("revision 42 does not exist"));
}

#[test]
fn config_flag_imports_when_changed_and_guards_other_edits() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "--config", &file]).assert().success();
    env.cmd(&["config", "history"])
        .assert()
        .stdout(contains("*    1  "));

    env.write_config("c.toml", "enabled = false\n");
    env.cmd(&["config", "-c", &file])
        .assert()
        .success()
        .stdout(contains("enabled = false"));

    env.cmd(&["config", "rollback", "1"]).assert().success();
    env.write_config("c.toml", "enabled = true\n");
    env.cmd(&["config", "--config", &file])
        .assert()
        .code(1)
        .stderr(contains("changed by rollback (revision 3)").and(contains("--force-config")));
    env.cmd(&["config", "--config", &file, "--force-config"])
        .assert()
        .success()
        .stdout(contains("enabled = true"));

    // TIDEPOOL_CONFIG works like --config.
    env.cmd(&["config", "history"])
        .env("TIDEPOOL_CONFIG", &file)
        .assert()
        .success()
        .stdout(contains("*    4  "));
}

#[test]
fn invalid_configs_report_file_line_col_and_are_not_stored() {
    let env = Env::new();
    let file = env.path("bad.toml");
    fs::write(
        &file,
        "[library]\nconcurency = 2\n\n[sources.lofi]\nurl = 'https://www.youtube.com/playlist?list=PLx'\nmin_duration = 'soon'\n",
    )
    .unwrap();
    let file = file.display().to_string();
    env.cmd(&["config", "import", &file])
        .assert()
        .code(1)
        .stderr(
            contains(format!(
                "{file}:2:1 library.concurency: unknown key \"concurency\""
            ))
            .and(contains(format!(
                "{file}:6:16 sources.lofi.min_duration: \"soon\" is not a valid duration"
            ))),
        );
    env.cmd(&["config", "check", &file]).assert().code(1);
    env.cmd(&["config", "history"])
        .assert()
        .success()
        .stdout(contains("no config revisions yet"));
}

#[test]
fn check_prints_the_effective_config() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "check", &file])
        .assert()
        .success()
        .stdout(
            contains("[sources.lofi]")
                .and(contains("cover_name = \"{title}-poster\""))
                .and(contains("AIza…x1Qw")),
        )
        .stderr(contains("yt-dlp 2026.10.01").and(contains("is valid (1 sources)")));

    // Without a path: checks the stored revision.
    env.cmd(&["config", "import", &file]).assert().success();
    env.cmd(&["config", "check"])
        .assert()
        .success()
        .stderr(contains("config revision 1 is valid"));
}

#[test]
fn check_fails_when_ytdlp_is_missing() {
    let env = Env::new();
    let file = env.path("c.toml");
    fs::write(
        &file,
        "[ytdlp]\nmanaged = false\npath = '/nonexistent/yt-dlp'\n",
    )
    .unwrap();
    env.cmd(&["config", "check", &file.display().to_string()])
        .assert()
        .code(1)
        .stderr(contains("cannot run \"/nonexistent/yt-dlp\""));
}

#[test]
fn diff_shows_key_changes_and_text_diff() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file]).assert().success();
    env.cmd(&["config", "diff"])
        .assert()
        .success()
        .stdout(contains("no differences"));

    env.write_config("c.toml", "enabled = false\n");
    env.cmd(&["config", "diff"])
        .assert()
        .success()
        .stdout(contains("+ sources.lofi.enabled = false").and(contains("+enabled = false")));
}

#[test]
fn filter_test_explains_the_decision() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    let run = |title: &str, source: &str| {
        env.cmd(&["filter", "test", title, "-s", source, "--config", &file])
            .assert()
    };

    run("Live at Rome", "lofi")
        .success()
        .stdout(contains("include[0]").and(contains("decision: keep (matches include[0])")));
    run("Live trailer", "PLaaaaaaaaaaaa")
        .success()
        .stdout(contains("decision: drop (matches exclude[0])"));
    run(
        "Studio",
        "https://www.youtube.com/playlist?list=PLaaaaaaaaaaaa",
    )
    .success()
    .stdout(contains("decision: drop (matches no include rule)"));
    run("x", "PLzzzzzzzzzzzz").code(2).stderr(contains(
        "--source \"PLzzzzzzzzzzzz\" matches no configured source (configured: lofi)",
    ));
}

#[test]
fn held_lock_names_the_holder() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    let _held = tidepool::lock::acquire(&env.path("data/tidepool/libraries/default/lock")).unwrap();
    env.cmd(&["config", "import", &file])
        .assert()
        .code(1)
        .stderr(contains(format!(
            "library is locked by another tidepool process (pid {})",
            std::process::id()
        )));
}

#[test]
fn libraries_are_isolated() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["--library", "work", "config", "import", &file])
        .assert()
        .success();
    assert!(env.path("data/tidepool/libraries/work/state.db").exists());
    env.cmd(&["config"]).assert().code(1);
    env.cmd(&["config", "-l", "work"]).assert().success();
}

#[test]
fn schema_command_prints_the_committed_schema() {
    let committed = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/tidepool.v1.schema.json"),
    )
    .unwrap();
    Env::new()
        .cmd(&["config", "schema"])
        .assert()
        .success()
        .stdout(committed);
}

#[test]
fn exit_codes() {
    let env = Env::new();
    env.cmd(&["problems"])
        .assert()
        .code(1)
        .stderr(contains("not implemented yet (planned for M2)"));
    env.cmd(&["run"])
        .assert()
        .code(1)
        .stderr(contains("has no config yet"));
    env.cmd(&["run", "--daemon"])
        .assert()
        .code(1)
        .stderr(contains("not implemented yet (planned for M7)"));
    env.cmd(&["--bogus"]).assert().code(2);
    env.cmd(&["sync"]).assert().code(2);
    env.cmd(&["run", "-s", "Not A Key"]).assert().code(2);
    env.cmd(&["run", "--playlist", "lofi"]).assert().code(2);
    // --source is for one-shot runs; --on-config-mismatch only applies to the daemon.
    env.cmd(&["run", "--daemon", "-s", "lofi"]).assert().code(2);
    env.cmd(&["run", "--on-config-mismatch", "import"])
        .assert()
        .code(2);
    env.cmd(&["--library", "../escape", "config"])
        .assert()
        .code(2);
}

#[test]
fn dry_run_import_changes_nothing() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file, "--dry-run"])
        .assert()
        .success()
        .stdout(contains("dry run"));
    assert!(
        !env.path("data/tidepool/libraries/default/state.db")
            .exists()
    );
}

#[test]
fn config_flag_applies_to_every_library_command() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "history", "--config", &file])
        .assert()
        .success()
        .stdout(contains("*    1  "));
    env.write_config("c.toml", "enabled = false\n");
    env.cmd(&["config", "history", "--config", &file])
        .assert()
        .success()
        .stdout(contains("*    2  "));
}

#[test]
fn init_creates_config_and_library() {
    let env = Env::new();
    let file = env.path("config/tidepool/config.toml");
    env.cmd(&["init"])
        .assert()
        .success()
        .stdout(
            contains(format!("Created {}", file.display()))
                .and(contains("Created library \"default\" (config revision 1)"))
                .and(contains("sample source"))
                .and(contains("tidepool config import"))
                .and(contains(
                    "every 6h with the web UI at http://127.0.0.1:8787: tidepool run --daemon",
                ))
                .and(contains("--on-config-mismatch import")),
        )
        .stderr(
            contains("will be downloaded on first use").and(contains("ffmpeg version 9.0.2-fake")),
        );

    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("#:schema https://raw.githubusercontent.com/fnando/tidepool/v"));
    assert!(text.contains(
        "[sources.sample]                    # one short video, to see tidepool work; replace it with your own\n\
         url = \"https://www.youtube.com/playlist?list=PLXLpuA-bwY4s\"   # the playlist's URL\n\
         # dir = \"sample\""
    ));

    // The library holds the same text as revision 1.
    env.cmd(&["config"]).assert().success().stdout(text);
    env.cmd(&["config", "history"])
        .assert()
        .success()
        .stdout(contains("*    1  "));
    env.cmd(&["filter", "test", "anything", "--source", "sample"])
        .assert()
        .success()
        .stdout(contains("decision: keep (no include rules)"));
}

#[test]
fn init_never_overwrites() {
    let env = Env::new();
    env.cmd(&["init"]).assert().success();
    let file = env.path("config/tidepool/config.toml");
    fs::write(&file, "# my edits\n").unwrap();

    // The library already has a config, even with --force.
    for args in [&["init"][..], &["init", "--force"]] {
        env.cmd(args).assert().code(1).stderr(contains(
            "library \"default\" already has a config (revision 1)",
        ));
    }
    // A new library, but the file exists.
    env.cmd(&["init", "-l", "work"])
        .assert()
        .code(1)
        .stderr(contains("already exists").and(contains("pass --force to replace it")));
    assert!(!env.path("data/tidepool/libraries/work/state.db").exists());

    // --force keeps the old file as .bak.
    env.cmd(&["init", "-l", "work", "--force"])
        .assert()
        .success()
        .stdout(contains("Sync once: tidepool run --library work"));
    assert_eq!(
        fs::read_to_string(env.path("config/tidepool/config.toml.bak")).unwrap(),
        "# my edits\n"
    );
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("[sources.sample]")
    );
}

#[test]
fn init_with_a_custom_path_and_library() {
    let env = Env::new();
    let file = env.path("elsewhere/tp.toml").display().to_string();
    env.cmd(&["init", "--config", &file, "--library", "music"])
        .assert()
        .success()
        .stdout(contains(format!(
            "tidepool config import {file} --library music"
        )));
    env.cmd(&["config", "-l", "music"])
        .assert()
        .success()
        .stdout(contains("[sources.sample]"));
}

#[test]
fn init_print_and_dry_run_create_nothing() {
    let env = Env::new();
    env.cmd(&["init", "--print"])
        .assert()
        .success()
        .stdout(contains("[sources.sample]").and(contains("# inherit_hooks = false")));
    env.cmd(&["init", "--dry-run"])
        .assert()
        .success()
        .stderr(contains("dry run"));
    assert!(!env.path("config").exists());
    assert!(!env.path("data").exists());
}

#[test]
fn init_asks_nothing() {
    // No terminal and no flags needed.
    let env = Env::new();
    env.cmd(&["init"]).write_stdin("").assert().success();
    for removed in ["--yes", "--root"] {
        env.cmd(&["init", removed]).assert().code(2);
    }
}

#[test]
fn check_requires_ffmpeg() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "check", &file])
        .env("PATH", env.path("empty-path"))
        .assert()
        .code(1)
        .stderr(contains("ffmpeg is required").and(contains("ffprobe is required")));
}

#[test]
fn import_defaults_to_the_config_dir() {
    let env = Env::new();
    let default = env.path("config/tidepool/config.toml");

    env.cmd(&["config", "import"]).assert().code(1).stderr(
        contains(format!("no config file at {}", default.display())).and(contains("tidepool init")),
    );

    env.cmd(&["init"]).assert().success();
    let text = fs::read_to_string(&default).unwrap();
    fs::write(
        &default,
        text.replace("interval = \"6h\"", "interval = \"12h\""),
    )
    .unwrap();
    env.cmd(&["config", "import"])
        .assert()
        .success()
        .stdout(contains(format!(
            "imported {} as revision 2",
            default.display()
        )));

    // --config / TIDEPOOL_CONFIG pick the file when no path is given.
    let other = env.write_config("other.toml", "");
    env.cmd(&["config", "import"])
        .env("TIDEPOOL_CONFIG", &other)
        .assert()
        .success()
        .stdout(contains(format!("imported {other} as revision 3")));
}

#[test]
fn init_next_steps_omit_the_default_path() {
    let env = Env::new();
    env.cmd(&["init"])
        .assert()
        .success()
        .stdout(contains("Apply your changes: tidepool config import\n"));
}

#[test]
fn filter_test_ignores_duration_and_cutoff_limits() {
    let env = Env::new();
    let file = env.write_config("c.toml", "min_duration = \"60s\"\ncutoff = \"30d\"\n");
    env.cmd(&[
        "filter", "test", "Live set", "-s", "lofi", "--config", &file,
    ])
    .assert()
    .success()
    .stdout(contains("decision: keep (matches include[0])"));
}

fn write_index_config(env: &Env, extra_mixed: &str) -> String {
    let ytdlp = test_bin().join("yt-dlp");
    let text = format!(
        "[library]\nroot = \"{root}\"\n\n[ytdlp]\nmanaged = false\npath = \"{ytdlp}\"\n\n\
         [sources.sample]\nurl = \"https://www.youtube.com/playlist?list=PLXLpuA-bwY4s\"\n\n\
         [sources.mixed]\nurl = \"https://www.youtube.com/playlist?list=PLmixedmixedmixed\"\n\
         min_duration = \"60s\"\nmax_duration = \"4h\"\ncutoff = \"2026-01-01\"\n{extra_mixed}\n\
         [sources.broken]\nurl = \"https://www.youtube.com/playlist?list=PLdoesnotexist\"\n",
        root = env.path("media").display(),
        ytdlp = ytdlp.display()
    );
    let path = env.path("index.toml");
    fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn item_statuses(env: &Env, source: &str) -> Vec<(String, String)> {
    let conn =
        rusqlite::Connection::open(env.path("data/tidepool/libraries/default/state.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT v.title, i.status FROM items i JOIN videos v ON v.id = i.video_id WHERE i.source_key = ?1 ORDER BY i.position")
        .unwrap();
    stmt.query_map([source], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn index_records_each_source_and_explains_what_it_skipped() {
    let env = Env::new();
    let log = env.path("ytdlp.log");
    let file = write_index_config(&env, "exclude = ['(?i)trailer']");
    env.cmd(&["index", "--config", &file])
        .env("TIDEPOOL_FAKE_YTDLP_LOG", &log)
        .assert()
        .code(3)
        .stdout(
            contains("sample: 1 video (1 new): 1 to download")
                .and(contains(
                    "mixed: 12 videos (12 new): 2 to download, 1 to check before download, 9 skipped \
                     (5 unavailable, 1 excluded by a rule, 1 published before the cutoff, 1 too long, 1 too short)",
                )),
        )
        .stderr(contains("broken: yt-dlp failed: ERROR: [youtube:tab] PLdoesnotexist: The playlist does not exist."));

    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("--flat-playlist -J --no-update"), "{calls}");

    let mixed = item_statuses(&env, "mixed");
    assert_eq!(mixed[0], ("Lofi live radio 24/7".into(), "pending".into()));
    assert_eq!(mixed[5], ("No date yet".into(), "pending_check".into()));
    assert_eq!(mixed[11], ("Official trailer".into(), "filtered".into()));

    // Scoped, and nothing new the second time.
    env.cmd(&["index", "-s", "sample"])
        .assert()
        .success()
        .stdout("sample: 1 video: 1 to download\n");

    // Relaxed rules apply to videos not downloaded yet.
    let file = write_index_config(&env, "");
    env.cmd(&["index", "-s", "mixed", "--config", &file])
        .assert()
        .success()
        .stdout(contains("mixed: 12 videos: 3 to download"));
    assert_eq!(
        item_statuses(&env, "mixed")[11],
        ("Official trailer".into(), "pending".into())
    );
}

#[test]
fn index_needs_a_working_ytdlp() {
    let env = Env::new();
    let file = env.path("c.toml");
    fs::write(
        &file,
        "[ytdlp]\nmanaged = false\npath = \"/nonexistent/yt-dlp\"\n[sources.sample]\nurl = \"https://www.youtube.com/playlist?list=PLXLpuA-bwY4s\"\n",
    )
    .unwrap();
    env.cmd(&["index", "--config", &file.display().to_string()])
        .assert()
        .code(3)
        .stderr(contains(
            "sample: yt-dlp failed: cannot run /nonexistent/yt-dlp",
        ));
}
