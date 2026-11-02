//! Managed yt-dlp against a fake GitHub (wiremock) serving signed fixture
//! releases from tests/fixtures/ytdlp/fake.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;
use tidepool::config::YtdlpChannel;
use tidepool::tools::manager::{Manager, ManagerError, Tool, UpdateOutcome};
use tidepool::tools::verify::{Verifier, VerifyError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ASSET: &str = "yt-dlp_linux";
const NIGHTLY_REPO: &str = "/repos/yt-dlp/yt-dlp-nightly-builds/releases/latest";

fn fixture(rel: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ytdlp/fake")
            .join(rel),
    )
    .unwrap()
}

fn test_verifier() -> Verifier {
    Verifier::from_armored(&String::from_utf8(fixture("test-signing-key.asc")).unwrap()).unwrap()
}

struct Github {
    server: MockServer,
}

impl Github {
    async fn start() -> Self {
        Self {
            server: MockServer::start().await,
        }
    }

    /// Serve `dir` (a fixture release) as the channel's latest release `tag`,
    /// replacing whatever was served before.
    async fn release(&self, tag: &str, dir: &str) {
        self.server.reset().await;
        let base = format!("{}/download/{tag}", self.server.uri());
        let assets: Vec<_> = [ASSET, "SHA2-256SUMS", "SHA2-256SUMS.sig"]
            .iter()
            .map(|name| json!({ "name": name, "browser_download_url": format!("{base}/{name}") }))
            .collect();
        Mock::given(method("GET"))
            .and(path(NIGHTLY_REPO))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "tag_name": tag, "assets": assets })),
            )
            .mount(&self.server)
            .await;
        for (name, file) in [
            (ASSET, "yt-dlp"),
            ("SHA2-256SUMS", "SHA2-256SUMS"),
            ("SHA2-256SUMS.sig", "SHA2-256SUMS.sig"),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/download/{tag}/{name}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_bytes(fixture(&format!("{dir}/{file}"))),
                )
                .mount(&self.server)
                .await;
        }
    }

    async fn down(&self) {
        self.server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&self.server)
            .await;
    }

    fn manager(&self, bin: &Path) -> Manager {
        Manager::with(
            bin.to_path_buf(),
            &self.server.uri(),
            test_verifier(),
            Ok(ASSET),
            Ok("deno-x86_64-unknown-linux-gnu.zip".into()),
        )
    }
}

fn version_of(binary: &Path) -> String {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

const CH: Tool = Tool::Ytdlp(YtdlpChannel::Nightly);

#[tokio::test]
async fn installs_then_updates_keeping_the_previous_version() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());
    assert!(manager.state(CH).is_none());

    gh.release("2026.01.01", "2026.01.01").await;
    let outcome = manager
        .update(CH, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Installed {
            tool: "yt-dlp",
            version: "2026.01.01".into()
        }
    );
    let binary: PathBuf = manager.binary(CH);
    assert_eq!(binary, bin.path().join("yt-dlp-nightly"));
    assert_eq!(version_of(&binary), "2026.01.01");
    assert_eq!(
        std::fs::metadata(&binary).unwrap().permissions().mode() & 0o111,
        0o111
    );
    let state = manager.state(CH).unwrap();
    assert_eq!(state.version, "2026.01.01");
    assert_eq!(state.last_check_at, ts("2026-10-31T10:00:00Z"));

    // Same release: nothing to do, but the check time moves.
    let outcome = manager
        .update(CH, ts("2026-10-31T11:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::UpToDate {
            tool: "yt-dlp",
            version: "2026.01.01".into()
        }
    );
    assert_eq!(
        manager.state(CH).unwrap().last_check_at,
        ts("2026-10-31T11:00:00Z")
    );

    gh.release("2026.02.01", "2026.02.01").await;
    let outcome = manager
        .update(CH, ts("2026-10-31T12:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Updated {
            tool: "yt-dlp",
            from: "2026.01.01".into(),
            to: "2026.02.01".into()
        }
    );
    assert_eq!(version_of(&binary), "2026.02.01");
    assert_eq!(
        manager.state(CH).unwrap().previous_version.as_deref(),
        Some("2026.01.01")
    );
    assert_eq!(
        version_of(&bin.path().join("yt-dlp-nightly.previous")),
        "2026.01.01"
    );
}

