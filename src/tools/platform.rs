//! Which release binaries fit this machine, and comparing versions.

use std::cmp::Ordering;
use std::path::Path;

/// The release asset for an OS/architecture/libc combination.
///
/// # Errors
///
/// When yt-dlp has no release binary for the combination.
pub fn asset_name(os: &str, arch: &str, musl: bool) -> Result<&'static str, String> {
    match (os, arch, musl) {
        ("macos", _, _) => Ok("yt-dlp_macos"),
        ("linux", "x86_64", false) => Ok("yt-dlp_linux"),
        ("linux", "aarch64", false) => Ok("yt-dlp_linux_aarch64"),
        ("linux", "x86_64", true) => Ok("yt-dlp_musllinux"),
        ("linux", "aarch64", true) => Ok("yt-dlp_musllinux_aarch64"),
        _ => Err(format!(
            "no yt-dlp release binary for {os}/{arch}; install yt-dlp yourself and set \
             managed = false in [tools.yt-dlp] with path pointing to it"
        )),
    }
}

/// Deno's release target for an OS/architecture (glibc only).
///
/// # Errors
///
/// When Deno has no release binary for the combination (including musl).
pub fn deno_target(os: &str, arch: &str, musl: bool) -> Result<&'static str, String> {
    match (os, arch, musl) {
        ("macos", "aarch64", _) => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64", _) => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64", false) => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64", false) => Ok("aarch64-unknown-linux-gnu"),
        _ => Err(format!(
            "no Deno release binary for {os}/{arch}{}; install Deno (or another runtime yt-dlp \
             supports) yourself and set managed = false in [tools.deno]",
            if musl { " (musl)" } else { "" }
        )),
    }
}

/// Deno's release asset for the machine tidepool runs on.
///
/// # Errors
///
/// When Deno has no release binary for this machine.
pub fn current_deno_asset() -> Result<String, String> {
    deno_target(std::env::consts::OS, std::env::consts::ARCH, is_musl())
        .map(|t| format!("deno-{t}.zip"))
}

/// The asset for the machine tidepool runs on.
///
/// # Errors
///
/// When yt-dlp has no release binary for this machine.
pub fn current_asset() -> Result<&'static str, String> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH, is_musl())
}

/// A musl-based Linux (e.g. Alpine): tools must be its musl builds.
fn is_musl() -> bool {
    std::env::consts::OS == "linux"
        && musl_system(
            std::env::consts::ARCH,
            |path| Path::new(path).exists(),
            || {
                std::fs::read_dir("/lib").is_ok_and(|entries| {
                    entries
                        .flatten()
                        .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
                })
            },
        )
}

/// Whether a Linux system runs musl programs only. Its glibc loader decides:
/// with it, glibc builds run (a musl loader may be there too, e.g. Debian's
/// `musl` package or files copied from an Alpine image). Without it, a musl
/// loader (or Alpine) means musl.
fn musl_system(arch: &str, exists: impl Fn(&str) -> bool, musl_loader: impl Fn() -> bool) -> bool {
    let glibc_loader = match arch {
        "x86_64" => Some("/lib64/ld-linux-x86-64.so.2"),
        "aarch64" => Some("/lib/ld-linux-aarch64.so.1"),
        _ => None,
    };
    if glibc_loader.is_some_and(&exists) {
        return false;
    }
    musl_loader() || exists("/etc/alpine-release")
}

/// Compare yt-dlp versions (`2026.10.26`, `2026.10.29.232701`) numerically,
/// part by part. Non-numeric parts compare as text.
#[must_use]
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
    fn the_glibc_loader_decides_the_libc() {
        let system = |files: &'static [&'static str], musl_loader: bool| {
            move |arch: &str| musl_system(arch, |p| files.contains(&p), || musl_loader)
        };
        // Debian, also with the musl package installed.
        let debian = system(
            &["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"],
            false,
        );
        assert!(!debian("x86_64") && !debian("aarch64"));
        let debian_with_musl = system(&["/lib64/ld-linux-x86-64.so.2"], true);
        assert!(!debian_with_musl("x86_64"));
        // Alpine.
        assert!(system(&[], true)("x86_64"));
        assert!(system(&["/etc/alpine-release"], false)("aarch64"));
        // Neither loader found: glibc, the common case.
        assert!(!system(&[], false)("x86_64"));
    }

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
        assert!(err.contains("managed = false in [tools.yt-dlp]"), "{err}");
        assert_eq!(
            deno_target("macos", "aarch64", false),
            Ok("aarch64-apple-darwin")
        );
        assert_eq!(
            deno_target("linux", "x86_64", false),
            Ok("x86_64-unknown-linux-gnu")
        );
        let err = deno_target("linux", "x86_64", true).unwrap_err();
        assert!(
            err.contains("musl") && err.contains("[tools.deno]"),
            "{err}"
        );
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
