//! Lowercase hex encoding, for digests and random tokens.

use std::fmt::Write as _;

/// `bytes` as lowercase hex, two digits per byte.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}