#[tokio::test]
async fn rejects_bad_checksums_and_signatures_without_touching_the_current_binary() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());

    gh.release("2026.02.01", "tampered").await;
    let err = manager
        .update(CH, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ManagerError::Verify(VerifyError::ChecksumMismatch { .. })
        ),
        "{err}"
    );
    assert!(!manager.binary(CH).exists());

    gh.release("2026.01.01", "2026.01.01").await;
    manager
        .update(CH, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap();

    // A release signed by another key (the real yt-dlp key isn't ours here).
    let other = Manager::with(
        bin.path().to_path_buf(),
        &gh.server.uri(),
        Verifier::ytdlp(),
        Ok(ASSET),
        Ok("deno-x86_64-unknown-linux-gnu.zip".into()),
    );
    gh.release("2026.02.01", "2026.02.01").await;
    let err = other
        .update(CH, ts("2026-10-31T11:00:00Z"))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "SHA2-256SUMS has no valid signature from the yt-dlp signing key"
    );
    assert_eq!(version_of(&manager.binary(CH)), "2026.01.01");

    // No leftovers from failed downloads.
    let names: Vec<String> = std::fs::read_dir(bin.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains("download")), "{names:?}");
}

#[tokio::test]
async fn a_binary_that_fails_its_smoke_test_is_not_installed() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());

    // A correctly signed binary that doesn't run on this machine: only the
    // smoke test catches it, so inject a failing one.
    gh.release("2026.01.01", "2026.01.01").await;
    let err = manager
        .update_with_smoke_test(CH, ts("2026-10-31T10:00:00Z"), |_| {
            Err("exit status: 1".into())
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ManagerError::SmokeTest(_, _)), "{err}");
    assert!(!manager.binary(CH).exists());
}

#[tokio::test]
async fn rollback_restores_the_previous_version_until_a_newer_release() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());
    assert!(matches!(
        manager.rollback(CH),
        Err(ManagerError::NotInstalled(_))
    ));

    gh.release("2026.01.01", "2026.01.01").await;
    manager
        .update(CH, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap();
    assert!(matches!(
        manager.rollback(CH),
        Err(ManagerError::NoPrevious(_))
    ));

    gh.release("2026.02.01", "2026.02.01").await;
    manager
        .update(CH, ts("2026-10-31T11:00:00Z"))
        .await
        .unwrap();
    let (from, to) = manager.rollback(CH).unwrap();
    assert_eq!((from.as_str(), to.as_str()), ("2026.02.01", "2026.01.01"));
    assert_eq!(version_of(&manager.binary(CH)), "2026.01.01");

    // The rolled-back release isn't reinstalled.
    let outcome = manager
        .update(CH, ts("2026-10-31T12:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Skipped {
            tool: "yt-dlp",
            version: "2026.02.01".into(),
            installed: "2026.01.01".into()
        }
    );
    assert_eq!(version_of(&manager.binary(CH)), "2026.01.01");
}

