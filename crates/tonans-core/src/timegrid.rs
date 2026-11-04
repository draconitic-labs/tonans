//! The interval grid, and the timezone rules that guard its edges.
//!
//! Tonans stores every instant in UTC and converts to the site's IANA timezone
//! only where a human calendar matters: bucketing monthly demand charges, and
//! labelling axes. Two consequences follow, and both are deliberate:
//!
//! * The grid is uniform in *absolute* time. A 15-minute interval is 15 minutes
//!   of wall clock everywhere, including across a DST transition. A "spring
//!   forward" day therefore has 92 intervals in local time and 92 in UTC — the
//!   day is genuinely an hour shorter, and the data must reflect that.
//! * A naive local timestamp in an imported CSV is ambiguous once a year and
//!   nonexistent once a year. Tonans rejects both rather than picking one. See
//!   [`resolve_local`].

use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use chrono_tz::Tz;

use crate::error::{Error, Result};
use crate::limits;

/// A uniform sequence of intervals in absolute time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalGrid {
    start_utc: DateTime<Utc>,
    interval_minutes: u32,
    count: usize,
    timezone: Tz,
}

impl IntervalGrid {
    /// Build a grid.
    ///
    /// Rejects interval durations outside [`limits::MIN_INTERVAL_MINUTES`] ..=
    /// [`limits::MAX_INTERVAL_MINUTES`], durations that do not divide an hour,
    /// and interval counts past [`limits::MAX_INTERVALS`].
    ///
    /// Dividing an hour is necessary but not sufficient for unambiguous monthly
    /// demand bucketing: a few IANA zones change their UTC offset by a fraction of
    /// an hour. Every boundary in the requested span is therefore checked against
    /// the local clock as well, so no accepted interval can straddle a local tariff
    /// hour or month.
    pub fn new(
        start_utc: DateTime<Utc>,
        interval_minutes: u32,
        count: usize,
        timezone: Tz,
    ) -> Result<Self> {
        if !(limits::MIN_INTERVAL_MINUTES..=limits::MAX_INTERVAL_MINUTES)
            .contains(&interval_minutes)
        {
            return Err(Error::validation(
                "validation.interval_minutes",
                "dataset.interval_minutes",
                format!(
                    "interval must be between {min} and {max} minutes, got {interval_minutes}",
                    min = limits::MIN_INTERVAL_MINUTES,
                    max = limits::MAX_INTERVAL_MINUTES
                ),
            ));
        }
        if 60 % interval_minutes != 0 {
            return Err(Error::validation(
                "validation.interval_minutes",
                "dataset.interval_minutes",
                format!(
                    "interval must divide an hour evenly; {interval_minutes} minutes does not. \
                     Supported intervals: 1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30, 60."
                ),
            ));
        }
        if count == 0 {
            return Err(Error::validation(
                "validation.empty_dataset",
                "dataset.interval_count",
                "the data set contains no intervals",
            ));
        }
        if count > limits::MAX_INTERVALS {
            return Err(Error::limit(
                "limit.intervals",
                limits::MAX_INTERVALS as u64,
                count as u64,
                format!(
                    "{count} intervals exceeds the {max} interval limit",
                    max = limits::MAX_INTERVALS
                ),
            ));
        }
        let span_minutes = i64::from(interval_minutes)
            * i64::try_from(count).map_err(|_| {
                Error::validation(
                    "validation.interval_span",
                    "dataset.interval_count",
                    "the interval count cannot be represented as a time span",
                )
            })?;
        if start_utc
            .checked_add_signed(TimeDelta::minutes(span_minutes))
            .is_none()
        {
            return Err(Error::validation(
                "validation.interval_span",
                "dataset.start_utc",
                "the dataset end would fall outside the supported timestamp range",
            ));
        }
        let grid = Self {
            start_utc,
            interval_minutes,
            count,
            timezone,
        };
        let local_start = start_utc.with_timezone(&timezone);
        if !is_local_boundary(&local_start, interval_minutes) {
            return Err(Error::validation(
                "validation.interval_start_alignment",
                "dataset.start_utc",
                format!(
                    "the first interval begins at {} in {timezone}, which is not aligned to a \
                     {interval_minutes}-minute local clock boundary. Align the source export before \
                     importing it so an interval cannot straddle a tariff hour or month.",
                    local_start.to_rfc3339()
                ),
            ));
        }
        for index in 1..=count {
            let local = grid.start_of(index).with_timezone(&timezone);
            if !is_local_boundary(&local, interval_minutes) {
                return Err(Error::validation(
                    "validation.interval_timezone_alignment",
                    "dataset.interval_grid",
                    format!(
                        "interval boundary {index} lands at {} in {timezone}, which is not aligned \
                         to the {interval_minutes}-minute local clock after a timezone-offset \
                         transition. Use a shorter interval that remains locally aligned, or \
                         provide a span that does not cross this transition, so an interval cannot \
                         straddle a tariff hour or month.",
                        local.to_rfc3339()
                    ),
                ));
            }
        }
        Ok(grid)
    }

