//! End-to-end: run the binary against a temp home.

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
            "# my config\n[category.default]\nmedia_dir = \"{}\"\n\n[tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n\n\
             [category.default.youtube]\napi_key = \"AIzaSyD-1234567890x1Qw\"\n\n\
             [sources.lofi]\nurls = [\"https://www.youtube.com/playlist?list=PLaaaaaaaaaaaa\"]\ninclude = ['(?i)\\blive\\b']\nexclude = ['(?i)trailer']\n{extra}",
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
        "tidepool has no config yet; run `tidepool init` (or `tidepool config import <path>`) first",
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
        "[general]\nconcurency = 2\n\n[sources.lofi]\nurls = ['https://www.youtube.com/playlist?list=PLx']\nmin_duration = 'soon'\n",
    )
    .unwrap();
    let file = file.display().to_string();
    env.cmd(&["config", "import", &file])
        .assert()
        .code(1)
        .stderr(
            contains(format!(
                "{file}:2:1 general.concurency: unknown key \"concurency\""
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
                .and(contains(
                    "cover_file = \"{output_dir}/{normalized_title}-poster.jpg\"",
                ))
                .and(contains("AIza…x1Qw")),
        )
        .stderr(contains("yt-dlp 2026.10.01").and(contains("is valid (1 sources)")));

    // Without a path: the file last imported, and the stored revision.
    env.cmd(&["config", "import", &file]).assert().success();
    env.cmd(&["config", "check"]).assert().success().stderr(
        contains(format!("{file} is valid (1 sources)"))
            .and(contains(
                "the stored config (revision 1) is valid and matches",
            ))
            .and(contains("differ").not()),
    );
}

#[test]
fn check_reports_both_configs_and_whether_they_differ() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file]).assert().success();

    // The file changed: out of sync, but both valid.
    let text = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        text.replace("include = ['(?i)\\blive\\b']", "include = ['(?i)concert']"),
    )
    .unwrap();
    env.cmd(&["config", "check"]).assert().success().stderr(
        contains(format!("{file} is valid"))
            .and(contains(format!(
                "{file} and the stored config (revision 1) differ: run `tidepool config import` to apply it (`tidepool config diff` shows what changed)"
            ))),
    );

    // The stored config no longer loads (e.g. written by an older tidepool).
    let conn = rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
    conn.execute(
        "UPDATE config_revisions SET toml = '[ytdlp]\nmanaged = false\n',
           json = '{\"ytdlp\":{\"managed\":false}}', source_hash = 'old' WHERE rev = 1",
        [],
    )
    .unwrap();
    env.cmd(&["config", "check"]).assert().code(1).stderr(
        contains(format!("{file} is valid"))
            .and(contains("the stored config (revision 1) no longer loads"))
            .and(contains("ytdlp: unknown key \"ytdlp\""))
            .and(contains(
                "fix it in your config file and run `tidepool config import`",
            ))
            .and(contains("differ")),
    );

    // Other commands say the same.
    env.cmd(&["filter", "test", "x", "-s", "lofi", "--skip-config-check"])
        .assert()
        .code(1)
        .stderr(
            contains("the stored config (revision 1) no longer loads with this tidepool").and(
                contains("fix it in your config file and run `tidepool config import`"),
            ),
        );

    // An invalid file is reported too, with its line numbers.
    fs::write(&file, "[general]\nconcurrency = 0\n").unwrap();
    env.cmd(&["config", "check"]).assert().code(1).stderr(
        contains(format!("{file}:2:15 general.concurrency")).and(contains("no longer loads")),
    );

    // Importing the fixed file brings both back in step.
    fs::write(&file, &text).unwrap();
    env.cmd(&["config", "import", &file]).assert().success();
    env.cmd(&["config", "check"])
        .assert()
        .success()
        .stderr(contains(
            "the stored config (revision 2) is valid and matches",
        ));
}

#[test]
fn check_fails_when_ytdlp_is_missing() {
    let env = Env::new();
    let file = env.path("c.toml");
    fs::write(
        &file,
        "[tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = '/nonexistent/yt-dlp'\n",
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
    let _held = tidepool::lock::acquire(&env.path("data/tidepool/lock")).unwrap();
    env.cmd(&["config", "import", &file])
        .assert()
        .code(1)
        .stderr(contains(format!(
            "another tidepool is running (pid {})",
            std::process::id()
        )));
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
    env.cmd(&["list"])
        .assert()
        .code(1)
        .stderr(contains("not implemented yet (planned for M6)"));
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
    env.cmd(&["--library", "work", "config"]).assert().code(2);
}

#[test]
fn dry_run_import_changes_nothing() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file, "--dry-run"])
        .assert()
        .success()
        .stdout(contains("dry run"));
    assert!(!env.path("data/tidepool/state.db").exists());
}