#[tokio::test]
async fn ensure_installs_when_missing_and_checks_only_when_due() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());
    let hour = Duration::from_secs(3600);

    gh.down().await;
    let err = manager
        .ensure(CH, hour, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap_err();
    assert!(matches!(err, ManagerError::Http(_)), "{err}");

    gh.release("2026.01.01", "2026.01.01").await;
    let ensured = manager
        .ensure(CH, hour, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap();
    assert_eq!(ensured.path, manager.binary(CH));
    assert_eq!(
        ensured.outcome,
        Some(UpdateOutcome::Installed {
            tool: "yt-dlp",
            version: "2026.01.01".into()
        })
    );

    // Not due yet: no request at all, even with GitHub down.
    gh.down().await;
    let ensured = manager
        .ensure(CH, hour, ts("2026-10-31T10:30:00Z"))
        .await
        .unwrap();
    assert_eq!((ensured.outcome, ensured.warning), (None, None));

    // Due, but GitHub fails: keep using the current binary, with a warning.
    let ensured = manager
        .ensure(CH, hour, ts("2026-10-31T11:00:00Z"))
        .await
        .unwrap();
    assert_eq!(ensured.outcome, None);
    assert!(
        ensured
            .warning
            .unwrap()
            .contains("could not check for a yt-dlp update")
    );

    // Due and a new release exists.
    gh.release("2026.02.01", "2026.02.01").await;
    let ensured = manager
        .ensure(CH, hour, ts("2026-10-31T12:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        ensured.outcome,
        Some(UpdateOutcome::Updated {
            tool: "yt-dlp",
            from: "2026.01.01".into(),
            to: "2026.02.01".into()
        })
    );
}

#[tokio::test]
async fn unsupported_platforms_say_how_to_proceed() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = Manager::with(
        bin.path().to_path_buf(),
        &gh.server.uri(),
        test_verifier(),
        Err("no yt-dlp release binary for x".into()),
        Err("no Deno release binary for x".into()),
    );
    let err = manager
        .update(CH, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "no yt-dlp release binary for x");
}

// ---- `tidepool tools …` end to end -------------------------------------

fn tidepool(home: &Path, gh: &Github, args: &[&str]) -> assert_cmd::assert::Assert {
    let key = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ytdlp/fake/test-signing-key.asc");
    assert_cmd::Command::cargo_bin("tidepool")
        .unwrap()
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("TIDEPOOL_TEST_GITHUB_API", gh.server.uri())
        .env("TIDEPOOL_TEST_SIGNING_KEY", key)
        .args(args)
        .assert()
}

/// The asset name this machine would download; the fake release serves it.
async fn serve_for_this_machine(gh: &Github, tag: &str) {
    let asset = tidepool::tools::platform::current_asset().unwrap();
    gh.server.reset().await;
    let base = format!("{}/download/{tag}", gh.server.uri());
    let assets: Vec<_> = [asset, "SHA2-256SUMS", "SHA2-256SUMS.sig"]
        .iter()
        .map(|name| json!({ "name": name, "browser_download_url": format!("{base}/{name}") }))
        .collect();
    Mock::given(method("GET"))
        .and(path(NIGHTLY_REPO))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "tag_name": tag, "assets": assets })),
        )
        .mount(&gh.server)
        .await;
    for (name, file) in [
        (asset, "yt-dlp"),
        ("SHA2-256SUMS", "SHA2-256SUMS"),
        ("SHA2-256SUMS.sig", "SHA2-256SUMS.sig"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/download/{tag}/{name}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(fixture(&format!("{tag}/{file}"))),
            )
            .mount(&gh.server)
            .await;
    }
}

// ---- Deno ---------------------------------------------------------------

const DENO_RELEASES: &str = "/repos/denoland/deno/releases";

/// A zip holding a fake `deno` that reports `reports`.
fn deno_zip(reports: &str) -> Vec<u8> {
    use std::io::Write as _;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        zip.start_file("deno", options).unwrap();
        write!(zip, "#!/bin/sh\necho '{reports} (stable, release, test)'\n").unwrap();
        zip.finish().unwrap();
    }
    buf.into_inner()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Serve Deno releases `(tag, prerelease, zip)` for `asset`; the checksum