    /// Interval duration in hours. The conversion factor between kW and kWh.
    #[must_use]
    pub fn hours(&self) -> f64 {
        f64::from(self.interval_minutes) / 60.0
    }

    /// Interval duration in minutes.
    #[must_use]
    pub fn interval_minutes(&self) -> u32 {
        self.interval_minutes
    }

    /// Number of intervals.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// The site timezone.
    #[must_use]
    pub fn timezone(&self) -> Tz {
        self.timezone
    }

    /// Instant at which interval `index` begins.
    ///
    /// Callers pass an in-range index; an out-of-range one still returns a
    /// well-defined instant (the grid is uniform), which keeps the arithmetic
    /// total and lets [`Self::end_utc`] be expressed as `start_utc(count)`.
    #[must_use]
    pub fn start_of(&self, index: usize) -> DateTime<Utc> {
        self.start_utc + TimeDelta::minutes(i64::from(self.interval_minutes) * index as i64)
    }

    /// Instant at which interval `index` ends, which is when `index + 1` begins.
    #[must_use]
    pub fn end_of(&self, index: usize) -> DateTime<Utc> {
        self.start_of(index + 1)
    }

    /// Instant at which the whole grid begins.
    #[must_use]
    pub fn start_utc(&self) -> DateTime<Utc> {
        self.start_utc
    }

    /// Instant at which the whole grid ends, exclusive.
    #[must_use]
    pub fn end_utc(&self) -> DateTime<Utc> {
        self.start_of(self.count)
    }

    /// The index of the interval containing `instant`, or `None` if it is outside
    /// the grid.
    #[must_use]
    pub fn index_containing(&self, instant: DateTime<Utc>) -> Option<usize> {
        if instant < self.start_utc || instant >= self.end_utc() {
            return None;
        }
        let elapsed = (instant - self.start_utc).num_minutes();
        usize::try_from(elapsed / i64::from(self.interval_minutes)).ok()
    }

    /// The index of the interval that *begins* exactly at `instant`.
    ///
    /// Manual schedules and outage windows must land on interval boundaries. A
    /// window that begins mid-interval would force the engine to either split an
    /// interval — changing the grid — or silently round, which is a repair. Both
    /// are refused; the caller gets `None` and turns it into a validation error.
    #[must_use]
    pub fn boundary_index(&self, instant: DateTime<Utc>) -> Option<usize> {
        if instant < self.start_utc || instant > self.end_utc() {
            return None;
        }
        let elapsed_seconds = (instant - self.start_utc).num_seconds();
        let interval_seconds = i64::from(self.interval_minutes) * 60;
        if elapsed_seconds % interval_seconds != 0 {
            return None;
        }
        let index = usize::try_from(elapsed_seconds / interval_seconds).ok()?;
        (self.start_of(index) == instant).then_some(index)
    }

    /// The local calendar month containing interval `index`, as `(year, month)`.
    ///
    /// Used to bucket demand charges. Construction verifies every boundary against
    /// the site's local clock, including across fractional-hour offset changes, so
    /// an accepted interval never straddles two local months.
    #[must_use]
    pub fn local_month_of(&self, index: usize) -> (i32, u32) {
        let local = self.start_of(index).with_timezone(&self.timezone);
        (local.year(), local.month())
    }
}

fn is_local_boundary(local: &DateTime<Tz>, interval_minutes: u32) -> bool {
    local.second() == 0
        && local.nanosecond() == 0
        && local.minute().is_multiple_of(interval_minutes)
}

