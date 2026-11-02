//! Managed tools: download the release binary for this platform from
//! GitHub, verify it, install it atomically, keep the previous version for
//! rollback, and check for updates.
//!
//! - yt-dlp: the release's `SHA2-256SUMS` must be signed by yt-dlp's key
//!   (pinned in tidepool); files `yt-dlp-<channel>`.
//! - Deno: the newest 2.x release (at least 2.3.0, which yt-dlp needs); the
//!   zip must match the release's `<asset>.sha256sum` (Deno doesn't sign
//!   releases); files `deno`.
//!
//! Each tool has `<name>`, `<name>.previous` and `<name>.json` (state) in
//! `~/.local/share/tidepool/bin/`, plus a shared `update.lock`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::platform::{compare_versions, current_asset, current_deno_asset};
use super::verify::{Verifier, VerifyError, check_checksum, checksum_for, single_checksum};
use crate::config::YtdlpChannel;
use crate::paths::Dirs;

const GITHUB_API: &str = "https://api.github.com";
const SUMS: &str = "SHA2-256SUMS";
const SUMS_SIG: &str = "SHA2-256SUMS.sig";
const DENO_REPOSITORY: &str = "denoland/deno";
/// The Deno major tidepool installs, and the oldest release yt-dlp accepts.
const DENO_MAJOR: u64 = 2;
const DENO_MINIMUM: &str = "2.3.0";

/// A tool tidepool manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ytdlp(YtdlpChannel),
    Deno,
}

impl Tool {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tool::Ytdlp(_) => "yt-dlp",
            Tool::Deno => "Deno",
        }
    }

    /// The installed file's name in the bin dir.
    fn file_name(self) -> String {
        match self {
            Tool::Ytdlp(channel) => format!("yt-dlp-{}", channel.as_str()),
            Tool::Deno => "deno".into(),
        }
    }
}

/// What tidepool remembers about an installed binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    pub version: String,
    pub sha256: String,
    pub installed_at: Timestamp,
    pub last_check_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// A release rolled back from; not reinstalled until a newer one appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Installed {
        tool: &'static str,
        version: String,
    },
    Updated {
        tool: &'static str,
        from: String,
        to: String,
    },
    UpToDate {
        tool: &'static str,
        version: String,
    },
    /// The latest release was rolled back from; kept on `installed`.
    Skipped {
        tool: &'static str,
        version: String,
        installed: String,
    },
}

impl std::fmt::Display for UpdateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateOutcome::Installed { tool, version } => write!(f, "installed {tool} {version}"),
            UpdateOutcome::Updated { tool, from, to } => {
                write!(f, "updated {tool} {from} → {to}")
            }
            UpdateOutcome::UpToDate { tool, version } => {
                write!(f, "{tool} {version} is up to date")
            }
            UpdateOutcome::Skipped {
                tool,
                version,
                installed,
            } => write!(
                f,
                "{tool} {version} was rolled back, staying on {installed} until a newer release"
            ),
        }
    }
}

