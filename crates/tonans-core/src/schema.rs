//! The versioned local project schema.
//!
//! A project is a single self-contained JSON document: site, imported data,
//! scenarios, and every assumption behind them. Keeping the interval data inside
//! the project rather than beside it costs disk but buys two properties that
//! matter more — the file is *portable* (a colleague can reproduce a report from
//! it alone) and it is *hashable* (one digest identifies the exact analysis).
//!
//! Every struct here is `deny_unknown_fields`. A project written by a future
//! Tonans, or hand-edited into an unrecognised shape, is rejected loudly rather
//! than being read with fields silently defaulted. [`SCHEMA_VERSION`] plus
//! [`migrate`] is the only supported way in.
//!
//! [`SCHEMA_VERSION`]: crate::version::SCHEMA_VERSION

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canonical::Hash;
use crate::error::{Error, Result, Warning};
use crate::limits;
use crate::timegrid::{parse_timezone, IntervalGrid};
use crate::units::Unit;
use crate::version::{APP_VERSION, MODEL_VERSION, SCHEMA_VERSION};

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------

/// A Tonans project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// On-disk schema version. See [`migrate`].
    pub schema_version: u32,
    /// Version of Tonans that last wrote this file.
    pub app_version: String,
    /// Version of the energy model this project's results were last produced by.
    pub model_version: String,
    /// Opaque, stable project identifier.
    pub id: String,
    /// Human-readable project name.
    pub name: String,
    /// When the project was created.
    pub created_utc: DateTime<Utc>,
    /// When the project was last written.
    pub modified_utc: DateTime<Utc>,
    /// Where the site is and what it charges in.
    pub site: Site,
    /// Imported interval data. Absent until the first successful import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<Dataset>,
    /// Scenarios to run. Ordered; the order is preserved in every output.
    pub scenarios: Vec<Scenario>,
    /// The scenario every other scenario is compared against.
    pub baseline_scenario_id: String,
    /// Free-text analyst notes, reproduced in the report.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

impl Project {
    /// Create an empty project with one no-storage baseline scenario.
    ///
    /// `id` and `now_utc` are injected: this crate contains no clock and no
    /// randomness, which is what makes every result reproducible from the file
    /// alone. See `docs/ARCHITECTURE.md`.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        site: Site,
        now_utc: DateTime<Utc>,
    ) -> Result<Self> {
        let project = Self {
            schema_version: SCHEMA_VERSION,
            app_version: APP_VERSION.to_string(),
            model_version: MODEL_VERSION.to_string(),
            id: id.into(),
            name: name.into(),
            created_utc: now_utc,
            modified_utc: now_utc,
            site,
            dataset: None,
            scenarios: vec![Scenario::no_storage_baseline()],
            baseline_scenario_id: Scenario::BASELINE_ID.to_string(),
            notes: String::new(),
        };
        project.validate()?;
        Ok(project)
    }

    /// The scenario with the given id.
    #[must_use]
    pub fn scenario(&self, id: &str) -> Option<&Scenario> {
        self.scenarios.iter().find(|s| s.id == id)
    }

    /// The baseline scenario. Guaranteed to exist by [`Self::validate`].
    #[must_use]
    pub fn baseline(&self) -> Option<&Scenario> {
        self.scenario(&self.baseline_scenario_id)
    }

    /// The interval grid this project's data sits on, if data has been imported.
    pub fn grid(&self) -> Result<Option<IntervalGrid>> {
        let Some(dataset) = &self.dataset else {
            return Ok(None);
        };
        let timezone = parse_timezone(&self.site.timezone)?;
        Ok(Some(IntervalGrid::new(
            dataset.start_utc,
            dataset.interval_minutes,
            dataset.interval_count,
            timezone,
        )?))
    }

    /// Check every invariant that does not require the data set.
    ///
    /// Split from [`Self::validate_runnable`] on purpose: a project must be
    /// storable and editable *before* data is imported, so the assumptions can be
    /// entered in any order, but it must not be *runnable* until every
    /// cross-reference into the data resolves.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::Schema {
                code: "schema.version_unsupported",
                message: format!(
                    "this project declares schema version {found}, but this build of Tonans \
                     understands version {supported}",
                    found = self.schema_version,
                    supported = SCHEMA_VERSION
                ),
            });
        }
        check_name("project.name", &self.name)?;
        check_id("project.id", &self.id)?;
        if self.notes.chars().count() > limits::MAX_NOTES_CHARS {
            return Err(Error::limit(
                "limit.notes",
                limits::MAX_NOTES_CHARS as u64,
                self.notes.chars().count() as u64,
                format!("notes exceed {} characters", limits::MAX_NOTES_CHARS),
            ));
        }
        if self.modified_utc < self.created_utc {
            return Err(Error::validation(
                "validation.modified_before_created",
                "project.modified_utc",
                "the project's modification time precedes its creation time",
            ));
        }
        self.site.validate()?;

        if self.scenarios.is_empty() {
            return Err(Error::validation(
                "validation.no_scenarios",
                "project.scenarios",
                "a project must contain at least one scenario",
            ));
        }
        if self.scenarios.len() > limits::MAX_SCENARIOS {
            return Err(Error::limit(
                "limit.scenarios",
                limits::MAX_SCENARIOS as u64,
                self.scenarios.len() as u64,
                format!(
                    "{} scenarios exceeds the limit of {}",
                    self.scenarios.len(),
                    limits::MAX_SCENARIOS
                ),
            ));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.scenarios.len());
        for scenario in &self.scenarios {
            scenario.validate()?;
            if seen.contains(&scenario.id.as_str()) {
                return Err(Error::validation(
                    "validation.duplicate_scenario_id",
                    "project.scenarios[].id",
                    format!("scenario id '{}' is used more than once", scenario.id),
                ));
            }
            seen.push(&scenario.id);
        }
        if self.baseline().is_none() {
            return Err(Error::validation(
                "validation.unknown_baseline",
                "project.baseline_scenario_id",
                format!(
                    "baseline_scenario_id '{}' does not match any scenario",
                    self.baseline_scenario_id
                ),
            ));
        }
        if let Some(dataset) = &self.dataset {
            dataset.validate()?;
        }
        Ok(())
    }

    /// Check every invariant, including those that cross-reference the data set.
    ///
    /// Must pass before the engine will run. Returns the grid so callers do not
    /// rebuild it.
    pub fn validate_runnable(&self) -> Result<IntervalGrid> {
        self.validate()?;
        let Some(dataset) = &self.dataset else {
            return Err(Error::validation(
                "validation.no_dataset",
                "project.dataset",
                "no interval data has been imported yet; import a CSV before running scenarios",
            ));
        };
        let grid = self
            .grid()?
            .expect("dataset presence was just checked, so the grid must exist");

        for kind in [
            SeriesKind::Load,
            SeriesKind::CriticalLoad,
            SeriesKind::Generation,
        ] {
            if let Some(series) = dataset.series.get(&kind) {
                if series.values.len() != dataset.interval_count {
                    return Err(Error::validation(
                        "validation.series_length",
                        format!("dataset.series.{}", kind.as_str()),
                        format!(
                            "series '{}' has {} values but the data set declares {} intervals",
                            kind.as_str(),
                            series.values.len(),
                            dataset.interval_count
                        ),
                    ));
                }
            }
        }
        if !dataset.series.contains_key(&SeriesKind::Load) {
            return Err(Error::validation(
                "validation.missing_load_series",
                "dataset.series.load",
                "a load series is required to run a scenario",
            ));
        }

        for scenario in &self.scenarios {
            scenario.validate_against(dataset, &grid)?;
        }
        Ok(grid)
    }

    /// Warnings about the project as configured, independent of any run.
    #[must_use]
    pub fn advisories(&self) -> Vec<Warning> {
        let mut warnings = Vec::new();
        if let Some(baseline) = self.baseline() {
            if baseline.battery.is_some() {
                warnings.push(Warning::caution(
                    "project.baseline_has_storage",
                    format!(
                        "the baseline scenario '{}' includes a battery, so savings are reported \
                         against a storage case rather than against a no-storage site. Every \
                         economic figure is a difference from that baseline, not an absolute benefit.",
                        baseline.name
                    ),
                ));
            }
        }
        if let Some(dataset) = &self.dataset {
            if !dataset.series.contains_key(&SeriesKind::CriticalLoad) {
                warnings.push(Warning::caution(
                    "project.no_critical_load",
                    "no critical-load series was imported, so critical load is taken as zero. \
                     Outage results will show no unmet load regardless of battery size.",
                ));
            }
            if !dataset.series.contains_key(&SeriesKind::Generation) {
                warnings.push(Warning::info(
                    "project.no_generation",
                    "no generation series was imported, so on-site generation is taken as zero.",
                ));
            }
            let year_hours =
                f64::from(dataset.interval_minutes) / 60.0 * dataset.interval_count as f64;
            if year_hours < 8_000.0 {
                warnings.push(Warning::caution(
                    "project.partial_year",
                    format!(
                        "the data set covers {year_hours:.0} hours, less than a full year. Annual \
                         costs, demand charges, and every economic result are extrapolated from a \
                         partial year and will misstate seasonal effects."
                    ),
                ));
            }
        }
        warnings
    }
}

