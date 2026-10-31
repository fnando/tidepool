//! Managed yt-dlp: download the release binary for this platform from
//! GitHub, verify it (signed checksums), install it atomically, keep the
//! previous version for rollback, and check for updates.
//!
//! Files in `~/.local/share/tidepool/bin/`, shared by all libraries:
//! `yt-dlp-<channel>` (current), `yt-dlp-<channel>.previous`,
//! `yt-dlp-<channel>.json` (state) and `update.lock`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::platform::{compare_versions, current_asset};
use super::verify::{Verifier, VerifyError, check_checksum, checksum_for};
use crate::config::YtdlpChannel;
use crate::paths::Dirs;

const GITHUB_API: &str = "https://api.github.com";
const SUMS: &str = "SHA2-256SUMS";
const SUMS_SIG: &str = "SHA2-256SUMS.sig";

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
        version: String,
    },
    Updated {
        from: String,
        to: String,
    },
    UpToDate {
        version: String,
    },
    /// The latest release was rolled back from; kept on `installed`.
    Skipped {
        version: String,
        installed: String,
    },
}

impl std::fmt::Display for UpdateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateOutcome::Installed { version } => write!(f, "installed yt-dlp {version}"),
            UpdateOutcome::Updated { from, to } => write!(f, "updated yt-dlp {from} → {to}"),
            UpdateOutcome::UpToDate { version } => write!(f, "yt-dlp {version} is up to date"),
            UpdateOutcome::Skipped { version, installed } => write!(
                f,
                "yt-dlp {version} was rolled back, staying on {installed} until a newer release"
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
    #[error("the downloaded yt-dlp doesn't run on this machine: {0}")]
    SmokeTest(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("yt-dlp isn't downloaded yet; run `tidepool ytdlp update`")]
    NotInstalled,
    #[error("there's no previous yt-dlp version to roll back to")]
    NoPrevious,
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
                ManagerError::Release(format!("yt-dlp release {} has no {name}", self.tag_name))
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
pub fn tool_version(binary: &Path) -> Result<String, String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
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

/// Run `<binary> --version`; it must exit 0 and print something.
pub fn default_smoke_test(binary: &Path) -> Result<(), String> {
    tool_version(binary).map(drop)
}

pub struct Manager {
    bin_dir: PathBuf,
    api_base: String,
    verifier: Verifier,
    asset: Result<&'static str, String>,
    client: reqwest::Client,
}

impl Manager {
    /// The real thing: GitHub, yt-dlp's signing key, this machine's binary.
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
        Self::with(dirs.bin_dir(), api_base, verifier, current_asset())
    }

    pub fn with(
        bin_dir: PathBuf,
        api_base: String,
        verifier: Verifier,
        asset: Result<&'static str, String>,
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
            asset,
            client,
        }
    }

    pub fn binary(&self, channel: YtdlpChannel) -> PathBuf {
        self.bin_dir.join(format!("yt-dlp-{}", channel.as_str()))
    }

    fn sibling(&self, channel: YtdlpChannel, suffix: &str) -> PathBuf {
        self.bin_dir
            .join(format!("yt-dlp-{}{suffix}", channel.as_str()))
    }

    pub fn previous(&self, channel: YtdlpChannel) -> PathBuf {
        self.sibling(channel, ".previous")
    }

    fn state_path(&self, channel: YtdlpChannel) -> PathBuf {
        self.sibling(channel, ".json")
    }

    pub fn state(&self, channel: YtdlpChannel) -> Option<InstallState> {
        let text = std::fs::read_to_string(self.state_path(channel)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save_state(&self, channel: YtdlpChannel, state: &InstallState) -> Result<(), ManagerError> {
        let path = self.state_path(channel);
        let tmp = self.sibling(channel, ".json.tmp");
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
            .and_then(|r| r.error_for_status())
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

    async fn latest(&self, channel: YtdlpChannel) -> Result<GithubRelease, ManagerError> {
        let url = format!(
            "{}/repos/{}/releases/latest",
            self.api_base,
            channel.repository()
        );
        self.get(&url)
            .await?
            .json()
            .await
            .map_err(|e| ManagerError::Release(format!("unexpected response from {url}: {e}")))
    }

    /// Check for a new release now and install it if there is one.
    pub async fn update(
        &self,
        channel: YtdlpChannel,
        now: Timestamp,
    ) -> Result<UpdateOutcome, ManagerError> {
        self.update_with_smoke_test(channel, now, default_smoke_test)
            .await
    }

    /// [`Manager::update`] with an injectable smoke test.
    pub async fn update_with_smoke_test(
        &self,
        channel: YtdlpChannel,
        now: Timestamp,
        smoke_test: impl Fn(&Path) -> Result<(), String>,
    ) -> Result<UpdateOutcome, ManagerError> {
        let asset = self.asset.clone().map_err(ManagerError::Unsupported)?;
        std::fs::create_dir_all(&self.bin_dir).map_err(io(&self.bin_dir))?;
        let _lock = self.lock()?;

        let installed = self
            .state(channel)
            .filter(|_| self.binary(channel).exists());
        let checked = |state: &InstallState| InstallState {
            last_check_at: now,
            ..state.clone()
        };
        let release = match self.latest(channel).await {
            Ok(release) => release,
            Err(err) => {
                // A failed check still counts, so offline runs don't retry every time.
                if let Some(state) = &installed {
                    self.save_state(channel, &checked(state))?;
                }
                return Err(err);
            }
        };

        if let Some(state) = &installed {
            let tag = &release.tag_name;
            let outcome = if *tag == state.version
                || compare_versions(tag, &state.version) != std::cmp::Ordering::Greater
            {
                Some(UpdateOutcome::UpToDate {
                    version: state.version.clone(),
                })
            } else if state.skip_version.as_ref() == Some(tag) {
                Some(UpdateOutcome::Skipped {
                    version: tag.clone(),
                    installed: state.version.clone(),
                })
            } else {
                None
            };
            if let Some(outcome) = outcome {
                self.save_state(channel, &checked(state))?;
                return Ok(outcome);
            }
        }

        self.install(
            channel,
            asset,
            &release,
            installed.as_ref(),
            now,
            smoke_test,
        )
        .await
    }

    async fn install(
        &self,
        channel: YtdlpChannel,
        asset: &str,
        release: &GithubRelease,
        installed: Option<&InstallState>,
        now: Timestamp,
        smoke_test: impl Fn(&Path) -> Result<(), String>,
    ) -> Result<UpdateOutcome, ManagerError> {
        let sums = self.get_bytes(release.asset_url(SUMS)?).await?;
        let signature = self.get_bytes(release.asset_url(SUMS_SIG)?).await?;
        self.verifier.verify(&sums, &signature)?;
        let expected = checksum_for(&String::from_utf8_lossy(&sums), asset)?;

        // Download next to the target, so the final rename is atomic.
        let tmp = self.sibling(channel, &format!(".download-{}", std::process::id()));
        let guard = TempFile(Some(tmp.clone()));
        let url = release.asset_url(asset)?;
        let mut response = self.get(url).await?;
        let mut file = std::fs::File::create(&tmp).map_err(io(&tmp))?;
        let mut hasher = Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ManagerError::Http(format!("could not download {url}: {e}")))?
        {
            hasher.update(&chunk);
            file.write_all(&chunk).map_err(io(&tmp))?;
        }
        file.sync_all().map_err(io(&tmp))?;
        drop(file);
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        check_checksum(asset, &expected, &actual)?;

        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(io(&tmp))?;
        smoke_test(&tmp).map_err(ManagerError::SmokeTest)?;

        let binary = self.binary(channel);
        let previous = self.previous(channel);
        if installed.is_some() {
            let _ = std::fs::remove_file(&previous);
            std::fs::hard_link(&binary, &previous)
                .or_else(|_| std::fs::copy(&binary, &previous).map(drop))
                .map_err(io(&previous))?;
        }
        std::fs::rename(&tmp, &binary).map_err(io(&binary))?;
        let mut guard = guard;
        guard.0 = None;

        let state = InstallState {
            version: release.tag_name.clone(),
            sha256: actual,
            installed_at: now,
            last_check_at: now,
            previous_version: installed.map(|s| s.version.clone()),
            skip_version: None,
        };
        self.save_state(channel, &state)?;
        Ok(match installed {
            Some(old) => UpdateOutcome::Updated {
                from: old.version.clone(),
                to: state.version,
            },
            None => UpdateOutcome::Installed {
                version: state.version,
            },
        })
    }

    /// Make sure the binary exists (downloading it if needed) and check for
    /// an update when the last check is older than `update_check`. A failed
    /// check keeps the current binary and returns a warning.
    pub async fn ensure(
        &self,
        channel: YtdlpChannel,
        update_check: Duration,
        now: Timestamp,
    ) -> Result<Ensured, ManagerError> {
        let path = self.binary(channel);
        if !path.exists() {
            let outcome = self.update(channel, now).await?;
            return Ok(Ensured {
                path,
                outcome: Some(outcome),
                warning: None,
            });
        }
        let interval = SignedDuration::try_from(update_check).unwrap_or(SignedDuration::MAX);
        let due = self
            .state(channel)
            .and_then(|s| s.last_check_at.checked_add(interval).ok())
            .is_none_or(|next| now >= next);
        if !due {
            return Ok(Ensured {
                path,
                outcome: None,
                warning: None,
            });
        }
        match self.update(channel, now).await {
            Ok(outcome) => Ok(Ensured {
                path,
                outcome: Some(outcome),
                warning: None,
            }),
            Err(err) => Ok(Ensured {
                path,
                outcome: None,
                warning: Some(format!("could not check for a yt-dlp update: {err}")),
            }),
        }
    }

    /// Swap the current and previous binaries; the version rolled back from
    /// isn't reinstalled until a newer release appears.
    pub fn rollback(&self, channel: YtdlpChannel) -> Result<(String, String), ManagerError> {
        let binary = self.binary(channel);
        let previous = self.previous(channel);
        let state = self
            .state(channel)
            .filter(|_| binary.exists())
            .ok_or(ManagerError::NotInstalled)?;
        let older = state
            .previous_version
            .clone()
            .filter(|_| previous.exists())
            .ok_or(ManagerError::NoPrevious)?;
        let _lock = self.lock()?;

        let swap = self.sibling(channel, ".swap");
        std::fs::rename(&binary, &swap).map_err(io(&binary))?;
        std::fs::rename(&previous, &binary).map_err(io(&previous))?;
        std::fs::rename(&swap, &previous).map_err(io(&previous))?;

        let bytes = std::fs::read(&binary).map_err(io(&binary))?;
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let newer = state.version.clone();
        self.save_state(
            channel,
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