/// Convert a naive local timestamp to UTC, refusing to guess.
///
/// A local time inside a DST "spring forward" gap does not exist; a local time
/// inside a "fall back" overlap happens twice. Interval data that carries such a
/// timestamp is under-specified, and no choice we could make on the user's behalf
/// would be defensible in a report. Both cases are hard errors, and the message
/// names the two candidate instants (or the gap) so the user can fix the export
/// at its source — normally by re-exporting with UTC offsets.
pub fn resolve_local(
    timezone: Tz,
    naive: chrono::NaiveDateTime,
    line: Option<u64>,
) -> Result<DateTime<Utc>> {
    use chrono::TimeZone;

    let mapped = timezone.from_local_datetime(&naive);
    if let Some(resolved) = mapped.single() {
        return Ok(resolved.with_timezone(&Utc));
    }
    match (mapped.earliest(), mapped.latest()) {
        (Some(earliest), Some(latest)) => Err(Error::import(
            "import.ambiguous_local_time",
            line,
            Some("timestamp"),
            format!(
                "local time {naive} occurs twice in {timezone} because of the daylight-saving \
                 transition; it could mean {earliest_utc} or {latest_utc}. Tonans will not guess. \
                 Re-export the data with UTC offsets (for example 2024-11-03T01:30:00-04:00).",
                earliest_utc = earliest.with_timezone(&Utc).to_rfc3339(),
                latest_utc = latest.with_timezone(&Utc).to_rfc3339(),
            ),
        )),
        _ => Err(Error::import(
            "import.nonexistent_local_time",
            line,
            Some("timestamp"),
            format!(
                "local time {naive} does not exist in {timezone}; the clock skips it at the \
                 daylight-saving transition. Check the source system's timezone handling, or \
                 re-export the data with UTC offsets."
            ),
        )),
    }
}