/// Where the site is, and what it charges in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// IANA timezone name, e.g. `America/New_York`. Fixes the local calendar used
    /// for demand-charge months and for interpreting naive CSV timestamps.
    pub timezone: String,
    /// ISO 4217 currency code. Only `USD` is supported; the field exists so that
    /// a report never renders a bare number and calls it money.
    pub currency: String,
}

impl Site {
    /// Validate the site.
    pub fn validate(&self) -> Result<()> {
        parse_timezone(&self.timezone)?;
        if self.currency != "USD" {
            return Err(Error::validation(
                "validation.unsupported_currency",
                "site.currency",
                format!(
                    "currency '{}' is not supported; this build handles USD only.",
                    self.currency
                ),
            ));
        }
        Ok(())
    }
}

impl Default for Site {
    fn default() -> Self {
        Self {
            timezone: "UTC".to_string(),
            currency: "USD".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Data set
// ---------------------------------------------------------------------------

/// Imported interval data, and everything needed to defend it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    /// Where the data came from.
    pub source: SourceFile,
    /// Interval duration.
    pub interval_minutes: u32,
    /// Start of the first interval.
    pub start_utc: DateTime<Utc>,
    /// Number of intervals.
    pub interval_count: usize,
    /// The imported series, keyed by role. Ordered, so serialization is stable.
    pub series: BTreeMap<SeriesKind, Series>,
    /// Per-interval demand-charge period labels, if a period column was mapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand_periods: Option<DemandPeriods>,
    /// What the importer was told to do about gaps.
    pub gap_policy: GapPolicy,
    /// How many intervals the gap policy synthesized. Zero under `Reject`.
    pub synthesized_interval_count: usize,
    /// Indices of synthesized intervals, truncated at
    /// [`limits::MAX_RECORDED_SYNTHESIZED_INDICES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synthesized_interval_indices: Vec<usize>,
    /// Warnings raised during import. Carried forever, into every report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub import_warnings: Vec<Warning>,
}

impl Dataset {
    /// The values of `kind`, or an all-zero series if it was never imported.
    ///
    /// Absent optional series are genuinely zero in this model — no generation
    /// means no generation — and [`Project::advisories`] tells the reader so.
    #[must_use]
    pub fn values_or_zero(&self, kind: SeriesKind) -> Vec<f64> {
        self.series
            .get(&kind)
            .map(|s| s.values.clone())
            .unwrap_or_else(|| vec![0.0; self.interval_count])
    }

    /// Validate the data set's internal consistency.
    pub fn validate(&self) -> Result<()> {
        if self.interval_count > limits::MAX_INTERVALS {
            return Err(Error::limit(
                "limit.intervals",
                limits::MAX_INTERVALS as u64,
                self.interval_count as u64,
                "data set exceeds the interval limit",
            ));
        }
        if !self.series.contains_key(&SeriesKind::Load) {
            return Err(Error::validation(
                "validation.missing_load_series",
                "dataset.series.load",
                "the data set has no load series",
            ));
        }
        for (kind, series) in &self.series {
            if series.values.len() != self.interval_count {
                return Err(Error::validation(
                    "validation.series_length",
                    format!("dataset.series.{}", kind.as_str()),
                    format!(
                        "series '{}' has {} values but the data set declares {} intervals",
                        kind.as_str(),
                        series.values.len(),
                        self.interval_count
                    ),
                ));
            }
            if series.unit != kind.unit() {
                return Err(Error::validation(
                    "validation.series_unit",
                    format!("dataset.series.{}.unit", kind.as_str()),
                    format!(
                        "series '{}' is stored as {} but must be {}",
                        kind.as_str(),
                        series.unit,
                        kind.unit()
                    ),
                ));
            }
            for (index, value) in series.values.iter().enumerate() {
                if !value.is_finite() {
                    return Err(Error::validation(
                        "validation.non_finite_series",
                        format!("dataset.series.{}", kind.as_str()),
                        format!(
                            "series '{}' holds a non-finite value at interval {index}",
                            kind.as_str()
                        ),
                    ));
                }
                if value.abs() > limits::MAX_ABS_VALUE {
                    return Err(Error::validation(
                        "validation.series_value_out_of_range",
                        format!("dataset.series.{}[{index}]", kind.as_str()),
                        format!(
                            "series '{}' contains {value}, beyond the magnitude limit of {}",
                            kind.as_str(),
                            limits::MAX_ABS_VALUE
                        ),
                    ));
                }
                if matches!(
                    kind,
                    SeriesKind::Load | SeriesKind::CriticalLoad | SeriesKind::Generation
                ) && *value < 0.0
                {
                    return Err(Error::validation(
                        "validation.negative_power",
                        format!("dataset.series.{}[{index}]", kind.as_str()),
                        format!("series '{}' contains negative power {value}", kind.as_str()),
                    ));
                }
                if *kind == SeriesKind::Temperature && !(-90.0..=70.0).contains(value) {
                    return Err(Error::validation(
                        "validation.temperature_out_of_range",
                        format!("dataset.series.temperature[{index}]"),
                        format!("temperature {value} C is outside the supported range -90 to 70 C"),
                    ));
                }
            }
        }
        if let Some(critical) = self.series.get(&SeriesKind::CriticalLoad) {
            let load = &self.series[&SeriesKind::Load];
            for (index, (critical_kw, load_kw)) in
                critical.values.iter().zip(&load.values).enumerate()
            {
                if critical_kw > &(load_kw + 1e-9) {
                    return Err(Error::validation(
                        "validation.critical_exceeds_load",
                        format!("dataset.series.critical_load[{index}]"),
                        format!("critical load {critical_kw} kW exceeds total load {load_kw} kW"),
                    ));
                }
            }
        }
        if let Some(periods) = &self.demand_periods {
            if periods.index.len() != self.interval_count {
                return Err(Error::validation(
                    "validation.demand_period_length",
                    "dataset.demand_periods.index",
                    "the demand-period index does not cover every interval",
                ));
            }
            if periods.labels.is_empty() || periods.labels.len() > limits::MAX_DEMAND_PERIODS {
                return Err(Error::limit(
                    "limit.demand_periods",
                    limits::MAX_DEMAND_PERIODS as u64,
                    periods.labels.len() as u64,
                    format!(
                        "{} distinct demand-charge periods; the limit is {}",
                        periods.labels.len(),
                        limits::MAX_DEMAND_PERIODS
                    ),
                ));
            }
            if let Some(bad) = periods
                .index
                .iter()
                .find(|i| usize::from(**i) >= periods.labels.len())
            {
                return Err(Error::validation(
                    "validation.demand_period_index",
                    "dataset.demand_periods.index",
                    format!("demand-period index {bad} has no matching label"),
                ));
            }
        }
        Ok(())
    }
}

/// Provenance of an imported file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    /// The file's name as supplied, after sanitization. Never a path.
    pub filename: String,
    /// SHA-256 of the exact bytes imported, before any parsing.
    pub sha256: Hash,
    /// Size of those bytes.
    pub size_bytes: u64,
    /// Data rows read, header excluded.
    pub row_count: usize,
    /// When the import happened.
    pub imported_utc: DateTime<Utc>,
    /// Which source header each series was read from. The audit trail for "how do
    /// you know that column was load".
    pub column_mapping: BTreeMap<String, String>,
    /// Source headers the user explicitly told the importer to ignore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_columns: Vec<String>,
}

/// One imported series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Series {
    /// The unit the values are stored in, always the canonical unit for the role.
    pub unit: Unit,
    /// One value per interval.
    pub values: Vec<f64>,
}

