//! When the daemon syncs and restarts its services. Pure: times are passed in.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

/// Up to this share of the interval is added at random, so syncs of several
/// machines don't line up.
const JITTER: f64 = 0.05;

/// The next sync: `interval` after the last one ended, plus up to 5% of it
/// (`jitter` is 0.0–1.0).
#[must_use]
pub fn next_sync(last_finished: Timestamp, interval: Duration, jitter: f64) -> Timestamp {
    let extra = interval.mul_f64(JITTER * jitter.clamp(0.0, 1.0));
    let wait = SignedDuration::try_from(interval + extra).unwrap_or(SignedDuration::MAX);
    last_finished.checked_add(wait).unwrap_or(Timestamp::MAX)
}

/// How long until `next` (zero when it's due: missed syncs, after a restart
/// or a sleep, become a single sync right away).
#[must_use]
pub fn wait_until(next: Timestamp, now: Timestamp) -> Duration {
    Duration::try_from(next.duration_since(now)).unwrap_or(Duration::ZERO)
}

/// How often the syncer looks for sources due, at least: new sources are
/// picked up within this, and a sync that couldn't run is tried again.
pub const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Of `sources` (keys), those due at `now`: never synced (no entry, or no
/// time), or their next sync has passed.
#[must_use]
pub fn due<S: std::hash::BuildHasher>(
    sources: &[String],
    next: &std::collections::HashMap<String, Option<Timestamp>, S>,
    now: Timestamp,
) -> Vec<String> {
    sources
        .iter()
        .filter(|key| next.get(*key).copied().flatten().is_none_or(|t| t <= now))
        .cloned()
        .collect()
}

/// The soonest of `sources`' next syncs (`now` for one never synced).
#[must_use]
pub fn soonest<S: std::hash::BuildHasher>(
    sources: &[String],
    next: &std::collections::HashMap<String, Option<Timestamp>, S>,
    now: Timestamp,
) -> Option<Timestamp> {
    sources
        .iter()
        .map(|key| next.get(key).copied().flatten().unwrap_or(now))
        .min()
}

/// How long to wait before restarting a service that stopped unexpectedly
/// for the `failures`-th time in a row: 1 s, doubling, at most 5 minutes.
#[must_use]
pub fn restart_delay(failures: u32) -> Duration {
    let seconds = 1u64 << failures.saturating_sub(1).min(16);
    Duration::from_secs(seconds.min(300))
}

/// A service that ran this long before stopping counts as healthy: its next
/// failure starts the backoff over.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(600);

/// A random number in 0.0–1.0.
#[must_use]
pub fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    let permille = u16::try_from(hasher.finish() % 1001).unwrap_or(1000);
    f64::from(permille) / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn the_next_sync_counts_from_the_end_with_some_jitter() {
        let six_hours = Duration::from_secs(6 * 3600);
        let end = at("2026-11-01T10:00:00Z");
        assert_eq!(next_sync(end, six_hours, 0.0), at("2026-11-01T16:00:00Z"));
        assert_eq!(next_sync(end, six_hours, 1.0), at("2026-11-01T16:18:00Z"));
        let any = next_sync(end, six_hours, jitter());
        assert!(any >= at("2026-11-01T16:00:00Z") && any <= at("2026-11-01T16:18:00Z"));
    }

    #[test]
    fn sources_are_due_when_new_or_their_time_has_passed() {
        let now = at("2026-11-01T12:00:00Z");
        let keys: Vec<String> = ["new", "unset", "past", "now", "later"]
            .map(String::from)
            .to_vec();
        let next = std::collections::HashMap::from([
            ("unset".to_string(), None),
            ("past".to_string(), Some(at("2026-11-01T11:00:00Z"))),
            ("now".to_string(), Some(now)),
            ("later".to_string(), Some(at("2026-11-01T13:00:00Z"))),
            ("gone".to_string(), Some(at("2026-11-01T09:00:00Z"))),
        ]);
        assert_eq!(due(&keys, &next, now), ["new", "unset", "past", "now"]);
        let later = vec!["later".to_string()];
        assert_eq!(due(&later, &next, now), Vec::<String>::new());
        assert_eq!(
            soonest(&later, &next, now),
            Some(at("2026-11-01T13:00:00Z"))
        );
        assert_eq!(soonest(&keys, &next, now), Some(at("2026-11-01T11:00:00Z")));
        assert_eq!(soonest(&[], &next, now), None);
    }

    #[test]
    fn missed_syncs_run_once_right_away() {
        let next = at("2026-11-01T16:00:00Z");
        assert_eq!(wait_until(next, at("2026-11-03T09:00:00Z")), Duration::ZERO);
        assert_eq!(
            wait_until(next, at("2026-11-01T15:59:00Z")),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn restarts_back_off() {
        let delays: Vec<u64> = (1..=11).map(|n| restart_delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(restart_delay(1000).as_secs(), 300);
    }
}
