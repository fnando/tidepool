//! Release verification: the `SHA2-256SUMS` file must carry a valid OpenPGP
//! signature from yt-dlp's signing key, and the downloaded binary must match
//! its checksum there.

use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use pgp::types::KeyDetails;

/// yt-dlp's release signing key, from <https://github.com/yt-dlp/yt-dlp/blob/master/public.key>.
pub const YTDLP_SIGNING_KEY: &str = include_str!("../../assets/yt-dlp-signing-key.asc");

/// Fingerprint the embedded key must have, so a swapped key file is caught.
pub const YTDLP_SIGNING_KEY_FINGERPRINT: &str = "AC0CBBE6848D6A873464AF4E57CF65933B5A7581";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("invalid signing key: {0}")]
    Key(String),
    #[error("SHA2-256SUMS has no valid signature from the yt-dlp signing key")]
    BadSignature,
    #[error("SHA2-256SUMS has no checksum for {0}")]
    MissingChecksum(String),
    #[error("checksum mismatch for {asset}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
}

/// Checks signatures against one trusted public key.
#[derive(Debug, Clone)]
pub struct Verifier {
    key: SignedPublicKey,
}

impl Verifier {
    /// The verifier for real yt-dlp releases.
    pub fn ytdlp() -> Self {
        let verifier = Self::from_armored(YTDLP_SIGNING_KEY).expect("embedded yt-dlp key parses");
        assert_eq!(
            verifier.fingerprint(),
            YTDLP_SIGNING_KEY_FINGERPRINT,
            "embedded yt-dlp key was replaced"
        );
        verifier
    }

    pub fn from_armored(armored: &str) -> Result<Self, VerifyError> {
        let (key, _) = SignedPublicKey::from_armor_single(armored.as_bytes())
            .map_err(|e| VerifyError::Key(e.to_string()))?;
        Ok(Self { key })
    }

    /// Uppercase hex fingerprint of the primary key.
    pub fn fingerprint(&self) -> String {
        format!("{:X}", self.key.fingerprint())
    }

    /// Verify a binary detached signature over `content`, made by the primary
    /// key or one of its subkeys.
    pub fn verify(&self, content: &[u8], signature: &[u8]) -> Result<(), VerifyError> {
        let signature =
            DetachedSignature::from_bytes(signature).map_err(|_| VerifyError::BadSignature)?;
        if signature.verify(&self.key, content).is_ok() {
            return Ok(());
        }
        let by_subkey = self
            .key
            .public_subkeys
            .iter()
            .any(|subkey| signature.verify(subkey, content).is_ok());
        if by_subkey {
            Ok(())
        } else {
            Err(VerifyError::BadSignature)
        }
    }
}

/// The expected SHA-256 of `asset` in a `SHA2-256SUMS` file.
pub fn checksum_for(sums: &str, asset: &str) -> Result<String, VerifyError> {
    sums.lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim_start().trim_start_matches('*') == asset)
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .filter(|hash| hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| VerifyError::MissingChecksum(asset.to_string()))
}

/// Compare a computed SHA-256 (hex) with the expected one.
pub fn check_checksum(asset: &str, expected: &str, actual: &str) -> Result<(), VerifyError> {
    if expected.eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(VerifyError::ChecksumMismatch {
            asset: asset.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/ytdlp/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn embedded_key_is_pinned() {
        assert_eq!(
            Verifier::ytdlp().fingerprint(),
            YTDLP_SIGNING_KEY_FINGERPRINT
        );
    }

    #[test]
    fn real_release_signature_verifies() {
        let verifier = Verifier::ytdlp();
        let sums = fixture("real/SHA2-256SUMS");
        let sig = fixture("real/SHA2-256SUMS.sig");
        assert_eq!(verifier.verify(&sums, &sig), Ok(()));

        let mut tampered = sums.clone();
        tampered[0] ^= 1;
        assert_eq!(
            verifier.verify(&tampered, &sig),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn signatures_from_other_keys_are_rejected() {
        let sums = fixture("fake/2026.01.01/SHA2-256SUMS");
        let sig = fixture("fake/2026.01.01/SHA2-256SUMS.sig");
        assert_eq!(
            Verifier::ytdlp().verify(&sums, &sig),
            Err(VerifyError::BadSignature)
        );

        let test_key = String::from_utf8(fixture("fake/test-signing-key.asc")).unwrap();
        assert_eq!(
            Verifier::from_armored(&test_key)
                .unwrap()
                .verify(&sums, &sig),
            Ok(())
        );
        assert_eq!(
            Verifier::ytdlp().verify(&sums, b"not a signature"),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn finds_checksums() {
        let sums = String::from_utf8(fixture("real/SHA2-256SUMS")).unwrap();
        assert_eq!(
            checksum_for(&sums, "yt-dlp_macos").unwrap(),
            "0f922c15ccc0b83eefa1c8bc9104e434318c4bf16e408e1ee93b7745a6efe0aa"
        );
        assert_eq!(
            checksum_for(&sums, "yt-dlp_linux").unwrap(),
            "767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795"
        );
        assert_eq!(
            checksum_for(&sums, "yt-dlp_nope"),
            Err(VerifyError::MissingChecksum("yt-dlp_nope".into()))
        );
        assert!(check_checksum("a", "ABC", "abc").is_ok());
        assert!(check_checksum("a", "abc", "abd").is_err());
    }
}
