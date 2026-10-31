//! Byte sizes such as `"20GiB"` or `"500MB"`.

/// Parse a byte size: a number (decimals allowed) followed by a unit.
///
/// Units: `B`, `KB`/`MB`/`GB`/`TB` (powers of 1000) and `KiB`/`MiB`/`GiB`/`TiB`
/// (powers of 1024), case-insensitive. A bare number is rejected so the unit is
/// always explicit.
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
    let value: f64 = number.parse().map_err(|_| invalid())?;
    let bytes = (value * multiplier as f64).round();
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(invalid());
    }
    Ok(bytes as u64)
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
