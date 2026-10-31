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
             [playlists.lofi]\nid = \"PLaaaaaaaaaaaa\"\ninclude = ['(?i)\\blive\\b']\nexclude = ['(?i)trailer']\n{extra}",
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
        "[library]\nconcurency = 2\n\n[playlists.lofi]\nid = 'PLx'\nmin_duration = 'soon'\n",
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
                "{file}:6:16 playlists.lofi.min_duration: \"soon\" is not a valid duration"
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
            contains("[playlists.lofi]")
                .and(contains("cover_name = \"{title}-poster\""))
                .and(contains("AIza…x1Qw")),
        )
        .stderr(contains("yt-dlp 2026.10.01").and(contains("is valid (1 playlists)")));

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
        .stdout(contains("+ playlists.lofi.enabled = false").and(contains("+enabled = false")));
}

#[test]
fn filter_test_explains_the_decision() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    let run = |title: &str, playlist: &str| {
        env.cmd(&["filter", "test", title, "-p", playlist, "--config", &file])
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
        "--playlist \"PLzzzzzzzzzzzz\" matches no configured playlist (configured: lofi)",
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
    env.cmd(&["sync"])
        .assert()
        .code(1)
        .stderr(contains("not implemented yet (planned for M2)"));
    env.cmd(&["--bogus"]).assert().code(2);
    env.cmd(&["sync", "-p", "Not A Key"]).assert().code(2);
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
    env.cmd(&[
        "init",
        "--yes",
        "--root",
        "~/Videos",
        "-p",
        "PLAbc_DEF",
        "-p",
        "https://www.youtube.com/playlist?list=PLxyz",
        "--api-key",
        "AIzaSyD-1234567890x1Qw",
    ])
    .assert()
    .success()
    .stdout(
        contains(format!("Created {}", file.display()))
            .and(contains("Created library \"default\" (config revision 1)"))
            .and(contains(
                "tidepool run     # daemon: sync every 6h, web UI at http://127.0.0.1:8787",
            ))
            .and(contains("--on-config-mismatch import")),
    )
    .stderr(contains("will be downloaded on first use").and(contains("ffmpeg version 9.0.2-fake")));

    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("#:schema https://raw.githubusercontent.com/fnando/tidepool/v"));
    assert!(
        text.contains("[playlists.plabc_def]\nid = \"PLAbc_DEF\""),
        "{text}"
    );
    assert!(
        text.contains("[playlists.plxyz]\nurl = \"https://www.youtube.com/playlist?list=PLxyz\"")
    );
    assert!(text.contains("root = \"~/Videos\""));

    // The library holds the same text as revision 1.
    env.cmd(&["config", "--show-secrets"])
        .assert()
        .success()
        .stdout(text);
    env.cmd(&["config", "history"])
        .assert()
        .success()
        .stdout(contains("*    1  "));
}

#[test]
fn init_never_overwrites() {
    let env = Env::new();
    env.cmd(&["init", "--yes"]).assert().success();
    let file = env.path("config/tidepool/config.toml");
    let original = fs::read_to_string(&file).unwrap();

    // The library already has a config, even with --force.
    for args in [&["init", "--yes"][..], &["init", "--yes", "--force"]] {
        env.cmd(args).assert().code(1).stderr(contains(
            "library \"default\" already has a config (revision 1)",
        ));
    }
    // A new library, but the file exists.
    env.cmd(&["init", "--yes", "-l", "work"])
        .assert()
        .code(1)
        .stderr(contains("already exists").and(contains("pass --force to replace it")));
    assert!(!env.path("data/tidepool/libraries/work/state.db").exists());

    // --force keeps the old file as .bak.
    env.cmd(&[
        "init",
        "--yes",
        "-l",
        "work",
        "--force",
        "--no-server",
        "--interval",
        "",
    ])
    .assert()
    .success()
    .stdout(contains("tidepool sync --library work").and(contains("tidepool run").not()));
    assert_eq!(
        fs::read_to_string(env.path("config/tidepool/config.toml.bak")).unwrap(),
        original
    );
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("enabled = false")
    );
}

#[test]
fn init_with_a_custom_path_and_library() {
    let env = Env::new();
    let file = env.path("elsewhere/tp.toml").display().to_string();
    env.cmd(&["init", "--yes", "--config", &file, "--library", "music"])
        .assert()
        .success()
        .stdout(contains("tidepool sync --library music").and(contains("Add playlists to")));
    env.cmd(&["config", "-l", "music"])
        .assert()
        .success()
        .stdout(contains("# No playlists yet"));
}

#[test]
fn init_print_and_dry_run_create_nothing() {
    let env = Env::new();
    env.cmd(&["init", "--yes", "--print", "-p", "PLaa"])
        .assert()
        .success()
        .stdout(contains("[playlists.plaa]").and(contains("# [playlists.example]")));
    env.cmd(&["init", "--yes", "--dry-run"])
        .assert()
        .success()
        .stderr(contains("dry run"));
    assert!(!env.path("config").exists());
    assert!(!env.path("data").exists());
}

#[test]
fn init_needs_a_terminal_or_yes() {
    let env = Env::new();
    env.cmd(&["init"])
        .assert()
        .code(1)
        .stderr(contains("pass --yes"));
    assert!(!env.path("config/tidepool/config.toml").exists());
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