/// Parse an IANA timezone name.
pub fn parse_timezone(name: &str) -> Result<Tz> {
    name.parse::<Tz>().map_err(|_| {
        Error::validation(
            "validation.unknown_timezone",
            "site.timezone",
            format!(
                "'{name}' is not an IANA timezone name. Use a name from the tz database, \
                 such as America/New_York, Europe/Madrid, or UTC."
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_utc()
    }

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    #[test]
    fn grid_arithmetic_is_uniform_in_absolute_time() {
        let grid =
            IntervalGrid::new(utc(2024, 1, 1, 0, 0), 15, 96, chrono_tz::America::New_York).unwrap();
        assert!((grid.hours() - 0.25).abs() < 1e-12);
        assert_eq!(grid.start_of(0), utc(2024, 1, 1, 0, 0));
        assert_eq!(grid.start_of(4), utc(2024, 1, 1, 1, 0));
        assert_eq!(grid.end_of(95), utc(2024, 1, 2, 0, 0));
        assert_eq!(grid.end_utc(), utc(2024, 1, 2, 0, 0));
    }

    #[test]
    fn boundary_index_refuses_mid_interval_instants() {
        let grid = IntervalGrid::new(utc(2024, 1, 1, 0, 0), 15, 96, chrono_tz::UTC).unwrap();
        assert_eq!(grid.boundary_index(utc(2024, 1, 1, 0, 30)), Some(2));
        // 07 minutes past is inside interval 0, not on a boundary.
        assert_eq!(grid.boundary_index(utc(2024, 1, 1, 0, 7)), None);
        assert_eq!(grid.index_containing(utc(2024, 1, 1, 0, 7)), Some(0));
        // The very end of the grid is a legal boundary (an exclusive window end).
        assert_eq!(grid.boundary_index(utc(2024, 1, 2, 0, 0)), Some(96));
        assert_eq!(grid.index_containing(utc(2024, 1, 2, 0, 0)), None);
        assert_eq!(grid.boundary_index(utc(2023, 12, 31, 23, 45)), None);
        let with_subseconds = utc(2024, 1, 1, 0, 30).with_nanosecond(1).unwrap();
        assert_eq!(grid.boundary_index(with_subseconds), None);
    }

    #[test]
    fn a_grid_may_not_begin_midway_through_its_local_interval() {
        assert!(IntervalGrid::new(
            utc(2024, 1, 31, 23, 30),
            60,
            2,
            chrono_tz::America::New_York,
        )
        .is_err());
        assert!(
            IntervalGrid::new(utc(2024, 2, 1, 5, 0), 60, 2, chrono_tz::America::New_York,).is_ok()
        );
    }

    #[test]
    fn a_fractional_hour_dst_shift_may_not_break_later_local_boundaries() {
        // Lord Howe advances by thirty minutes at this transition. A fixed
        // sixty-minute UTC cadence that begins at local midnight therefore lands
        // on 02:30 after the change and can no longer be assigned wholly to local
        // tariff hours. Thirty-minute intervals remain aligned and are accepted.
        let start = utc(2024, 10, 5, 13, 30); // 2024-10-06 00:00 +10:30
        let error = IntervalGrid::new(start, 60, 4, chrono_tz::Australia::Lord_Howe).unwrap_err();
        assert_eq!(error.code(), "validation.interval_timezone_alignment");
        assert!(error.to_string().contains("02:30:00"));
        assert!(IntervalGrid::new(start, 30, 8, chrono_tz::Australia::Lord_Howe).is_ok());
    }

    #[test]
    fn intervals_must_divide_an_hour() {
        let start = utc(2024, 1, 1, 0, 0);
        assert!(IntervalGrid::new(start, 7, 10, chrono_tz::UTC).is_err());
        assert!(IntervalGrid::new(start, 0, 10, chrono_tz::UTC).is_err());
        assert!(IntervalGrid::new(start, 61, 10, chrono_tz::UTC).is_err());
        for good in [1, 5, 15, 30, 60] {
            assert!(
                IntervalGrid::new(start, good, 10, chrono_tz::UTC).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn empty_and_oversized_grids_are_rejected() {
        let start = utc(2024, 1, 1, 0, 0);
        assert_eq!(
            IntervalGrid::new(start, 15, 0, chrono_tz::UTC)
                .unwrap_err()
                .code(),
            "validation.empty_dataset"
        );
        assert_eq!(
            IntervalGrid::new(start, 15, limits::MAX_INTERVALS + 1, chrono_tz::UTC)
                .unwrap_err()
                .code(),
            "limit.intervals"
        );
    }

    #[test]
    fn a_grid_span_may_not_overflow_the_supported_timestamp_range() {
        let start = DateTime::<Utc>::MAX_UTC - TimeDelta::minutes(30);
        let error = IntervalGrid::new(start, 60, 2, chrono_tz::UTC).unwrap_err();
        assert_eq!(error.code(), "validation.interval_span");
    }

    #[test]
    fn spring_forward_gap_is_rejected_not_guessed() {
        // 2024-03-10 02:30 does not exist in America/New_York: 02:00 EST becomes
        // 03:00 EDT.
        let error = resolve_local(
            chrono_tz::America::New_York,
            naive(2024, 3, 10, 2, 30),
            Some(9),
        )
        .unwrap_err();
        assert_eq!(error.code(), "import.nonexistent_local_time");
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn fall_back_overlap_is_rejected_and_names_both_candidates() {
        // 2024-11-03 01:30 happens twice in America/New_York: once at -04:00 and
        // once at -05:00.
        let error = resolve_local(
            chrono_tz::America::New_York,
            naive(2024, 11, 3, 1, 30),
            Some(9),
        )
        .unwrap_err();
        assert_eq!(error.code(), "import.ambiguous_local_time");
        let message = error.to_string();
        assert!(message.contains("05:30:00+00:00"), "{message}");
        assert!(message.contains("06:30:00+00:00"), "{message}");
    }

    #[test]
    fn unambiguous_local_times_resolve() {
        let resolved =
            resolve_local(chrono_tz::America::New_York, naive(2024, 6, 1, 12, 0), None).unwrap();
        assert_eq!(resolved, utc(2024, 6, 1, 16, 0)); // EDT is UTC-4
        let resolved =
            resolve_local(chrono_tz::America::New_York, naive(2024, 1, 1, 12, 0), None).unwrap();
        assert_eq!(resolved, utc(2024, 1, 1, 17, 0)); // EST is UTC-5
    }

    #[test]
    fn month_bucketing_follows_local_time_not_utc() {
        // 2024-02-01 02:00 UTC is still 2024-01-31 21:00 in New York, so the
        // interval belongs to January's demand-charge bucket.
        let grid =
            IntervalGrid::new(utc(2024, 2, 1, 2, 0), 60, 24, chrono_tz::America::New_York).unwrap();
        assert_eq!(grid.local_month_of(0), (2024, 1));
        assert_eq!(grid.local_month_of(3), (2024, 2)); // 05:00 UTC = 00:00 EST
    }

    #[test]
    fn timezone_names_are_validated() {
        assert!(parse_timezone("America/New_York").is_ok());
        assert!(parse_timezone("UTC").is_ok());
        let error = parse_timezone("Mars/Olympus").unwrap_err();
        assert_eq!(error.code(), "validation.unknown_timezone");
        // A fixed offset is not an IANA zone and must not be silently accepted.
        assert!(parse_timezone("-05:00").is_err());
        assert!(parse_timezone("EST5EDT").is_ok()); // this one genuinely is in the tz database
    }
}