/// matches unless `bad_sum`.
async fn serve_deno(gh: &Github, asset: &str, releases: &[(&str, bool, Vec<u8>)], bad_sum: bool) {
    gh.server.reset().await;
    let list: Vec<_> = releases
        .iter()
        .map(|(tag, prerelease, _)| {
            let base = format!("{}/deno/{tag}", gh.server.uri());
            json!({
                "tag_name": tag,
                "prerelease": prerelease,
                "assets": [
                    { "name": asset, "browser_download_url": format!("{base}/{asset}") },
                    { "name": format!("{asset}.sha256sum"), "browser_download_url": format!("{base}/{asset}.sha256sum") },
                ],
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(DENO_RELEASES))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(list)))
        .mount(&gh.server)
        .await;
    for (tag, _, zip) in releases {
        let sum = if bad_sum {
            "0".repeat(64)
        } else {
            sha256_hex(zip)
        };
        Mock::given(method("GET"))
            .and(path(format!("/deno/{tag}/{asset}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip.clone()))
            .mount(&gh.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/deno/{tag}/{asset}.sha256sum")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sum}  {asset}\n")))
            .mount(&gh.server)
            .await;
    }
}

const DENO_ASSET: &str = "deno-x86_64-unknown-linux-gnu.zip";

#[tokio::test]
async fn deno_installs_the_newest_2x_and_updates() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());
    serve_deno(
        &gh,
        DENO_ASSET,
        &[
            ("v3.0.0", false, deno_zip("deno 3.0.0")),
            ("v2.10.0-rc.1", true, deno_zip("deno 2.10.0-rc.1")),
            ("v2.9.8", false, deno_zip("deno 2.9.8")),
            ("v2.2.0", false, deno_zip("deno 2.2.0")),
        ],
        false,
    )
    .await;
    let outcome = manager
        .update(Tool::Deno, ts("2026-10-31T10:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Installed {
            tool: "Deno",
            version: "2.9.8".into()
        },
        "the newest stable 2.x: not 3.0, not a prerelease"
    );
    let binary = bin.path().join("deno");
    assert!(version_of(&binary).starts_with("deno 2.9.8"));
    assert_eq!(manager.state(Tool::Deno).unwrap().version, "2.9.8");

    serve_deno(
        &gh,
        DENO_ASSET,
        &[("v2.9.9", false, deno_zip("deno 2.9.9"))],
        false,
    )
    .await;
    let outcome = manager
        .update(Tool::Deno, ts("2026-11-01T10:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Updated {
            tool: "Deno",
            from: "2.9.8".into(),
            to: "2.9.9".into()
        }
    );
    assert!(version_of(&bin.path().join("deno.previous")).starts_with("deno 2.9.8"));
    assert_eq!(
        manager.rollback(Tool::Deno).unwrap(),
        ("2.9.9".into(), "2.9.8".into())
    );
    assert!(version_of(&binary).starts_with("deno 2.9.8"));
}

