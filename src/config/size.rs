//! Byte sizes such as `"20GiB"` or `"500MB"`.

/// Parse a byte size: a number (decimals allowed) followed by a unit.
///
/// Units: `B`, `KB`/`MB`/`GB`/`TB` (powers of 1000) and `KiB`/`MiB`/`GiB`/`TiB`
/// (powers of 1024), case-insensitive. A bare number is rejected so the unit is
/// always explicit.
///
/// # Errors
///
/// When `input` has no number, more than one `.`, a missing or unknown unit,
/// or a size larger than a `u64` holds.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let invalid = || format!("{input:?} is not a valid byte size (e.g. \"20GiB\", \"500MB\")");
    let split = input
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .ok_or_else(invalid)?;
    let (number, unit) = input.split_at(split);
    if number.is_empty() || number.matches('.').count() > 1 {
        return Err(invalid());
    }
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "b" => 1,
        "kb" => 1000,
        "mb" => 1000_u64.pow(2),
        "gb" => 1000_u64.pow(3),
        "tb" => 1000_u64.pow(4),
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(invalid()),
    };
    let bytes = scale(number, multiplier).ok_or_else(invalid)?;
    u64::try_from(bytes).map_err(|_| invalid())
}

/// `number` (digits with at most one `.`) times `multiplier`, rounded to the
/// nearest whole byte (halves round up). `None` if `number` has no digits or
/// the result doesn't fit.
fn scale(number: &str, multiplier: u64) -> Option<u128> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let multiplier = u128::from(multiplier);
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    // Digits past the 24th can't move the result by a byte, and leaving them
    // out keeps the arithmetic below within `u128`.
    let (numerator, denominator) = fraction
        .bytes()
        .take(24)
        .fold((0_u128, 1_u128), |(n, d), digit| {
            (n * 10 + u128::from(digit - b'0'), d * 10)
        });
    let fraction = (numerator * multiplier * 2 + denominator) / (denominator * 2);
    whole.checked_mul(multiplier)?.checked_add(fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_and_decimal_units() {
        assert_eq!(parse_size("512B"), Ok(512));
        assert_eq!(parse_size("1KB"), Ok(1000));
        assert_eq!(parse_size("1KiB"), Ok(1024));
        assert_eq!(parse_size("20GiB"), Ok(20 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("500MB"), Ok(500_000_000));
        assert_eq!(parse_size("1.5GiB"), Ok(1_610_612_736));
        assert_eq!(parse_size("2tb"), Ok(2_000_000_000_000));
    }

    #[test]
    fn rejects_invalid_sizes() {
        for bad in ["", "20", "GiB", "20 gigs", "-1GiB", "1.2.3MB", "20 GiB"] {
            assert!(parse_size(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
