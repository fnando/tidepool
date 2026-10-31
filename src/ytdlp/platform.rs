//! Which yt-dlp release binary fits this machine, and comparing versions.

use std::cmp::Ordering;
use std::path::Path;

/// The release asset for an OS/architecture/libc combination.
pub fn asset_name(os: &str, arch: &str, musl: bool) -> Result<&'static str, String> {
    match (os, arch, musl) {
        ("macos", _, _) => Ok("yt-dlp_macos"),
        ("linux", "x86_64", false) => Ok("yt-dlp_linux"),
        ("linux", "aarch64", false) => Ok("yt-dlp_linux_aarch64"),
        ("linux", "x86_64", true) => Ok("yt-dlp_musllinux"),
        ("linux", "aarch64", true) => Ok("yt-dlp_musllinux_aarch64"),
        _ => Err(format!(
            "no yt-dlp release binary for {os}/{arch}; install yt-dlp yourself and set \
             ytdlp.managed = false with ytdlp.path pointing to it"
        )),
    }
}

/// The asset for the machine tidepool runs on.
pub fn current_asset() -> Result<&'static str, String> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH, is_musl())
}

/// A musl-based Linux (e.g. Alpine) has a musl dynamic loader.
fn is_musl() -> bool {
    std::env::consts::OS == "linux"
        && std::fs::read_dir("/lib")
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
            })
            .unwrap_or(false)
        || Path::new("/etc/alpine-release").exists()
}

/// Compare yt-dlp versions (`2026.10.26`, `2026.10.29.232701`) numerically,
/// part by part. Non-numeric parts compare as text.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    _ => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_assets() {
        assert_eq!(asset_name("macos", "aarch64", false), Ok("yt-dlp_macos"));
        assert_eq!(asset_name("macos", "x86_64", false), Ok("yt-dlp_macos"));
        assert_eq!(asset_name("linux", "x86_64", false), Ok("yt-dlp_linux"));
        assert_eq!(
            asset_name("linux", "aarch64", false),
            Ok("yt-dlp_linux_aarch64")
        );
        assert_eq!(asset_name("linux", "x86_64", true), Ok("yt-dlp_musllinux"));
        assert_eq!(
            asset_name("linux", "aarch64", true),
            Ok("yt-dlp_musllinux_aarch64")
        );
        let err = asset_name("freebsd", "x86_64", false).unwrap_err();
        assert!(err.contains("ytdlp.managed = false"), "{err}");
        assert!(asset_name("linux", "arm", false).is_err());
    }

    #[test]
    fn compares_versions() {
        assert_eq!(
            compare_versions("2026.10.26", "2026.10.26"),
            Ordering::Equal
        );
        assert_eq!(
            compare_versions("2026.10.26", "2026.9.30"),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("2026.10.29.232701", "2026.10.29.201734"),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("2026.10.29", "2026.10.29.1"),
            Ordering::Less
        );
        assert_eq!(compare_versions("2025.12.31", "2026.01.01"), Ordering::Less);
    }

    #[test]
    fn this_machine_has_an_asset() {
        if matches!(std::env::consts::OS, "macos" | "linux") {
            assert!(current_asset().is_ok());
        }
    }
}