/// Interned per-interval demand-charge period labels.
///
/// A 35,040-element `Vec<String>` of the word "on-peak" would add megabytes to
/// every project file for no information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandPeriods {
    /// Distinct labels, in first-seen order.
    pub labels: Vec<String>,
    /// One index into `labels` per interval.
    pub index: Vec<u16>,
}

/// The role a series plays in the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesKind {
    /// Total site load.
    Load,
    /// The part of the load that must be served during an outage.
    CriticalLoad,
    /// On-site renewable generation, at the AC bus.
    Generation,
    /// Price paid per kWh imported from the grid.
    ImportPrice,
    /// Credit received per kWh exported to the grid.
    ExportPrice,
    /// Ambient temperature, for battery derating.
    Temperature,
}

impl SeriesKind {
    /// Every role, in schema order.
    pub const ALL: &'static [SeriesKind] = &[
        SeriesKind::Load,
        SeriesKind::CriticalLoad,
        SeriesKind::Generation,
        SeriesKind::ImportPrice,
        SeriesKind::ExportPrice,
        SeriesKind::Temperature,
    ];

    /// The canonical field name for this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SeriesKind::Load => "load",
            SeriesKind::CriticalLoad => "critical_load",
            SeriesKind::Generation => "generation",
            SeriesKind::ImportPrice => "import_price",
            SeriesKind::ExportPrice => "export_price",
            SeriesKind::Temperature => "temperature",
        }
    }

    /// The unit this role is always stored in, whatever unit it was imported in.
    #[must_use]
    pub const fn unit(self) -> Unit {
        match self {
            SeriesKind::Load | SeriesKind::CriticalLoad | SeriesKind::Generation => Unit::Kilowatt,
            SeriesKind::ImportPrice | SeriesKind::ExportPrice => Unit::MoneyPerKilowattHour,
            SeriesKind::Temperature => Unit::Celsius,
        }
    }

    /// Whether a scenario can be run without this role.
    #[must_use]
    pub const fn is_required(self) -> bool {
        matches!(self, SeriesKind::Load)
    }
}

/// What to do when the importer finds a hole in the timestamp sequence.
///
/// The default is [`GapPolicy::Reject`]. The other two exist because real meter
/// exports do drop intervals, and a user who understands their data may
/// deliberately choose to fill them — but the choice is theirs, it is recorded in
/// the project, it is reported as a warning on every artifact, and it is capped
/// by [`limits::MAX_SYNTHESIZED_FRACTION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GapPolicy {
    /// Any missing interval is an error. The only safe default.
    #[default]
    Reject,
    /// Fill missing intervals with zero, up to `max_consecutive_intervals` in a run.
    FillZero {
        /// Longest run of consecutive missing intervals that may be filled.
        max_consecutive_intervals: usize,
    },
    /// Fill missing intervals by holding the previous value, up to
    /// `max_consecutive_intervals` in a run.
    HoldLastValue {
        /// Longest run of consecutive missing intervals that may be filled.
        max_consecutive_intervals: usize,
    },
}

impl GapPolicy {
    /// How many consecutive missing intervals this policy will synthesize.
    #[must_use]
    pub const fn max_consecutive(self) -> usize {
        match self {
            GapPolicy::Reject => 0,
            GapPolicy::FillZero {
                max_consecutive_intervals,
            }
            | GapPolicy::HoldLastValue {
                max_consecutive_intervals,
            } => max_consecutive_intervals,
        }
    }

    /// A short phrase for the report's assumptions table.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            GapPolicy::Reject => "reject any file with a missing interval",
            GapPolicy::FillZero { .. } => "fill missing intervals with zero",
            GapPolicy::HoldLastValue { .. } => {
                "fill missing intervals by holding the previous value"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

/// One complete, self-describing set of assumptions to run.
///
/// Scenarios do not inherit from project-level defaults. Duplication in the file
/// is the price of a guarantee that matters more: what you read in a scenario is
/// exactly what the engine used, with no resolution step in between.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Stable identifier, lowercase kebab-case.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// The battery, if this scenario has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<BatterySpec>,
    /// Power-conversion limits at the AC bus.
    pub inverter: InverterSpec,
    /// The named rule the battery is dispatched by.
    pub dispatch: DispatchPolicy,
    /// What the grid charges.
    pub tariff: TariffSpec,
    /// What the system costs.
    pub economics: EconomicsSpec,
    /// How the battery is assumed to age.
    pub degradation: DegradationSpec,
    /// Outages to replay.
    pub outage: OutageSpec,
    /// State of charge at the first interval.
    pub initial_soc_pct: f64,
    /// Usable capacity at the first interval, as a percentage of nameplate. Lets a
    /// scenario start from an aged battery.
    pub initial_capacity_pct: f64,
}

impl Scenario {
    /// The id of the scenario created with a new project.
    pub const BASELINE_ID: &'static str = "baseline";

    /// A grid-only scenario: no battery, flat tariff, no outage.
    #[must_use]
    pub fn no_storage_baseline() -> Self {
        Self {
            id: Self::BASELINE_ID.to_string(),
            name: "Baseline (no storage)".to_string(),
            battery: None,
            inverter: InverterSpec {
                battery_power_kw: 0.0,
                site_export_limit_kw: None,
            },
            dispatch: DispatchPolicy::SelfConsumption,
            tariff: TariffSpec::default(),
            economics: EconomicsSpec::default(),
            degradation: DegradationSpec::default(),
            outage: OutageSpec::default(),
            initial_soc_pct: 0.0,
            initial_capacity_pct: 100.0,
        }
    }

    /// Check every invariant that does not require the data set.
    pub fn validate(&self) -> Result<()> {
        check_id("scenario.id", &self.id)?;
        check_name("scenario.name", &self.name)?;

        if let Some(battery) = &self.battery {
            battery.validate(&self.id)?;
            check_range(
                &format!("scenario[{}].initial_soc_pct", self.id),
                self.initial_soc_pct,
                battery.soc_min_pct,
                battery.soc_max_pct,
            )?;
        }
        check_range(
            &format!("scenario[{}].initial_capacity_pct", self.id),
            self.initial_capacity_pct,
            1.0,
            100.0,
        )?;
        self.inverter.validate(&self.id)?;
        self.dispatch.validate(&self.id, self.battery.as_ref())?;
        self.tariff.validate(&self.id)?;
        self.economics.validate(&self.id)?;
        self.degradation.validate(&self.id)?;
        self.outage.validate(&self.id)?;

        if self.battery.is_none() && self.inverter.battery_power_kw > 0.0 {
            return Err(Error::validation(
                "validation.inverter_without_battery",
                format!("scenario[{}].inverter.battery_power_kw", self.id),
                "this scenario has no battery, so its battery inverter rating must be 0 kW",
            ));
        }
        Ok(())
    }