/// The binary to run, after making sure it's there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ensured {
    pub path: PathBuf,
    /// What an update check did, if one ran.
    pub outcome: Option<UpdateOutcome>,
    /// An update check that failed; the current binary is used anyway.
    pub warning: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Http(String),
    #[error("{0}")]
    Release(String),
    #[error(transparent)]
    Verify(#[from] VerifyError),
    #[error("the downloaded {0} doesn't run on this machine: {1}")]
    SmokeTest(&'static str, String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} isn't downloaded yet; run `tidepool tools update`")]
    NotInstalled(&'static str),
    #[error("there's no previous {0} version to roll back to")]
    NoPrevious(&'static str),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ManagerError + '_ {
    move |source| ManagerError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

impl GithubRelease {
    fn asset_url(&self, name: &str) -> Result<&str, ManagerError> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
            .ok_or_else(|| {
                ManagerError::Release(format!("release {} has no {name}", self.tag_name))
            })
    }
}

/// Deletes a file when dropped, unless disarmed.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The first line `<binary> --version` prints; it must exit 0.
///
/// # Errors
///
/// When `binary` can't be run, exits with an error, or prints nothing.
pub fn tool_version(binary: &Path) -> Result<String, String> {
    let output = run_fresh(binary, "--version").map_err(|e| e.to_string())?;
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if output.status.success() && !version.is_empty() {
        Ok(version)
    } else {
        Err(format!("`--version` exited with {}", output.status))
    }
}

/// Run a binary that was just written. On Linux, a child another thread is
/// starting can briefly hold a copy of the file still open for writing, and
/// running it then fails with "Text file busy" until that child has started
/// (rust-lang/rust#114554); so wait a little and try again.
fn run_fresh(binary: &Path, arg: &str) -> std::io::Result<std::process::Output> {
    let mut wait = std::time::Duration::from_millis(10);
    for _ in 0..8 {
        match std::process::Command::new(binary).arg(arg).output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            result => return result,
        }
    }
    std::process::Command::new(binary).arg(arg).output()
}

/// Run `<binary> --version`; it must exit 0 and print something.
///
/// # Errors
///
/// When it doesn't (see [`tool_version`]).
pub fn default_smoke_test(binary: &Path) -> Result<(), String> {
    tool_version(binary).map(drop)
}

pub struct Manager {
    bin_dir: PathBuf,
    api_base: String,
    verifier: Verifier,
    ytdlp_asset: Result<&'static str, String>,
    deno_asset: Result<String, String>,
    client: reqwest::Client,
}

impl Manager {
    /// The real thing: GitHub, yt-dlp's signing key, this machine's binaries.
    ///
    /// # Panics
    ///
    /// If the HTTP client can't be built (see [`Manager::with`]), or the
    /// embedded yt-dlp key is broken (see [`Verifier::ytdlp`]). Debug builds
    /// also panic when `TIDEPOOL_TEST_SIGNING_KEY` is set to a file that can't
    /// be read or isn't a public key.
    #[must_use]
    pub fn new(dirs: &Dirs) -> Self {
        #[allow(unused_mut)]
        let (mut api_base, mut verifier) = (GITHUB_API.to_string(), Verifier::ytdlp());
        // Debug builds (tests) can point at a fake GitHub with a test key.
        #[cfg(debug_assertions)]
        {
            if let Ok(base) = std::env::var("TIDEPOOL_TEST_GITHUB_API") {
                api_base = base;
            }
            if let Ok(key) = std::env::var("TIDEPOOL_TEST_SIGNING_KEY") {
                let armored =
                    std::fs::read_to_string(&key).expect("TIDEPOOL_TEST_SIGNING_KEY is readable");
                verifier =
                    Verifier::from_armored(&armored).expect("TIDEPOOL_TEST_SIGNING_KEY is a key");
            }
        }
        Self::with(
            dirs.bin_dir(),
            &api_base,
            verifier,
            current_asset(),
            current_deno_asset(),
        )
    }

    /// A manager for `bin_dir` that talks to `api_base` instead of GitHub,
    /// trusts `verifier`'s key and installs the given assets.
    ///
    /// # Panics
    ///
    /// If the HTTP client can't be built: its TLS backend fails to
    /// initialize, or the system's DNS configuration can't be loaded.
    #[must_use]
    pub fn with(
        bin_dir: PathBuf,
        api_base: &str,
        verifier: Verifier,
        ytdlp_asset: Result<&'static str, String>,
        deno_asset: Result<String, String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("tidepool/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .expect("HTTP client builds");
        Self {
            bin_dir,
            api_base: api_base.trim_end_matches('/').to_string(),
            verifier,
            ytdlp_asset,
            deno_asset,
            client,
        }
    }

    pub fn binary(&self, tool: Tool) -> PathBuf {
        self.bin_dir.join(tool.file_name())
    }

    fn sibling(&self, tool: Tool, suffix: &str) -> PathBuf {
        self.bin_dir.join(format!("{}{suffix}", tool.file_name()))
    }

    pub fn previous(&self, tool: Tool) -> PathBuf {
        self.sibling(tool, ".previous")
    }

    fn state_path(&self, tool: Tool) -> PathBuf {
        self.sibling(tool, ".json")
    }

    pub fn state(&self, tool: Tool) -> Option<InstallState> {
        let text = std::fs::read_to_string(self.state_path(tool)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save_state(&self, tool: Tool, state: &InstallState) -> Result<(), ManagerError> {
        let path = self.state_path(tool);
        let tmp = self.sibling(tool, ".json.tmp");
        let json = serde_json::to_string_pretty(state).expect("state serializes");
        std::fs::write(&tmp, json + "\n").map_err(io(&tmp))?;
        std::fs::rename(&tmp, &path).map_err(io(&path))
    }

    fn lock(&self) -> Result<crate::lock::LibraryLock, ManagerError> {
        Ok(crate::lock::acquire_blocking(
            &self.bin_dir.join("update.lock"),
        )?)
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, ManagerError> {
        self.client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| ManagerError::Http(format!("could not reach {url}: {e}")))
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, ManagerError> {
        let response = self.get(url).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ManagerError::Http(format!("could not download {url}: {e}")))?;
        Ok(bytes.to_vec())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, ManagerError> {
        self.get(url)
            .await?
            .json()
            .await
            .map_err(|e| ManagerError::Release(format!("unexpected response from {url}: {e}")))
    }

    /// The release to install: yt-dlp's latest on the channel; Deno's
    /// newest stable 2.x.
    async fn latest(&self, tool: Tool) -> Result<GithubRelease, ManagerError> {
        match tool {
            Tool::Ytdlp(channel) => {
                let url = format!(
                    "{}/repos/{}/releases/latest",
                    self.api_base,
                    channel.repository()
                );
                self.get_json(&url).await
            }
            Tool::Deno => {
                let url = format!(
                    "{}/repos/{DENO_REPOSITORY}/releases?per_page=100",
                    self.api_base
                );
                let releases: Vec<GithubRelease> = self.get_json(&url).await?;
                releases
                    .into_iter()
                    .filter(|r| !r.prerelease && !r.draft)
                    .filter(|r| deno_version_ok(&version_of(tool, &r.tag_name)))
                    .max_by(|a, b| {
                        compare_versions(
                            &version_of(tool, &a.tag_name),
                            &version_of(tool, &b.tag_name),
                        )
                    })
                    .ok_or_else(|| {
                        ManagerError::Release(format!(
                            "no Deno {DENO_MAJOR}.x release (at least {DENO_MINIMUM}) at {url}"
                        ))
                    })
            }
        }
    }

    /// Check for a new release now and install it if there is one.
    ///
    /// # Errors
    ///
    /// As for [`Manager::update_with_smoke_test`].
    pub async fn update(&self, tool: Tool, now: Timestamp) -> Result<UpdateOutcome, ManagerError> {
        self.update_with_smoke_test(tool, now, default_smoke_test)
            .await
    }

    /// [`Manager::update`] with an injectable smoke test (on top of the
    /// version check for Deno).
    ///
    /// # Errors
    ///
    /// When there's no release binary for this machine, the GitHub check or
    /// the download fails, the signature or checksum doesn't match, the new
    /// binary fails its smoke test, or installing it (files, lock, state)
    /// fails.
    pub async fn update_with_smoke_test(
        &self,
        tool: Tool,
        now: Timestamp,
        smoke_test: impl Fn(&Path) -> Result<(), String>,
    ) -> Result<UpdateOutcome, ManagerError> {
        let asset = match tool {
            Tool::Ytdlp(_) => self.ytdlp_asset.clone().map(str::to_string),
            Tool::Deno => self.deno_asset.clone(),
        }
        .map_err(ManagerError::Unsupported)?;
        std::fs::create_dir_all(&self.bin_dir).map_err(io(&self.bin_dir))?;
        let _lock = self.lock()?;

        let name = tool.name();
        let installed = self.state(tool).filter(|_| self.binary(tool).exists());
        let checked = |state: &InstallState| InstallState {
            last_check_at: now,
            ..state.clone()
        };
        let release = match self.latest(tool).await {
            Ok(release) => release,
            Err(err) => {
                // A failed check still counts, so offline runs don't retry every time.
                if let Some(state) = &installed {
                    self.save_state(tool, &checked(state))?;
                }
                return Err(err);
            }
        };
        let version = version_of(tool, &release.tag_name);

        if let Some(state) = &installed {
            let outcome = if version == state.version
                || compare_versions(&version, &state.version) != std::cmp::Ordering::Greater
            {
                Some(UpdateOutcome::UpToDate {
                    tool: name,
                    version: state.version.clone(),
                })
            } else if state.skip_version.as_ref() == Some(&version) {
                Some(UpdateOutcome::Skipped {
                    tool: name,
                    version: version.clone(),
                    installed: state.version.clone(),
                })
            } else {
                None
            };
            if let Some(outcome) = outcome {
                self.save_state(tool, &checked(state))?;
                return Ok(outcome);
            }
        }

        self.install(tool, &asset, &release, installed.as_ref(), now, smoke_test)
            .await
    }

    /// The checksum the release promises for `asset`.
    async fn expected_checksum(
        &self,
        tool: Tool,
        asset: &str,
        release: &GithubRelease,
    ) -> Result<String, ManagerError> {
        match tool {
            Tool::Ytdlp(_) => {
                let sums = self.get_bytes(release.asset_url(SUMS)?).await?;
                let signature = self.get_bytes(release.asset_url(SUMS_SIG)?).await?;
                self.verifier.verify(&sums, &signature)?;
                Ok(checksum_for(&String::from_utf8_lossy(&sums), asset)?)
            }
            Tool::Deno => {
                let sum = self
                    .get_bytes(release.asset_url(&format!("{asset}.sha256sum"))?)
                    .await?;
                Ok(single_checksum(&String::from_utf8_lossy(&sum), asset)?)
            }
        }
    }

    async fn install(
        &self,
        tool: Tool,
        asset: &str,
        release: &GithubRelease,
        installed: Option<&InstallState>,
        now: Timestamp,
        smoke_test: impl Fn(&Path) -> Result<(), String>,
    ) -> Result<UpdateOutcome, ManagerError> {
        let name = tool.name();
        let expected = self.expected_checksum(tool, asset, release).await?;

        // Download next to the target, so the final rename is atomic.
        let pid = std::process::id();
        let download = self.sibling(tool, &format!(".download-{pid}"));
        let download_guard = TempFile(Some(download.clone()));
        let url = release.asset_url(asset)?;
        let mut response = self.get(url).await?;
        let mut file = std::fs::File::create(&download).map_err(io(&download))?;
        let mut hasher = Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ManagerError::Http(format!("could not download {url}: {e}")))?
        {
            hasher.update(&chunk);
            file.write_all(&chunk).map_err(io(&download))?;
        }
        file.sync_all().map_err(io(&download))?;
        drop(file);
        let actual = crate::hex::encode(&hasher.finalize());
        check_checksum(asset, &expected, &actual)?;

        // Deno comes zipped: the binary is the `deno` entry.
        let (tmp, guard) = match tool {
            Tool::Ytdlp(_) => (download, download_guard),
            Tool::Deno => {
                let binary = self.sibling(tool, &format!(".unpacked-{pid}"));
                let guard = TempFile(Some(binary.clone()));
                unzip_entry(&download, "deno", &binary)?;
                drop(download_guard);
                (binary, guard)
            }
        };

        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(io(&tmp))?;
        let version = version_of(tool, &release.tag_name);
        if tool == Tool::Deno {
            check_deno_version(&tmp, &version).map_err(|e| ManagerError::SmokeTest(name, e))?;
        }
        smoke_test(&tmp).map_err(|e| ManagerError::SmokeTest(name, e))?;

        let binary = self.binary(tool);
        let previous = self.previous(tool);
        if installed.is_some() {
            let _ = std::fs::remove_file(&previous);
            std::fs::hard_link(&binary, &previous)
                .or_else(|_| std::fs::copy(&binary, &previous).map(drop))
                .map_err(io(&previous))?;
        }
        std::fs::rename(&tmp, &binary).map_err(io(&binary))?;
        let mut guard = guard;
        guard.0 = None;

        let sha256 = match tool {
            Tool::Ytdlp(_) => actual,
            Tool::Deno => file_sha256(&binary)?,
        };
        let state = InstallState {
            version,
            sha256,
            installed_at: now,
            last_check_at: now,
            previous_version: installed.map(|s| s.version.clone()),
            skip_version: None,
        };
        self.save_state(tool, &state)?;
        Ok(match installed {
            Some(old) => UpdateOutcome::Updated {
                tool: name,
                from: old.version.clone(),
                to: state.version,
            },
            None => UpdateOutcome::Installed {
                tool: name,
                version: state.version,
            },
        })
    }

    /// Make sure the binary exists (downloading it if needed) and check for
    /// an update when the last check is older than `update_check`. A failed
    /// check keeps the current binary and returns a warning.
    ///
    /// # Errors
    ///
    /// When the binary is missing and installing it fails (see
    /// [`Manager::update`]).
    pub async fn ensure(
        &self,
        tool: Tool,
        update_check: Duration,
        now: Timestamp,
    ) -> Result<Ensured, ManagerError> {
        let path = self.binary(tool);
        if !path.exists() {
            let outcome = self.update(tool, now).await?;
            return Ok(Ensured {
                path,
                outcome: Some(outcome),
                warning: None,
            });
        }
        let interval = SignedDuration::try_from(update_check).unwrap_or(SignedDuration::MAX);
        let due = self
            .state(tool)
            .and_then(|s| s.last_check_at.checked_add(interval).ok())
            .is_none_or(|next| now >= next);
        if !due {
            return Ok(Ensured {
                path,
                outcome: None,
                warning: None,
            });
        }
        match self.update(tool, now).await {
            Ok(outcome) => Ok(Ensured {
                path,
                outcome: Some(outcome),
                warning: None,
            }),
            Err(err) => Ok(Ensured {
                path,
                outcome: None,
                warning: Some(format!(
                    "could not check for a {} update: {err}",
                    tool.name()
                )),
            }),
        }
    }

    /// Swap the current and previous binaries; the version rolled back from
    /// isn't reinstalled until a newer release appears.
    ///
    /// # Errors
    ///
    /// [`ManagerError::NotInstalled`] or [`ManagerError::NoPrevious`] when
    /// there's nothing to swap, and an error when taking the lock, renaming
    /// the binaries or saving the state fails.
    pub fn rollback(&self, tool: Tool) -> Result<(String, String), ManagerError> {
        let name = tool.name();
        let binary = self.binary(tool);
        let previous = self.previous(tool);
        let state = self
            .state(tool)
            .filter(|_| binary.exists())
            .ok_or(ManagerError::NotInstalled(name))?;
        let older = state
            .previous_version
            .clone()
            .filter(|_| previous.exists())
            .ok_or(ManagerError::NoPrevious(name))?;
        let _lock = self.lock()?;

        let swap = self.sibling(tool, ".swap");
        std::fs::rename(&binary, &swap).map_err(io(&binary))?;
        std::fs::rename(&previous, &binary).map_err(io(&previous))?;
        std::fs::rename(&swap, &previous).map_err(io(&previous))?;

        let sha256 = file_sha256(&binary)?;
        let newer = state.version.clone();
        self.save_state(
            tool,
            &InstallState {
                version: older.clone(),
                sha256,
                previous_version: Some(newer.clone()),
                skip_version: Some(newer.clone()),
                ..state
            },
        )?;
        Ok((newer, older))
    }
}

/// A release tag as a version: Deno's `v2.9.8` → `2.9.8`.
fn version_of(tool: Tool, tag: &str) -> String {
    match tool {
        Tool::Ytdlp(_) => tag.to_string(),
        Tool::Deno => tag.trim_start_matches('v').to_string(),
    }
}

/// Deno 2.x, at least 2.3.0.
fn deno_version_ok(version: &str) -> bool {
    let major = version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u64>().ok());
    major == Some(DENO_MAJOR) && compare_versions(version, DENO_MINIMUM) != std::cmp::Ordering::Less
}

/// `deno --version` must report the release's version.
fn check_deno_version(binary: &Path, expected: &str) -> Result<(), String> {
    // "deno 2.9.8 (stable, release, aarch64-apple-darwin)"
    let line = tool_version(binary)?;
    let reported = line.split_whitespace().nth(1).unwrap_or_default();
    if reported != expected {
        return Err(format!("it reports {line:?}, expected deno {expected}"));
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, ManagerError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    Ok(crate::hex::encode(&Sha256::digest(&bytes)))
}

/// Extract one file from a zip archive.
fn unzip_entry(zip: &Path, entry: &str, to: &Path) -> Result<(), ManagerError> {
    let file = std::fs::File::open(zip).map_err(io(zip))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| ManagerError::Release(format!("{} isn't a valid zip: {e}", zip.display())))?;
    let mut inner = archive
        .by_name(entry)
        .map_err(|_| ManagerError::Release(format!("the Deno archive has no {entry}")))?;
    let mut out = std::fs::File::create(to).map_err(io(to))?;
    std::io::copy(&mut inner, &mut out).map_err(io(to))?;
    out.sync_all().map_err(io(to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deno_versions() {
        assert!(deno_version_ok("2.3.0"));
        assert!(deno_version_ok("2.9.8"));
        assert!(deno_version_ok("2.10.0"));
        assert!(!deno_version_ok("2.2.9"), "older than yt-dlp needs");
        assert!(!deno_version_ok("3.0.0"), "a new major waits for tidepool");
        assert!(!deno_version_ok("1.46.3"));
        assert_eq!(version_of(Tool::Deno, "v2.9.8"), "2.9.8");
    }
}