#[test]
fn config_flag_applies_to_every_command() {
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
fn init_creates_config_and_state() {
    let env = Env::new();
    let file = env.path("config/tidepool/config.toml");
    env.cmd(&["init"])
        .assert()
        .success()
        .stdout(
            contains(format!("Created {}", file.display()))
                .and(contains("Stored it as config revision 1"))
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
         urls = [\"https://www.youtube.com/playlist?list=PLXLpuA-bwY4s\"]   # playlist or channel URLs (https://www.youtube.com/@name), saved together\n\
         # category = \"default\""
    ));

    // The stored config holds the same text as revision 1.
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
fn init_never_overwrites_without_force() {
    let env = Env::new();
    env.cmd(&["init"]).assert().success();
    let file = env.path("config/tidepool/config.toml");
    fs::write(&file, "# my edits\n").unwrap();

    // Stored config and a file: nothing is replaced without --force.
    env.cmd(&["init"]).assert().code(1).stderr(
        contains("tidepool already has a config (revision 1)").and(contains("pass --force")),
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "# my edits\n");

    // No stored config, but a file: still not replaced.
    fs::remove_dir_all(env.path("data")).unwrap();
    env.cmd(&["init"]).assert().code(1).stderr(
        contains("already exists")
            .and(contains(format!(
                "tidepool config import {}",
                file.display()
            )))
            .and(contains("pass --force")),
    );
    assert!(!env.path("data/tidepool/state.db").exists());
    assert_eq!(fs::read_to_string(&file).unwrap(), "# my edits\n");
}

#[test]
fn init_writes_the_stored_config_when_the_file_is_missing() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file]).assert().success();
    let stored = fs::read_to_string(&file).unwrap();
    let target = env.path("config/tidepool/config.toml");
    env.cmd(&["init"])
        .assert()
        .success()
        .stdout(contains(format!(
            "Wrote the stored config (revision 1) to {}",
            target.display()
        )));
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        stored,
        "secrets included: it's your own file"
    );
    let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    env.cmd(&["config", "history"])
        .assert()
        .success()
        .stdout(contains("*    1  ").and(contains("     2  ").not()));
}

fn backups(dir: &Path) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "bak"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
    found.sort();
    found
}

#[test]
fn init_force_backs_up_and_replaces_the_file() {
    let env = Env::new();
    env.cmd(&["init"]).assert().success();
    let dir = env.path("config/tidepool");
    let file = dir.join("config.toml");
    fs::write(
        &file,
        "# my edits\n[category.default.youtube]\napi_key = \"AIzaSyD-1234567890x1Qw\"\n",
    )
    .unwrap();
    env.cmd(&["config", "import"]).assert().success();

    let assert = env.cmd(&["init", "--force"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let found = backups(&dir);
    assert_eq!(found.len(), 1, "{found:?}");
    let (name, content) = &found[0];
    assert!(content.starts_with("# my edits"));
    // config.<UTC timestamp>.bak, e.g. config.20261031T213045Z.bak
    let stamp = name
        .strip_prefix("config.")
        .and_then(|n| n.strip_suffix(".bak"))
        .unwrap();
    assert!(
        stamp.len() == 16 && stamp.as_bytes()[8] == b'T' && stamp.ends_with('Z'),
        "{name}"
    );
    for expected in [
        format!("Backed up {} to {}", file.display(), dir.join(name).display()),
        format!("Wrote a fresh config to {}", file.display()),
        "-# my edits".to_string(),
        "+version = 1".to_string(),
        "-api_key = \"AIza…x1Qw\"".to_string(),
        "The stored config (revision 2) is unchanged; apply the new file with `tidepool config import`".to_string(),
    ] {
        assert!(stdout.contains(&expected), "missing {expected:?} in:\n{stdout}");
    }
    assert!(!stdout.contains("1234567890"), "secrets masked in the diff");
    assert!(
        !stdout.contains('\u{1b}'),
        "no colours when stdout isn't a terminal"
    );
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("[sources.sample]")
    );
    env.cmd(&["config", "history"])
        .assert()
        .success()
        .stdout(contains("*    2  ").and(contains("     3  ").not()));
    // Until it's imported, runs stop at the drift check.
    env.cmd(&["filter", "test", "x", "-s", "sample"])
        .assert()
        .code(1)
        .stderr(contains("aren't applied yet"));

    // Again in the same second: a numbered backup, never overwriting the first.
    fs::write(&file, "# more edits\n").unwrap();
    env.cmd(&["init", "--force"]).assert().success();
    let found = backups(&dir);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().any(|(_, c)| c == "# more edits\n"));

    // With no stored config, --force also stores the new file as revision 1.
    fs::remove_dir_all(env.path("data")).unwrap();
    env.cmd(&["init", "--force"])
        .assert()
        .success()
        .stdout(contains("Stored it as config revision 1\n"));
    assert_eq!(backups(&dir).len(), 3);
}

