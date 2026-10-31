//! Human durations (`"30d"`, `"12h"`, `"1h30m"`) and the `cutoff` value.

use std::time::Duration;

/// Parse a duration such as `"30d"`, `"12h"`, `"10s"` or `"1h30m"`.
///
/// Units: `s`, `m`, `h`, `d`, `w`. At least one `<number><unit>` group is
/// required; groups may be concatenated.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let invalid =
        || format!("{input:?} is not a valid duration (e.g. \"30d\", \"12h\", \"1h30m\")");
    if input.is_empty() {
        return Err(invalid());
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    for ch in input.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(invalid()),
        };
        let n: u64 = digits.parse().map_err(|_| invalid())?;
        digits.clear();
        total = n
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(invalid)?;
    }
    if !digits.is_empty() {
        return Err(invalid());
    }
    Ok(Duration::from_secs(total))
}

/// When to stop looking back for videos to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cutoff {
    /// No cutoff.
    None,
    /// Videos published more than this long before the run are skipped.
    Relative(Duration),
    /// Videos published before this date are skipped.
    Date(jiff::civil::Date),
}

impl std::str::FromStr for Cutoff {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input == "none" {
            return Ok(Cutoff::None);
        }
        if let Ok(duration) = parse_duration(input) {
            return Ok(Cutoff::Relative(duration));
        }
        input
            .parse::<jiff::civil::Date>()
            .map(Cutoff::Date)
            .map_err(|_| {
                format!(
                    "{input:?} is not a valid cutoff (e.g. \"30d\", \"2024-01-01\" or \"none\")"
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_units() {
        assert_eq!(parse_duration("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("12h"), Ok(Duration::from_secs(12 * 3600)));
        assert_eq!(parse_duration("30d"), Ok(Duration::from_secs(30 * 86400)));
        assert_eq!(parse_duration("2w"), Ok(Duration::from_secs(14 * 86400)));
    }

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse_duration("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_duration("1d12h"), Ok(Duration::from_secs(129_600)));
    }

    #[test]
    fn rejects_invalid_durations() {
        for bad in ["", "30", "d", "30x", "1.5h", "-1h", "30 d", "h30"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn parses_cutoffs() {
        assert_eq!("none".parse::<Cutoff>(), Ok(Cutoff::None));
        assert_eq!(
            "30d".parse::<Cutoff>(),
            Ok(Cutoff::Relative(Duration::from_secs(30 * 86400)))
        );
        assert_eq!(
            "2024-01-01".parse::<Cutoff>(),
            Ok(Cutoff::Date(jiff::civil::date(2024, 1, 1)))
        );
        assert!("2024-13-01".parse::<Cutoff>().is_err());
        assert!("yesterday".parse::<Cutoff>().is_err());
    }
}