    /// Check the invariants that cross-reference the data set.
    pub fn validate_against(&self, dataset: &Dataset, grid: &IntervalGrid) -> Result<()> {
        // A manual schedule and an outage window both address absolute instants.
        // Both must land on interval boundaries and inside the data.
        if let DispatchPolicy::ManualSchedule { entries } = &self.dispatch {
            let mut previous_end: Option<usize> = None;
            for (position, entry) in entries.iter().enumerate() {
                let field = format!("scenario[{}].dispatch.entries[{position}]", self.id);
                let start = grid.boundary_index(entry.start_utc).ok_or_else(|| {
                    Error::validation(
                        "validation.schedule_not_aligned",
                        &field,
                        format!(
                            "schedule entry starts at {} which is not an interval boundary inside \
                             the imported data ({} to {}, {}-minute intervals)",
                            entry.start_utc.to_rfc3339(),
                            grid.start_utc().to_rfc3339(),
                            grid.end_utc().to_rfc3339(),
                            grid.interval_minutes()
                        ),
                    )
                })?;
                let end = grid.boundary_index(entry.end_utc).ok_or_else(|| {
                    Error::validation(
                        "validation.schedule_not_aligned",
                        &field,
                        format!(
                            "schedule entry ends at {} which is not an interval boundary inside \
                             the imported data",
                            entry.end_utc.to_rfc3339()
                        ),
                    )
                })?;
                if start >= end {
                    return Err(Error::validation(
                        "validation.schedule_empty",
                        &field,
                        "schedule entry ends at or before it starts",
                    ));
                }
                if previous_end.is_some_and(|previous| start < previous) {
                    return Err(Error::validation(
                        "validation.schedule_overlap",
                        &field,
                        "schedule entries must be sorted by start time and must not overlap",
                    ));
                }
                previous_end = Some(end);
            }
        }

        for (position, window) in self.outage.windows.iter().enumerate() {
            let field = format!("scenario[{}].outage.windows[{position}]", self.id);
            let start = grid.boundary_index(window.start_utc).ok_or_else(|| {
                Error::validation(
                    "validation.outage_not_aligned",
                    &field,
                    format!(
                        "outage starts at {} which is not an interval boundary inside the imported \
                         data ({} to {})",
                        window.start_utc.to_rfc3339(),
                        grid.start_utc().to_rfc3339(),
                        grid.end_utc().to_rfc3339()
                    ),
                )
            })?;
            let end = grid.boundary_index(window.end_utc).ok_or_else(|| {
                Error::validation(
                    "validation.outage_not_aligned",
                    &field,
                    format!(
                        "outage ends at {} which is not an interval boundary inside the imported data",
                        window.end_utc.to_rfc3339()
                    ),
                )
            })?;
            if start >= end {
                return Err(Error::validation(
                    "validation.outage_empty",
                    &field,
                    "outage window ends at or before it starts",
                ));
            }
        }

        // Temperature derating without a temperature series would silently do
        // nothing, which is worse than refusing to run.
        if let Some(battery) = &self.battery {
            if battery.temperature_derate.is_some()
                && !dataset.series.contains_key(&SeriesKind::Temperature)
            {
                return Err(Error::validation(
                    "validation.derate_without_temperature",
                    format!("scenario[{}].battery.temperature_derate", self.id),
                    "temperature derating is configured but no temperature series was imported. \
                     Import a temperature_c column, or remove the derating table.",
                ));
            }
        }

        // Every demand-charge period present in the data must have a price, or the
        // demand charge for those intervals would silently be zero.
        if !self.tariff.demand_prices_per_kw_month_usd.is_empty() {
            let labels: Vec<&str> = match &dataset.demand_periods {
                Some(periods) => periods.labels.iter().map(String::as_str).collect(),
                None => vec![TariffSpec::DEFAULT_DEMAND_PERIOD],
            };
            let missing: Vec<&str> = labels
                .iter()
                .copied()
                .filter(|label| {
                    !self
                        .tariff
                        .demand_prices_per_kw_month_usd
                        .contains_key(*label)
                })
                .collect();
            if !missing.is_empty() {
                return Err(Error::validation(
                    "validation.demand_price_missing",
                    format!(
                        "scenario[{}].tariff.demand_prices_per_kw_month_usd",
                        self.id
                    ),
                    format!(
                        "the data set uses demand-charge period(s) {} but no price is defined for \
                         them. Define a price for every period, or remove the demand charge.",
                        missing.join(", ")
                    ),
                ));
            }
        }

        // A price series referenced but never imported would be read as zero.
        for (source, kind, field) in [
            (
                &self.tariff.import,
                SeriesKind::ImportPrice,
                "tariff.import",
            ),
            (
                &self.tariff.export,
                SeriesKind::ExportPrice,
                "tariff.export",
            ),
        ] {
            if matches!(source, RateSource::Series) && !dataset.series.contains_key(&kind) {
                return Err(Error::validation(
                    "validation.rate_series_missing",
                    format!("scenario[{}].{field}", self.id),
                    format!(
                        "this scenario prices energy from the imported '{}' series, but no such \
                         series was imported. Import the column, or switch to a flat rate.",
                        kind.as_str()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Battery assumptions.
///
/// Every power and energy limit is stated **at the AC bus**. Conversion losses
/// live entirely in [`Self::charge_efficiency_pct`] and
/// [`Self::discharge_efficiency_pct`]; the inverter contributes a power ceiling
/// and no loss of its own. Splitting losses across both would double-count them,
/// which is the single most common way an energy model quietly overstates a
/// battery. See `docs/MODEL.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatterySpec {
    /// Nameplate energy capacity.
    pub nameplate_energy_kwh: f64,
    /// Maximum charge power drawn from the AC bus.
    pub max_charge_kw: f64,
    /// Maximum discharge power delivered to the AC bus.
    pub max_discharge_kw: f64,
    /// Lowest state of charge the battery may reach, ever.
    pub soc_min_pct: f64,
    /// Highest state of charge the battery may reach.
    pub soc_max_pct: f64,
    /// State of charge held back for outages. Grid-connected dispatch will not
    /// discharge below this; an outage may.
    pub reserve_soc_pct: f64,
    /// Fraction of AC energy drawn that is retained in storage.
    pub charge_efficiency_pct: f64,
    /// Fraction of stored energy that reaches the AC bus.
    pub discharge_efficiency_pct: f64,
    /// Standing loss, as a percentage of stored energy per 730-hour month.
    pub self_discharge_pct_per_month: f64,
    /// Optional ambient-temperature derating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_derate: Option<TemperatureDerate>,
}

impl BatterySpec {
    /// Round-trip efficiency implied by the two one-way efficiencies.
    #[must_use]
    pub fn round_trip_efficiency_pct(&self) -> f64 {
        self.charge_efficiency_pct * self.discharge_efficiency_pct / 100.0
    }

    fn validate(&self, scenario: &str) -> Result<()> {
        let at = |field: &str| format!("scenario[{scenario}].battery.{field}");
        check_range(
            &at("nameplate_energy_kwh"),
            self.nameplate_energy_kwh,
            0.001,
            1.0e7,
        )?;
        check_range(&at("max_charge_kw"), self.max_charge_kw, 0.0, 1.0e7)?;
        check_range(&at("max_discharge_kw"), self.max_discharge_kw, 0.0, 1.0e7)?;
        check_range(&at("soc_min_pct"), self.soc_min_pct, 0.0, 100.0)?;
        check_range(&at("soc_max_pct"), self.soc_max_pct, 0.0, 100.0)?;
        check_range(&at("reserve_soc_pct"), self.reserve_soc_pct, 0.0, 100.0)?;
        check_range(
            &at("charge_efficiency_pct"),
            self.charge_efficiency_pct,
            1.0,
            100.0,
        )?;
        check_range(
            &at("discharge_efficiency_pct"),
            self.discharge_efficiency_pct,
            1.0,
            100.0,
        )?;
        check_range(
            &at("self_discharge_pct_per_month"),
            self.self_discharge_pct_per_month,
            0.0,
            50.0,
        )?;

        if self.soc_min_pct >= self.soc_max_pct {
            return Err(Error::validation(
                "validation.soc_envelope",
                at("soc_min_pct"),
                format!(
                    "soc_min_pct ({}) must be below soc_max_pct ({})",
                    self.soc_min_pct, self.soc_max_pct
                ),
            ));
        }
        if self.reserve_soc_pct < self.soc_min_pct || self.reserve_soc_pct > self.soc_max_pct {
            return Err(Error::validation(
                "validation.reserve_outside_envelope",
                at("reserve_soc_pct"),
                format!(
                    "reserve_soc_pct ({}) must lie between soc_min_pct ({}) and soc_max_pct ({})",
                    self.reserve_soc_pct, self.soc_min_pct, self.soc_max_pct
                ),
            ));
        }
        if self.max_charge_kw == 0.0 && self.max_discharge_kw == 0.0 {
            return Err(Error::validation(
                "validation.battery_cannot_move_energy",
                at("max_discharge_kw"),
                "a battery with zero charge and zero discharge power can never move energy; \
                 remove the battery instead",
            ));
        }
        if let Some(derate) = &self.temperature_derate {
            derate.validate(scenario)?;
        }
        Ok(())
    }
}

/// Ambient-temperature derating, as a piecewise-linear table.
///
/// Interpolated linearly between points and clamped outside the outermost ones.
/// This is a transparent lookup, not a thermal model of the pack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemperatureDerate {
    /// Points, strictly ascending in temperature. At least two.
    pub points: Vec<TemperatureDeratePoint>,
}

impl TemperatureDerate {
    /// Derating factors at `temperature_c`, each as a fraction in `0.0..=1.0`.
    #[must_use]
    pub fn factors_at(&self, temperature_c: f64) -> (f64, f64) {
        let points = &self.points;
        if temperature_c <= points[0].temperature_c {
            return (
                points[0].power_limit_pct / 100.0,
                points[0].usable_capacity_pct / 100.0,
            );
        }
        let last = &points[points.len() - 1];
        if temperature_c >= last.temperature_c {
            return (
                last.power_limit_pct / 100.0,
                last.usable_capacity_pct / 100.0,
            );
        }
        for window in points.windows(2) {
            let (low, high) = (&window[0], &window[1]);
            if temperature_c >= low.temperature_c && temperature_c <= high.temperature_c {
                let span = high.temperature_c - low.temperature_c;
                // `span > 0` is guaranteed by validate(): temperatures strictly ascend.
                let t = (temperature_c - low.temperature_c) / span;
                let power = low.power_limit_pct + t * (high.power_limit_pct - low.power_limit_pct);
                let capacity = low.usable_capacity_pct
                    + t * (high.usable_capacity_pct - low.usable_capacity_pct);
                return (power / 100.0, capacity / 100.0);
            }
        }
        // Unreachable given the bounds checks above; clamping to the last point is
        // the conservative answer rather than a panic in a release binary.
        (
            last.power_limit_pct / 100.0,
            last.usable_capacity_pct / 100.0,
        )
    }

    fn validate(&self, scenario: &str) -> Result<()> {
        let field = format!("scenario[{scenario}].battery.temperature_derate.points");
        if self.points.len() < 2 {
            return Err(Error::validation(
                "validation.derate_table_too_small",
                &field,
                "a derating table needs at least two points to interpolate between",
            ));
        }
        if self.points.len() > limits::MAX_TEMPERATURE_POINTS {
            return Err(Error::limit(
                "limit.temperature_points",
                limits::MAX_TEMPERATURE_POINTS as u64,
                self.points.len() as u64,
                format!(
                    "{} derating points; the limit is {}",
                    self.points.len(),
                    limits::MAX_TEMPERATURE_POINTS
                ),
            ));
        }
        for (index, point) in self.points.iter().enumerate() {
            check_range(
                &format!("{field}[{index}].temperature_c"),
                point.temperature_c,
                -60.0,
                90.0,
            )?;
            check_range(
                &format!("{field}[{index}].power_limit_pct"),
                point.power_limit_pct,
                0.0,
                100.0,
            )?;
            check_range(
                &format!("{field}[{index}].usable_capacity_pct"),
                point.usable_capacity_pct,
                0.0,
                100.0,
            )?;
            if index > 0 && point.temperature_c <= self.points[index - 1].temperature_c {
                return Err(Error::validation(
                    "validation.derate_table_unsorted",
                    format!("{field}[{index}].temperature_c"),
                    "derating points must ascend strictly in temperature",
                ));
            }
        }
        Ok(())
    }
}

/// One row of a [`TemperatureDerate`] table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemperatureDeratePoint {
    /// Ambient temperature at this point.
    pub temperature_c: f64,
    /// Charge and discharge power available, as a percentage of the rating.
    ///
    /// Deliberately *not* called "power factor": in an energy product that phrase
    /// already means cos φ, which Tonans does not model at all.
    pub power_limit_pct: f64,
    /// Usable energy capacity, as a percentage of nameplate.
    pub usable_capacity_pct: f64,
}

/// Power-conversion limits at the AC bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InverterSpec {
    /// Rating of the battery's power-conversion system. Caps charge and discharge
    /// power. Contributes no loss: see [`BatterySpec`].
    pub battery_power_kw: f64,
    /// Site export ceiling, e.g. a limited-export interconnection agreement.
    /// `None` means export is unlimited. `Some(0.0)` means non-export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_export_limit_kw: Option<f64>,
}

impl InverterSpec {
    fn validate(&self, scenario: &str) -> Result<()> {
        check_range(
            &format!("scenario[{scenario}].inverter.battery_power_kw"),
            self.battery_power_kw,
            0.0,
            1.0e7,
        )?;
        if let Some(limit) = self.site_export_limit_kw {
            check_range(
                &format!("scenario[{scenario}].inverter.site_export_limit_kw"),
                limit,
                0.0,
                1.0e7,
            )?;
        }
        Ok(())
    }
}

/// The named rule the battery is dispatched by.
///
/// These are rules, not an optimiser. Each is evaluated interval by interval with
/// no knowledge of the future, so a result is reproducible and explicable — and
/// so that Tonans never claims an optimum it did not compute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchPolicy {
    /// Store surplus generation; discharge to cover net load. Never charges from
    /// the grid, never exports from the battery.
    SelfConsumption,

    /// Hold grid import at or below a ceiling. Charges from surplus generation,
    /// and optionally from the grid within the remaining headroom.
    PeakLimit {
        /// The import ceiling to defend.
        import_limit_kw: f64,
        /// Whether the battery may charge from the grid to prepare for a peak.
        allow_grid_charge: bool,
        /// Grid charging stops at this state of charge.
        grid_charge_target_soc_pct: f64,
    },

    /// Refill the outage reserve before doing anything else. Below the target the
    /// battery only charges; above it, it behaves as [`Self::SelfConsumption`]
    /// with the target as its floor.
    ReserveFirst {
        /// The state of charge to reach and hold.
        reserve_target_soc_pct: f64,
        /// Whether the battery may charge from the grid to reach the target.
        allow_grid_charge: bool,
    },

    /// Follow an explicit setpoint schedule. Outside every entry the battery is
    /// idle. Setpoints are clamped by the physical limits, and a clamp is a
    /// warning, never a silent adjustment.
    ManualSchedule {
        /// Entries, sorted by start and non-overlapping.
        entries: Vec<ScheduleEntry>,
    },
}

impl DispatchPolicy {
    /// The policy's name, for reports and scenario cards.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            DispatchPolicy::SelfConsumption => "Self-consumption",
            DispatchPolicy::PeakLimit { .. } => "Peak limit",
            DispatchPolicy::ReserveFirst { .. } => "Reserve first",
            DispatchPolicy::ManualSchedule { .. } => "Manual schedule",
        }
    }

