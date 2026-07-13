//! The only place in Tonans that reads a clock.
//!
//! `tonans-core` keeps ambient time out of its numeric model: the same canonical
//! inputs, model version, and supported build target must reproduce the same numeric
//! `results_hash`. A wall-clock read inside it would quietly break that contract, so
//! `clippy.toml` bans `SystemTime::now` and `Instant::now` workspace-wide and this
//! module is the single audited exception. Every generated timestamp is passed in
//! from here.
//!
//! If you find yourself wanting to call a clock somewhere else: don't. Take the
//! instant as a parameter. The reason the determinism test can prove anything at
//! all is that there is exactly one place to look.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

/// The current instant, in UTC.
#[must_use]
pub fn now_utc() -> DateTime<Utc> {
    #[allow(clippy::disallowed_methods, reason = "the one audited wall-clock read")]
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    DateTime::from_timestamp(
        i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        elapsed.subsec_nanos(),
    )
    .unwrap_or_else(|| DateTime::from_timestamp(0, 0).expect("the Unix epoch is a valid instant"))
}

/// A monotonic instant, for rate limiting.
///
/// Never used in a result: only to decide whether a request arrived too soon after
/// the last one.
#[must_use]
pub fn monotonic() -> Instant {
    #[allow(clippy::disallowed_methods, reason = "the one audited monotonic read")]
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_returns_a_plausible_instant() {
        let now = now_utc();
        // Sanity, not precision: if this is wrong, something is very wrong.
        assert!(now.timestamp() > 1_700_000_000, "the clock is before 2023");
        assert!(now.timestamp() < 4_102_444_800, "the clock is after 2100");
    }

    #[test]
    fn the_monotonic_clock_does_not_go_backwards() {
        let first = monotonic();
        let second = monotonic();
        assert!(second >= first);
    }
}