#[tokio::test]
async fn deno_rejects_bad_checksums_wrong_binaries_and_old_releases() {
    let gh = Github::start().await;
    let bin = tempfile::tempdir().unwrap();
    let manager = gh.manager(bin.path());
    let now = ts("2026-10-31T10:00:00Z");

    serve_deno(
        &gh,
        DENO_ASSET,
        &[("v2.9.8", false, deno_zip("deno 2.9.8"))],
        true,
    )
    .await;
    let err = manager.update(Tool::Deno, now).await.unwrap_err();
    assert!(err.to_string().contains("checksum mismatch"), "{err}");

    // The zip claims 2.9.8 but holds something else.
    serve_deno(
        &gh,
        DENO_ASSET,
        &[("v2.9.8", false, deno_zip("deno 1.0.0"))],
        false,
    )
    .await;
    let err = manager.update(Tool::Deno, now).await.unwrap_err();
    assert!(
        err.to_string()
            .starts_with("the downloaded Deno doesn't run on this machine"),
        "{err}"
    );

    serve_deno(
        &gh,
        DENO_ASSET,
        &[("v2.2.0", false, deno_zip("deno 2.2.0"))],
        false,
    )
    .await;
    let err = manager.update(Tool::Deno, now).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("no Deno 2.x release (at least 2.3.0)"),
        "{err}"
    );
    assert!(!bin.path().join("deno").exists(), "nothing installed");
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_commands_end_to_end() {
    use predicates::prelude::*;
    use predicates::str::contains;

    let gh = Github::start().await;
    let home = tempfile::tempdir().unwrap();
    let home = home.path();

    tidepool(home, &gh, &["tools", "status"]).success().stdout(
        contains("yt-dlp isn't downloaded yet (managed, nightly channel)")
            .and(contains("Deno isn't downloaded yet (managed, 2.x")),
    );
    tidepool(home, &gh, &["tools", "rollback", "yt-dlp"])
        .code(1)
        .stderr(contains("yt-dlp isn't downloaded yet"));

    serve_for_this_machine(&gh, "2026.01.01").await;
    tidepool(home, &gh, &["tools", "update", "yt-dlp"])
        .success()
        .stdout(contains("installed yt-dlp 2026.01.01"));
    tidepool(home, &gh, &["tools", "update", "yt-dlp"])
        .success()
        .stdout(contains("yt-dlp 2026.01.01 is up to date"));

    serve_for_this_machine(&gh, "2026.02.01").await;
    tidepool(home, &gh, &["tools", "update", "yt-dlp"])
        .success()
        .stdout(contains("updated yt-dlp 2026.01.01 → 2026.02.01"));
    tidepool(home, &gh, &["tools", "status"]).success().stdout(
        contains("yt-dlp 2026.02.01 (managed, nightly channel)")
            .and(contains("data/tidepool/bin/yt-dlp-nightly"))
            .and(contains(
                "previous version: 2026.01.01 (`tidepool tools rollback yt-dlp`",
            )),
    );
    tidepool(home, &gh, &["tools", "rollback", "yt-dlp"])
        .success()
        .stdout(contains("rolled back yt-dlp 2026.02.01 → 2026.01.01"));
    tidepool(home, &gh, &["tools", "status"])
        .success()
        .stdout(contains("yt-dlp 2026.01.01").and(contains("2026.02.01 was rolled back")));

    // Deno, for this machine.
    let asset = tidepool::tools::platform::current_deno_asset().unwrap();
    serve_deno(
        &gh,
        &asset,
        &[("v2.9.8", false, deno_zip("deno 2.9.8"))],
        false,
    )
    .await;
    tidepool(home, &gh, &["tools", "update", "deno"])
        .success()
        .stdout(contains("installed Deno 2.9.8"));
    tidepool(home, &gh, &["tools", "status"]).success().stdout(
        contains("Deno 2.9.8 (managed, 2.x, the JavaScript runtime yt-dlp needs for YouTube)")
            .and(contains("data/tidepool/bin/deno")),
    );
    // Both at once: yt-dlp's release is gone, so that part fails.
    tidepool(home, &gh, &["tools", "update"])
        .code(3)
        .stdout(contains("yt-dlp: ").and(contains("Deno 2.9.8 is up to date")));
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_commands_with_your_own_tools() {
    use predicates::prelude::*;
    use predicates::str::contains;

    let gh = Github::start().await;
    let home = tempfile::tempdir().unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bin/yt-dlp");
    let config = home.path().join("c.toml");
    std::fs::write(
        &config,
        format!(
            "[tools.deno]\nmanaged = false\npath = \"/nonexistent/deno\"\n\n[tools.yt-dlp]\nmanaged = false\npath = \"{}\"\n",
            fake.display()
        ),
    )
    .unwrap();
    let config = config.display().to_string();

    tidepool(home.path(), &gh, &["tools", "status", "--config", &config])
        .success()
        .stdout(
            contains("yt-dlp 2026.10.01 at ")
                .and(contains("(your own; tidepool doesn't update it)"))
                .and(contains(
                    "Deno: cannot run \"/nonexistent/deno\" (tools.deno.path)",
                )),
        );
    for args in [["tools", "update", "yt-dlp"], ["tools", "rollback", "deno"]] {
        tidepool(home.path(), &gh, &args)
            .code(1)
            .stderr(contains("managed = false in [tools."));
    }
    tidepool(home.path(), &gh, &["tools", "update"])
        .success()
        .stdout(contains("yt-dlp is your own").and(contains("Deno is your own")));
}