    fn validate(&self, scenario: &str, battery: Option<&BatterySpec>) -> Result<()> {
        if battery.is_none() && !matches!(self, DispatchPolicy::SelfConsumption) {
            return Err(Error::validation(
                "validation.dispatch_without_battery",
                format!("scenario[{scenario}].dispatch"),
                "peak-limit, reserve-first, and manual-schedule dispatch require a battery; use \
                 self-consumption for a no-storage baseline",
            ));
        }
        match self {
            DispatchPolicy::SelfConsumption => Ok(()),
            DispatchPolicy::PeakLimit {
                import_limit_kw,
                grid_charge_target_soc_pct,
                ..
            } => {
                check_range(
                    &format!("scenario[{scenario}].dispatch.import_limit_kw"),
                    *import_limit_kw,
                    0.0,
                    1.0e7,
                )?;
                check_soc_target(
                    &format!("scenario[{scenario}].dispatch.grid_charge_target_soc_pct"),
                    *grid_charge_target_soc_pct,
                    battery,
                )
            }
            DispatchPolicy::ReserveFirst {
                reserve_target_soc_pct,
                ..
            } => check_soc_target(
                &format!("scenario[{scenario}].dispatch.reserve_target_soc_pct"),
                *reserve_target_soc_pct,
                battery,
            ),
            DispatchPolicy::ManualSchedule { entries } => {
                if entries.len() > limits::MAX_SCHEDULE_ENTRIES {
                    return Err(Error::limit(
                        "limit.schedule_entries",
                        limits::MAX_SCHEDULE_ENTRIES as u64,
                        entries.len() as u64,
                        format!(
                            "{} schedule entries; the limit is {}",
                            entries.len(),
                            limits::MAX_SCHEDULE_ENTRIES
                        ),
                    ));
                }
                for (index, entry) in entries.iter().enumerate() {
                    check_range(
                        &format!("scenario[{scenario}].dispatch.entries[{index}].battery_power_kw"),
                        entry.battery_power_kw,
                        -1.0e7,
                        1.0e7,
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// One entry of a [`DispatchPolicy::ManualSchedule`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEntry {
    /// First interval this setpoint applies to. Must be an interval boundary.
    pub start_utc: DateTime<Utc>,
    /// First interval it no longer applies to. Must be an interval boundary.
    pub end_utc: DateTime<Utc>,
    /// Battery power at the AC bus. Positive discharges, negative charges.
    pub battery_power_kw: f64,
}

/// What the grid charges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TariffSpec {
    /// Where the import price comes from.
    pub import: RateSource,
    /// Where the export credit comes from.
    pub export: RateSource,
    /// Scales whatever the import source yields. The handle a tariff sensitivity
    /// turns, and the reason a time-of-use price series can be stressed without
    /// re-importing it.
    pub import_price_multiplier: f64,
    /// Scales whatever the export source yields.
    pub export_price_multiplier: f64,
    /// Demand price per period label. Empty means no demand charge.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub demand_prices_per_kw_month_usd: BTreeMap<String, f64>,
    /// Fixed monthly charge, applied once per local calendar month in the data.
    pub fixed_charge_usd_per_month: f64,
}

impl TariffSpec {
    /// The label used for every interval when the data has no period column.
    pub const DEFAULT_DEMAND_PERIOD: &'static str = "all";

    fn validate(&self, scenario: &str) -> Result<()> {
        let at = |field: &str| format!("scenario[{scenario}].tariff.{field}");
        self.import.validate(&at("import"))?;
        self.export.validate(&at("export"))?;
        check_range(
            &at("import_price_multiplier"),
            self.import_price_multiplier,
            0.0,
            1000.0,
        )?;
        check_range(
            &at("export_price_multiplier"),
            self.export_price_multiplier,
            0.0,
            1000.0,
        )?;
        check_range(
            &at("fixed_charge_usd_per_month"),
            self.fixed_charge_usd_per_month,
            0.0,
            1.0e7,
        )?;
        if self.demand_prices_per_kw_month_usd.len() > limits::MAX_DEMAND_PERIODS {
            return Err(Error::limit(
                "limit.demand_periods",
                limits::MAX_DEMAND_PERIODS as u64,
                self.demand_prices_per_kw_month_usd.len() as u64,
                "too many demand-charge periods",
            ));
        }
        for (label, price) in &self.demand_prices_per_kw_month_usd {
            check_name(&at("demand_prices_per_kw_month_usd"), label)?;
            check_range(
                &at(&format!("demand_prices_per_kw_month_usd[{label}]")),
                *price,
                0.0,
                1.0e6,
            )?;
        }
        Ok(())
    }
}

impl Default for TariffSpec {
    fn default() -> Self {
        Self {
            import: RateSource::Flat {
                price_per_kwh_usd: 0.15,
            },
            export: RateSource::Flat {
                price_per_kwh_usd: 0.05,
            },
            import_price_multiplier: 1.0,
            export_price_multiplier: 1.0,
            demand_prices_per_kw_month_usd: BTreeMap::new(),
            fixed_charge_usd_per_month: 0.0,
        }
    }
}

/// Where an energy price comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum RateSource {
    /// One price for every interval.
    Flat {
        /// The price.
        price_per_kwh_usd: f64,
    },
    /// The per-interval price imported from the CSV. This is how time-of-use and
    /// real-time tariffs are expressed: as data, not as a schedule language.
    Series,
}

impl RateSource {
    fn validate(&self, field: &str) -> Result<()> {
        match self {
            // Negative prices are real. Wholesale markets go negative, and an
            // export credit can be a fee. The engine must handle it; only the
            // magnitude is bounded.
            RateSource::Flat { price_per_kwh_usd } => {
                check_range(field, *price_per_kwh_usd, -100.0, 100.0)
            }
            RateSource::Series => Ok(()),
        }
    }
}

/// What the system costs, and how the money is discounted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomicsSpec {
    /// Total installed cost, spent at year zero.
    pub capex_usd: f64,
    /// Fixed operating cost per year, in year-one money.
    pub annual_opex_usd: f64,
    /// How many years the cash flow runs for.
    pub horizon_years: u32,
    /// Discount rate applied to future cash flows.
    pub discount_rate_pct_per_year: f64,
    /// Annual escalation applied to energy savings.
    pub energy_price_escalation_pct_per_year: f64,
    /// What a replacement costs and when it is triggered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<ReplacementSpec>,
}

impl EconomicsSpec {
    fn validate(&self, scenario: &str) -> Result<()> {
        let at = |field: &str| format!("scenario[{scenario}].economics.{field}");
        check_range(&at("capex_usd"), self.capex_usd, 0.0, 1.0e9)?;
        check_range(&at("annual_opex_usd"), self.annual_opex_usd, 0.0, 1.0e9)?;
        check_range(
            &at("discount_rate_pct_per_year"),
            self.discount_rate_pct_per_year,
            0.0,
            50.0,
        )?;
        check_range(
            &at("energy_price_escalation_pct_per_year"),
            self.energy_price_escalation_pct_per_year,
            -20.0,
            20.0,
        )?;
        if self.horizon_years == 0 || self.horizon_years > limits::MAX_HORIZON_YEARS {
            return Err(Error::validation(
                "validation.horizon",
                at("horizon_years"),
                format!(
                    "horizon must be between 1 and {} years, got {}",
                    limits::MAX_HORIZON_YEARS,
                    self.horizon_years
                ),
            ));
        }
        if let Some(replacement) = &self.replacement {
            check_range(
                &at("replacement.cost_usd"),
                replacement.cost_usd,
                0.0,
                1.0e9,
            )?;
            check_range(
                &at("replacement.trigger_capacity_pct"),
                replacement.trigger_capacity_pct,
                1.0,
                99.0,
            )?;
        }
        Ok(())
    }
}

impl Default for EconomicsSpec {
    fn default() -> Self {
        Self {
            capex_usd: 0.0,
            annual_opex_usd: 0.0,
            horizon_years: 15,
            discount_rate_pct_per_year: 6.0,
            energy_price_escalation_pct_per_year: 2.0,
            replacement: None,
        }
    }
}

/// When the battery is replaced, and what that costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementSpec {
    /// Cost of one replacement.
    pub cost_usd: f64,
    /// Replacement happens in the first year that usable capacity would start
    /// below this percentage of nameplate.
    pub trigger_capacity_pct: f64,
}

/// How the battery is assumed to age.
///
/// This is an *approximation*, stated as one, everywhere it appears. It is a
/// compounding fade in two terms — calendar and cycling — plus a throughput cost. It
/// is not an electrochemical lifetime model, it does not know about depth of
/// discharge, C-rate, or temperature history, and a vendor's warranty curve
/// should override it whenever one exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DegradationSpec {
    /// Capacity lost per year regardless of use.
    pub calendar_fade_pct_per_year: f64,
    /// Capacity lost per equivalent full cycle, where one cycle is one discharge
    /// of the usable envelope.
    pub cycle_fade_pct_per_cycle: f64,
    /// A wear cost booked against every kWh the battery discharges.
    pub throughput_cost_per_kwh_throughput_usd: f64,
    /// How year-one benefit is carried into later years as the battery fades.
    pub benefit_scaling: BenefitScaling,
}

impl DegradationSpec {
    fn validate(&self, scenario: &str) -> Result<()> {
        let at = |field: &str| format!("scenario[{scenario}].degradation.{field}");
        check_range(
            &at("calendar_fade_pct_per_year"),
            self.calendar_fade_pct_per_year,
            0.0,
            50.0,
        )?;
        check_range(
            &at("cycle_fade_pct_per_cycle"),
            self.cycle_fade_pct_per_cycle,
            0.0,
            1.0,
        )?;
        check_range(
            &at("throughput_cost_per_kwh_throughput_usd"),
            self.throughput_cost_per_kwh_throughput_usd,
            0.0,
            10.0,
        )?;
        Ok(())
    }
}

impl Default for DegradationSpec {
    fn default() -> Self {
        Self {
            calendar_fade_pct_per_year: 0.0,
            cycle_fade_pct_per_cycle: 0.0,
            throughput_cost_per_kwh_throughput_usd: 0.0,
            benefit_scaling: BenefitScaling::ProportionalToUsableCapacity,
        }
    }
}

/// How a faded battery's benefit is carried into later years.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenefitScaling {
    /// Year-N benefit is year-one benefit times year-N usable capacity.
    ///
    /// A first-order approximation, and Tonans says so in the report. Tonans
    /// simulates one representative year; re-simulating every year against a
    /// faded battery would be more faithful, and is a documented limitation in
    /// `docs/MODEL.md`.
    ProportionalToUsableCapacity,
    /// Year-N benefit equals year-one benefit. Use when fade is handled outside
    /// the model, e.g. by a vendor capacity guarantee.
    Constant,
}

