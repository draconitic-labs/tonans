//! Hard bounds on every unbounded input the application accepts.
//!
//! These are enforced, not advisory. Each one exists because the corresponding
//! input is attacker- or accident-controlled and would otherwise let a single
//! file exhaust memory or wall-clock time. They are published in the report and
//! the provenance manifest so a reviewer can see the envelope a result was
//! produced inside.

/// Largest CSV upload accepted, in bytes.
pub const MAX_CSV_BYTES: usize = 64 * 1024 * 1024;

/// Largest number of data rows accepted from one CSV.
///
/// Three years of 15-minute data. Beyond this the interval engine's memory and
/// the report's chart decimation stop being honest about resolution.
pub const MAX_CSV_ROWS: usize = 105_120;

/// Largest number of columns accepted from one CSV header.
pub const MAX_CSV_COLUMNS: usize = 32;

/// Largest single CSV field, in bytes. Long fields are always malformed input.
pub const MAX_CSV_FIELD_BYTES: usize = 256;

/// Largest magnitude accepted for any imported numeric value.
///
/// Physical site data never approaches this. It exists to keep downstream
/// accumulation far from overflow and to reject sentinel values such as 1e30.
pub const MAX_ABS_VALUE: f64 = 1.0e9;

/// Largest interval count a project may hold. Matches [`MAX_CSV_ROWS`].
pub const MAX_INTERVALS: usize = MAX_CSV_ROWS;

/// Shortest and longest interval durations, in minutes.
pub const MIN_INTERVAL_MINUTES: u32 = 1;
/// See [`MIN_INTERVAL_MINUTES`].
pub const MAX_INTERVAL_MINUTES: u32 = 60;

/// Largest number of scenarios in one project.
pub const MAX_SCENARIOS: usize = 32;

/// Largest number of scenarios multiplied by intervals in one run.
///
/// Every interval of every scenario is kept in memory and canonicalized for hashing, at
/// roughly 6 kB per row. This fits 32 scenarios of hourly data, or 4 scenarios of a
/// five-minute year, in about 2.6 GB. Running out of memory stops the application, because an
/// allocation failure cannot be caught, so `README.md` says what the largest project needs.
pub const MAX_RUN_ROWS: usize = 420_480;

/// Largest number of axes in one sensitivity study.
pub const MAX_SENSITIVITY_AXES: usize = 2;

/// Largest number of values on one sensitivity axis.
pub const MAX_SENSITIVITY_VALUES_PER_AXIS: usize = 16;

/// Largest number of cells (engine runs) in one sensitivity study.
pub const MAX_SENSITIVITY_CELLS: usize = 256;

/// Largest number of outage windows in one scenario.
pub const MAX_OUTAGE_WINDOWS: usize = 16;

/// Largest number of entries in a manual dispatch schedule.
pub const MAX_SCHEDULE_ENTRIES: usize = 512;

/// Largest number of points in a temperature derating table.
pub const MAX_TEMPERATURE_POINTS: usize = 16;

/// Largest number of distinct demand-charge period labels.
pub const MAX_DEMAND_PERIODS: usize = 8;

/// Largest project file accepted on open, in bytes.
pub const MAX_PROJECT_BYTES: usize = 64 * 1024 * 1024;

/// Largest number of characters in a user-supplied name.
pub const MAX_NAME_CHARS: usize = 64;

/// Largest number of characters in a user-supplied free-text note.
pub const MAX_NOTES_CHARS: usize = 4_000;

/// Longest economic horizon, in years.
pub const MAX_HORIZON_YEARS: u32 = 40;

/// Largest fraction of intervals a gap policy may synthesize before the import
/// is rejected outright. A file that is five percent holes is not a data set.
pub const MAX_SYNTHESIZED_FRACTION: f64 = 0.05;

/// Largest number of synthesized-interval indices recorded individually. Past
/// this the count is still exact but the index list is truncated.
pub const MAX_RECORDED_SYNTHESIZED_INDICES: usize = 64;

/// Largest number of warnings retained per run. Warnings are deduplicated by
/// code before this bound is applied.
pub const MAX_WARNINGS: usize = 256;

/// Points per rendered chart series, after decimation.
pub const CHART_MAX_POINTS: usize = 1_200;

/// A stable, serializable description of the enforced envelope, embedded in
/// every report and provenance manifest.
// Not `Eq`: `max_synthesized_fraction` is a float. `PartialEq` is what a test needs
// and is all a float can honestly offer.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Limits {
    /// See [`MAX_CSV_BYTES`].
    pub max_csv_bytes: usize,
    /// See [`MAX_CSV_ROWS`].
    pub max_csv_rows: usize,
    /// See [`MAX_CSV_COLUMNS`].
    pub max_csv_columns: usize,
    /// See [`MAX_INTERVALS`].
    pub max_intervals: usize,
    /// See [`MAX_SCENARIOS`].
    pub max_scenarios: usize,
    /// See [`MAX_RUN_ROWS`].
    pub max_run_rows: usize,
    /// See [`MAX_SENSITIVITY_CELLS`].
    pub max_sensitivity_cells: usize,
    /// See [`MAX_OUTAGE_WINDOWS`].
    pub max_outage_windows: usize,
    /// See [`MAX_HORIZON_YEARS`].
    pub max_horizon_years: u32,
    /// See [`MAX_SYNTHESIZED_FRACTION`].
    pub max_synthesized_fraction: f64,
}

impl Limits {
    /// The compiled-in envelope.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            max_csv_bytes: MAX_CSV_BYTES,
            max_csv_rows: MAX_CSV_ROWS,
            max_csv_columns: MAX_CSV_COLUMNS,
            max_intervals: MAX_INTERVALS,
            max_scenarios: MAX_SCENARIOS,
            max_run_rows: MAX_RUN_ROWS,
            max_sensitivity_cells: MAX_SENSITIVITY_CELLS,
            max_outage_windows: MAX_OUTAGE_WINDOWS,
            max_horizon_years: MAX_HORIZON_YEARS,
            max_synthesized_fraction: MAX_SYNTHESIZED_FRACTION,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitivity_grid_cannot_exceed_the_cell_cap_by_construction() {
        // The two axis bounds must not multiply out past the cell cap, or the
        // cell cap would be the only thing standing between a user and a
        // 16x16x(3 years of 15-minute intervals) run.
        assert!(MAX_SENSITIVITY_VALUES_PER_AXIS.pow(2) <= MAX_SENSITIVITY_CELLS);
    }

    #[test]
    fn interval_bounds_divide_an_hour() {
        // Demand-charge months and outage windows are aligned on interval
        // boundaries; an interval that does not divide an hour would make local
        // calendar bucketing ambiguous. The importer accepts any interval in this
        // range that divides 60, so both ends of the range must themselves be
        // intervals it would accept — otherwise the advertised bound is a value
        // that cannot actually be used.
        let usable: Vec<u32> = (MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES)
            .filter(|minutes| 60 % minutes == 0)
            .collect();
        assert_eq!(usable.first(), Some(&MIN_INTERVAL_MINUTES));
        assert_eq!(usable.last(), Some(&MAX_INTERVAL_MINUTES));
    }
}