#[test]
fn init_with_a_custom_path() {
    let env = Env::new();
    let file = env.path("elsewhere/tp.toml").display().to_string();
    env.cmd(&["init", "--config", &file])
        .assert()
        .success()
        .stdout(contains(format!("tidepool config import {file}\n")));
    env.cmd(&["config"])
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
        "[category.default]\nmedia_dir = \"{root}\"\n\n[tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{ytdlp}\"\n\n\
         [sources.sample]\nurls = [\"https://www.youtube.com/playlist?list=PLXLpuA-bwY4s\"]\n\n\
         [sources.mixed]\nurls = [\"https://www.youtube.com/playlist?list=PLmixedmixedmixed\"]\n\
         min_duration = \"60s\"\nmax_duration = \"4h\"\ncutoff = \"2026-01-01\"\n{extra_mixed}\n\
         [sources.broken]\nurls = [\"https://www.youtube.com/playlist?list=PLdoesnotexist\"]\n",
        root = env.path("media").display(),
        ytdlp = ytdlp.display()
    );
    let path = env.path("index.toml");
    fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn item_statuses(env: &Env, source: &str) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(env.path("data/tidepool/state.db")).unwrap();
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
        "[tools.deno]\nmanaged = false\n\n[tools.yt-dlp]\nmanaged = false\npath = \"/nonexistent/yt-dlp\"\n[sources.sample]\nurls = [\"https://www.youtube.com/playlist?list=PLXLpuA-bwY4s\"]\n",
    )
    .unwrap();
    env.cmd(&["index", "--config", &file.display().to_string()])
        .assert()
        .code(3)
        .stderr(contains(
            "sample: yt-dlp failed: cannot run /nonexistent/yt-dlp",
        ));
}

#[test]
fn config_drift_halts_until_applied_or_skipped() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file]).assert().success();
    let run = |args: &[&str]| {
        let mut all = vec!["filter", "test", "Live set", "-s", "lofi"];
        all.extend_from_slice(args);
        env.cmd(&all).assert()
    };
    run(&[]).success();

    // Comments and formatting don't count.
    let text = fs::read_to_string(&file).unwrap();
    fs::write(&file, format!("# a new comment\n{text}")).unwrap();
    run(&[]).success();

    // A real change halts, listing it with secrets masked.
    let text = fs::read_to_string(&file)
        .unwrap()
        .replace("AIzaSyD-1234567890x1Qw", "AIzaSyD-0000000000z9Zz")
        + "enabled = false\n";
    fs::write(&file, text).unwrap();
    run(&[]).code(1).stderr(
        contains(format!("{file} has changes that aren't applied yet"))
            .and(contains(
                "~ category.default.youtube.api_key: \"AIza…x1Qw\" → \"AIza…z9Zz\"",
            ))
            .and(contains("+ sources.lofi.enabled = false"))
            .and(contains("tidepool config import"))
            .and(contains("--skip-config-check"))
            .and(contains("0000000000").not()),
    );
    run(&["--skip-config-check"]).success();
    // Config commands are how you fix it, so they never halt.
    env.cmd(&["config", "history"]).assert().success();
    env.cmd(&["config", "diff"]).assert().success();
    env.cmd(&["config", "import"])
        .env("TIDEPOOL_CONFIG", &file)
        .assert()
        .success();
    run(&[]).success();

    // A file with errors halts too, and says why.
    fs::write(&file, fs::read_to_string(&file).unwrap() + "bogus = 1\n").unwrap();
    run(&[])
        .code(1)
        .stderr(contains("has errors").and(contains("unknown key \"bogus\"")));
}

#[test]
fn config_drift_after_a_rollback_explains_how_to_keep_it() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file]).assert().success();
    env.write_config("c.toml", "enabled = false\n");
    env.cmd(&["config", "import", &file]).assert().success();
    env.cmd(&["config", "rollback", "1"]).assert().success();
    env.cmd(&["filter", "test", "x", "-s", "lofi"])
        .assert()
        .code(1)
        .stderr(
            contains("the stored config came from a rollback (revision 3)").and(contains(format!(
                "tidepool config export --show-secrets > {file}"
            ))),
        );
}

#[test]
fn config_flag_means_no_drift() {
    let env = Env::new();
    let file = env.write_config("c.toml", "");
    env.cmd(&["config", "import", &file]).assert().success();
    env.write_config("c.toml", "enabled = false\n");
    env.cmd(&["filter", "test", "x", "-s", "lofi", "--config", &file])
        .assert()
        .success();
}