/// Outages to replay.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutageSpec {
    /// Windows during which the grid is unavailable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<OutageWindow>,
}

impl OutageSpec {
    fn validate(&self, scenario: &str) -> Result<()> {
        if self.windows.len() > limits::MAX_OUTAGE_WINDOWS {
            return Err(Error::limit(
                "limit.outage_windows",
                limits::MAX_OUTAGE_WINDOWS as u64,
                self.windows.len() as u64,
                format!(
                    "{} outage windows; the limit is {}",
                    self.windows.len(),
                    limits::MAX_OUTAGE_WINDOWS
                ),
            ));
        }
        for (index, window) in self.windows.iter().enumerate() {
            if window.end_utc <= window.start_utc {
                return Err(Error::validation(
                    "validation.outage_empty",
                    format!("scenario[{scenario}].outage.windows[{index}]"),
                    "outage window ends at or before it starts",
                ));
            }
            if index > 0 && window.start_utc < self.windows[index - 1].end_utc {
                return Err(Error::validation(
                    "validation.outage_overlap",
                    format!("scenario[{scenario}].outage.windows[{index}]"),
                    "outage windows must be sorted by start time and must not overlap",
                ));
            }
        }
        Ok(())
    }
}

/// One outage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutageWindow {
    /// When the grid goes away. Must be an interval boundary.
    pub start_utc: DateTime<Utc>,
    /// When it comes back. Must be an interval boundary.
    pub end_utc: DateTime<Utc>,
    /// What to call it in the report.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

/// Read a project from JSON, migrating older schema versions forward.
///
/// Version 1 is the only version that has ever shipped, so this function has no
/// migration to perform yet. It exists — and is tested — because the *shape* of
/// the upgrade path has to be settled before there is a second version, not
/// after. A file from a newer Tonans is rejected with a message that says so,
/// rather than being parsed with unknown fields dropped.
pub fn migrate(json: &str) -> Result<Project> {
    #[derive(Deserialize)]
    struct VersionProbe {
        schema_version: u32,
    }

    if json.len() > limits::MAX_PROJECT_BYTES {
        return Err(Error::limit(
            "limit.project_bytes",
            limits::MAX_PROJECT_BYTES as u64,
            json.len() as u64,
            format!(
                "project file is {} bytes; the limit is {}",
                json.len(),
                limits::MAX_PROJECT_BYTES
            ),
        ));
    }

    let probe: VersionProbe = serde_json::from_str(json).map_err(|e| Error::Schema {
        code: "schema.unreadable",
        message: format!(
            "this file is not a Tonans project: {e}. A project is JSON with a \
             'schema_version' field."
        ),
    })?;

    match probe.schema_version {
        0 => Err(Error::Schema {
            code: "schema.version_unsupported",
            message: "schema version 0 was never released".to_string(),
        }),
        1 => {
            let project: Project = serde_json::from_str(json).map_err(|e| Error::Schema {
                code: "schema.invalid",
                message: format!("this project file is not valid for schema version 1: {e}"),
            })?;
            project.validate()?;
            Ok(project)
        }
        newer => Err(Error::Schema {
            code: "schema.version_from_the_future",
            message: format!(
                "this project was written by a newer Tonans (schema version {newer}); this build \
                 understands version {SCHEMA_VERSION}. Upgrade Tonans to open it."
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Shared validators
// ---------------------------------------------------------------------------

fn check_range(field: &str, value: f64, low: f64, high: f64) -> Result<()> {
    if !value.is_finite() {
        return Err(Error::validation(
            "validation.non_finite",
            field,
            format!("{field} must be a finite number, got {value}"),
        ));
    }
    if value < low || value > high {
        return Err(Error::validation(
            "validation.out_of_range",
            field,
            format!("{field} must be between {low} and {high}, got {value}"),
        ));
    }
    Ok(())
}

fn check_soc_target(field: &str, value: f64, battery: Option<&BatterySpec>) -> Result<()> {
    check_range(field, value, 0.0, 100.0)?;
    if let Some(battery) = battery {
        if value < battery.soc_min_pct || value > battery.soc_max_pct {
            return Err(Error::validation(
                "validation.soc_target_outside_envelope",
                field,
                format!(
                    "{field} is {value}%, outside the battery's usable envelope of \
                     {min}% to {max}%",
                    min = battery.soc_min_pct,
                    max = battery.soc_max_pct
                ),
            ));
        }
    }
    Ok(())
}

fn check_name(field: &str, value: &str) -> Result<()> {
    let length = value.chars().count();
    if length == 0 {
        return Err(Error::validation(
            "validation.empty_name",
            field,
            format!("{field} must not be empty"),
        ));
    }
    if length > limits::MAX_NAME_CHARS {
        return Err(Error::limit(
            "limit.name_length",
            limits::MAX_NAME_CHARS as u64,
            length as u64,
            format!("{field} exceeds {} characters", limits::MAX_NAME_CHARS),
        ));
    }
    // Control characters and bidirectional overrides are how a hostile name
    // smuggles a false line into a report or a terminal. Names are labels; they
    // never need them.
    if let Some(bad) = value
        .chars()
        .find(|c| c.is_control() || is_bidi_control(*c))
    {
        return Err(Error::validation(
            "validation.name_control_character",
            field,
            format!(
                "{field} contains the disallowed character U+{:04X}",
                u32::from(bad)
            ),
        ));
    }
    Ok(())
}

fn check_id(field: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 48 {
        return Err(Error::validation(
            "validation.id_length",
            field,
            format!("{field} must be 1 to 48 characters, got {}", value.len()),
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(Error::validation(
            "validation.id_charset",
            field,
            format!("{field} must contain only lowercase ASCII letters, digits, and hyphens"),
        ));
    }
    Ok(())
}

/// Unicode bidirectional formatting characters.
///
/// These reorder rendered text without changing the underlying bytes, which is
/// how a name like `report_txt\u{202E}vsc.exe` renders as something else entirely.
/// See `docs/SECURITY.md`.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn site() -> Site {
        Site {
            timezone: "America/New_York".to_string(),
            currency: "USD".to_string(),
        }
    }

    fn battery() -> BatterySpec {
        BatterySpec {
            nameplate_energy_kwh: 100.0,
            max_charge_kw: 50.0,
            max_discharge_kw: 50.0,
            soc_min_pct: 10.0,
            soc_max_pct: 100.0,
            reserve_soc_pct: 20.0,
            charge_efficiency_pct: 95.0,
            discharge_efficiency_pct: 95.0,
            self_discharge_pct_per_month: 2.0,
            temperature_derate: None,
        }
    }

    #[test]
    fn a_new_project_is_valid_and_has_a_no_storage_baseline() {
        let project = Project::new("p1", "Test site", site(), now()).unwrap();
        assert_eq!(project.schema_version, SCHEMA_VERSION);
        assert_eq!(project.scenarios.len(), 1);
        assert!(project.baseline().unwrap().battery.is_none());
        project.validate().unwrap();
    }

    #[test]
    fn a_project_round_trips_through_json_unchanged() {
        let project = Project::new("p1", "Test site", site(), now()).unwrap();
        let json = serde_json::to_string(&project).unwrap();
        let parsed = migrate(&json).unwrap();
        assert_eq!(project, parsed);
    }

    #[test]
    fn unknown_fields_are_rejected_rather_than_ignored() {
        let project = Project::new("p1", "Test site", site(), now()).unwrap();
        let mut value = serde_json::to_value(&project).unwrap();
        value["surprise"] = serde_json::json!("hello");
        let error = migrate(&value.to_string()).unwrap_err();
        assert_eq!(error.code(), "schema.invalid");
    }

    #[test]
    fn a_project_from_a_newer_schema_is_refused_by_name() {
        let error = migrate(r#"{"schema_version": 99}"#).unwrap_err();
        assert_eq!(error.code(), "schema.version_from_the_future");
        assert!(error.to_string().contains("Upgrade Tonans"));
    }

    #[test]
    fn a_file_that_is_not_a_project_is_refused_clearly() {
        let error = migrate("hello, world").unwrap_err();
        assert_eq!(error.code(), "schema.unreadable");
        let error = migrate(r#"{"name": "no version here"}"#).unwrap_err();
        assert_eq!(error.code(), "schema.unreadable");
    }

    #[test]
    fn an_oversized_project_is_refused_before_it_is_parsed() {
        let huge = "x".repeat(limits::MAX_PROJECT_BYTES + 1);
        let error = migrate(&huge).unwrap_err();
        assert_eq!(error.code(), "limit.project_bytes");
    }

    #[test]
    fn reserve_must_sit_inside_the_usable_envelope() {
        let mut spec = battery();
        spec.reserve_soc_pct = 5.0; // below soc_min_pct of 10
        let error = spec.validate("s").unwrap_err();
        assert_eq!(error.code(), "validation.reserve_outside_envelope");

        spec.reserve_soc_pct = 101.0;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.out_of_range"
        );
    }

    #[test]
    fn soc_envelope_must_be_ordered() {
        let mut spec = battery();
        spec.soc_min_pct = 90.0;
        spec.soc_max_pct = 20.0;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.soc_envelope"
        );
    }

    #[test]
    fn efficiencies_may_not_be_zero() {
        // A zero discharge efficiency would divide by zero in the engine. It is
        // rejected at the schema, so the engine never has to defend against it.
        let mut spec = battery();
        spec.discharge_efficiency_pct = 0.0;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.out_of_range"
        );
        spec.discharge_efficiency_pct = 101.0;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.out_of_range"
        );
    }

    #[test]
    fn non_finite_assumptions_are_rejected() {
        let mut spec = battery();
        spec.nameplate_energy_kwh = f64::NAN;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.non_finite"
        );
        spec.nameplate_energy_kwh = f64::INFINITY;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.non_finite"
        );
    }

    #[test]
    fn round_trip_efficiency_is_the_product_of_the_two_legs() {
        let spec = battery(); // 95% x 95%
        assert!((spec.round_trip_efficiency_pct() - 90.25).abs() < 1e-9);
    }

    #[test]
    fn a_battery_that_cannot_move_energy_is_rejected() {
        let mut spec = battery();
        spec.max_charge_kw = 0.0;
        spec.max_discharge_kw = 0.0;
        assert_eq!(
            spec.validate("s").unwrap_err().code(),
            "validation.battery_cannot_move_energy"
        );
    }

    #[test]
    fn duplicate_scenario_ids_are_rejected() {
        let mut project = Project::new("p1", "Test", site(), now()).unwrap();
        let mut clone = project.scenarios[0].clone();
        clone.name = "Copy".to_string();
        project.scenarios.push(clone);
        assert_eq!(
            project.validate().unwrap_err().code(),
            "validation.duplicate_scenario_id"
        );
    }

    #[test]
    fn a_dangling_baseline_reference_is_rejected() {
        let mut project = Project::new("p1", "Test", site(), now()).unwrap();
        project.baseline_scenario_id = "does-not-exist".to_string();
        assert_eq!(
            project.validate().unwrap_err().code(),
            "validation.unknown_baseline"
        );
    }

    #[test]
    fn ids_are_restricted_to_a_safe_charset() {
        assert!(check_id("f", "baseline-2").is_ok());
        assert_eq!(
            check_id("f", "Baseline").unwrap_err().code(),
            "validation.id_charset"
        );
        assert_eq!(
            check_id("f", "a b").unwrap_err().code(),
            "validation.id_charset"
        );
        assert_eq!(
            check_id("f", "../etc").unwrap_err().code(),
            "validation.id_charset"
        );
        assert_eq!(
            check_id("f", "").unwrap_err().code(),
            "validation.id_length"
        );
    }

    #[test]
    fn names_reject_control_and_bidi_characters() {
        assert!(check_name("f", "Warehouse 3 — East").is_ok());
        assert_eq!(
            check_name("f", "bad\u{202E}name").unwrap_err().code(),
            "validation.name_control_character"
        );
        assert_eq!(
            check_name("f", "line\nbreak").unwrap_err().code(),
            "validation.name_control_character"
        );
        assert_eq!(
            check_name("f", "nul\0byte").unwrap_err().code(),
            "validation.name_control_character"
        );
    }

    #[test]
    fn a_scenario_without_a_battery_may_not_have_a_battery_inverter() {
        let mut scenario = Scenario::no_storage_baseline();
        scenario.inverter.battery_power_kw = 50.0;
        assert_eq!(
            scenario.validate().unwrap_err().code(),
            "validation.inverter_without_battery"
        );
    }

    #[test]
    fn battery_dispatch_policies_require_a_battery() {
        let policies = [
            DispatchPolicy::PeakLimit {
                import_limit_kw: 10.0,
                allow_grid_charge: false,
                grid_charge_target_soc_pct: 50.0,
            },
            DispatchPolicy::ReserveFirst {
                reserve_target_soc_pct: 50.0,
                allow_grid_charge: false,
            },
            DispatchPolicy::ManualSchedule {
                entries: Vec::new(),
            },
        ];
        for dispatch in policies {
            let mut scenario = Scenario::no_storage_baseline();
            scenario.dispatch = dispatch;
            assert_eq!(
                scenario.validate().unwrap_err().code(),
                "validation.dispatch_without_battery"
            );
        }
        Scenario::no_storage_baseline().validate().unwrap();
    }

    #[test]
    fn a_soc_target_outside_the_battery_envelope_is_rejected() {
        let mut scenario = Scenario::no_storage_baseline();
        scenario.battery = Some(battery()); // envelope is 10..100
        scenario.inverter.battery_power_kw = 50.0;
        scenario.initial_soc_pct = 50.0;
        scenario.dispatch = DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct: 5.0,
            allow_grid_charge: true,
        };
        assert_eq!(
            scenario.validate().unwrap_err().code(),
            "validation.soc_target_outside_envelope"
        );
    }

    #[test]
    fn temperature_derate_interpolates_linearly_and_clamps_outside_its_table() {
        let derate = TemperatureDerate {
            points: vec![
                TemperatureDeratePoint {
                    temperature_c: 0.0,
                    power_limit_pct: 50.0,
                    usable_capacity_pct: 80.0,
                },
                TemperatureDeratePoint {
                    temperature_c: 20.0,
                    power_limit_pct: 100.0,
                    usable_capacity_pct: 100.0,
                },
                TemperatureDeratePoint {
                    temperature_c: 40.0,
                    power_limit_pct: 70.0,
                    usable_capacity_pct: 95.0,
                },
            ],
        };
        derate.validate("s").unwrap();

        // Clamped below the table.
        let (power, capacity) = derate.factors_at(-30.0);
        assert!((power - 0.50).abs() < 1e-12 && (capacity - 0.80).abs() < 1e-12);
        // Exactly on a point.
        let (power, _) = derate.factors_at(20.0);
        assert!((power - 1.0).abs() < 1e-12);
        // Halfway between 0 and 20: power 50 -> 100 gives 75.
        let (power, capacity) = derate.factors_at(10.0);
        assert!((power - 0.75).abs() < 1e-12, "{power}");
        assert!((capacity - 0.90).abs() < 1e-12, "{capacity}");
        // Clamped above the table.
        let (power, _) = derate.factors_at(100.0);
        assert!((power - 0.70).abs() < 1e-12);
    }

    #[test]
    fn a_derate_table_must_ascend_and_have_at_least_two_points() {
        let one = TemperatureDerate {
            points: vec![TemperatureDeratePoint {
                temperature_c: 0.0,
                power_limit_pct: 100.0,
                usable_capacity_pct: 100.0,
            }],
        };
        assert_eq!(
            one.validate("s").unwrap_err().code(),
            "validation.derate_table_too_small"
        );

        let unsorted = TemperatureDerate {
            points: vec![
                TemperatureDeratePoint {
                    temperature_c: 20.0,
                    power_limit_pct: 100.0,
                    usable_capacity_pct: 100.0,
                },
                TemperatureDeratePoint {
                    temperature_c: 0.0,
                    power_limit_pct: 50.0,
                    usable_capacity_pct: 80.0,
                },
            ],
        };
        assert_eq!(
            unsorted.validate("s").unwrap_err().code(),
            "validation.derate_table_unsorted"
        );
    }

    #[test]
    fn only_usd_is_accepted_because_only_usd_can_be_labelled() {
        let site = Site {
            timezone: "UTC".into(),
            currency: "EUR".into(),
        };
        assert_eq!(
            site.validate().unwrap_err().code(),
            "validation.unsupported_currency"
        );
    }

    #[test]
    fn advisories_flag_a_baseline_that_already_has_storage() {
        let mut project = Project::new("p1", "Test", site(), now()).unwrap();
        project.scenarios[0].battery = Some(battery());
        project.scenarios[0].inverter.battery_power_kw = 50.0;
        project.scenarios[0].initial_soc_pct = 50.0;
        project.validate().unwrap();

        let advisories = project.advisories();
        let codes: Vec<&str> = advisories.iter().map(|w| w.code.as_str()).collect();
        assert!(codes.contains(&"project.baseline_has_storage"), "{codes:?}");
    }
}
