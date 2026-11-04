//! Bounded sensitivity studies.
//!
//! One or two axes, a handful of values on each, and a full engine run per cell.
//! There is no interpolation between cells and no surrogate model: every number in
//! the matrix was simulated. That is why the grid is small, and why it is bounded
//! hard: [`limits::MAX_SENSITIVITY_CELLS`] engine runs is the ceiling.
//!
//! Every perturbed scenario is validated *before* a single cell is run. A sweep
//! that would produce an invalid scenario at value 7 of 10 fails immediately and
//! names the value, rather than running six cells and then presenting a matrix
//! with a hole in it.
//!
//! Cells are emitted in row-major order — first axis outer, second axis inner —
//! and that order is stable across runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::economics;
use crate::engine::{self, EngineOptions, ResolvedInput};
use crate::error::{Error, Result, Warning, WarningLedger};
use crate::limits;
use crate::schema::{Project, Scenario, TariffSpec};
use crate::timegrid::IntervalGrid;
use crate::version::MODEL_VERSION;

/// An assumption a sensitivity study can vary.
///
/// A closed list, on purpose. An open "path into the schema" would let a study
/// perturb something the engine was never designed to have perturbed, and the
/// resulting matrix would look just as authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityParam {
    /// Battery nameplate energy, kWh.
    BatteryEnergyKwh,
    /// Battery charge and discharge power, kW. Moves the inverter rating with it,
    /// since a battery cannot use power its converter cannot pass.
    BatteryPowerKw,
    /// Outage reserve floor, percent.
    ReserveSocPct,
    /// Initial usable capacity, percent. The aged-battery axis.
    InitialCapacityPct,
    /// Multiplier on the import price, whatever its source. Applied to this scenario
    /// and to the baseline, because a price is not something one scenario can have alone.
    ImportPriceMultiplier,
    /// Demand price, USD per kW-month. Applied to every demand period the tariff
    /// defines, in this scenario and in the baseline.
    DemandPricePerKwMonthUsd,
    /// Calendar fade, percent per year.
    CalendarFadePctPerYear,
    /// Installed cost, USD.
    CapexUsd,
    /// Discount rate, percent per year.
    DiscountRatePctPerYear,
}

impl SensitivityParam {
    /// The axis label, with its unit.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            SensitivityParam::BatteryEnergyKwh => "Battery energy (kWh)",
            SensitivityParam::BatteryPowerKw => "Battery power (kW)",
            SensitivityParam::ReserveSocPct => "Reserve floor (%)",
            SensitivityParam::InitialCapacityPct => "Initial capacity (%)",
            SensitivityParam::ImportPriceMultiplier => "Import price (x)",
            SensitivityParam::DemandPricePerKwMonthUsd => "Demand price (USD/kW-month)",
            SensitivityParam::CalendarFadePctPerYear => "Calendar fade (%/year)",
            SensitivityParam::CapexUsd => "CAPEX (USD)",
            SensitivityParam::DiscountRatePctPerYear => "Discount rate (%/year)",
        }
    }

    /// Whether this parameter requires the scenario to have a battery.
    #[must_use]
    pub const fn needs_battery(self) -> bool {
        matches!(
            self,
            SensitivityParam::BatteryEnergyKwh
                | SensitivityParam::BatteryPowerKw
                | SensitivityParam::ReserveSocPct
                | SensitivityParam::InitialCapacityPct
        )
    }

    /// Whether this parameter is a price the site pays whichever scenario it picks.
    ///
    /// Sweeping one of these in the candidate alone would compare a candidate at one
    /// price against a baseline at another, and the result would mostly measure that gap:
    /// raising the price would look like it makes storage worth less.
    #[must_use]
    pub const fn moves_baseline(self) -> bool {
        matches!(
            self,
            SensitivityParam::ImportPriceMultiplier | SensitivityParam::DemandPricePerKwMonthUsd
        )
    }

    /// Set this parameter on the baseline to match a candidate it has just been applied to.
    ///
    /// A baseline with no demand charge takes the candidate's periods, so the two are at
    /// the same price rather than one of them paying nothing.
    fn apply_to_baseline(self, baseline: &mut Scenario, candidate: &Scenario) {
        match self {
            SensitivityParam::ImportPriceMultiplier => {
                baseline.tariff.import_price_multiplier = candidate.tariff.import_price_multiplier;
            }
            SensitivityParam::DemandPricePerKwMonthUsd => {
                baseline.tariff.demand_prices_per_kw_month_usd =
                    candidate.tariff.demand_prices_per_kw_month_usd.clone();
            }
            _ => {}
        }
    }

    /// Apply this parameter to a scenario.
    fn apply(self, scenario: &mut Scenario, value: f64) -> Result<()> {
        match self {
            SensitivityParam::BatteryEnergyKwh => {
                let battery = require_battery(scenario, self)?;
                battery.nameplate_energy_kwh = value;
            }
            SensitivityParam::BatteryPowerKw => {
                let battery = require_battery(scenario, self)?;
                battery.max_charge_kw = value;
                battery.max_discharge_kw = value;
                // The converter must be able to pass what the battery can move, or
                // the axis would silently stop having any effect past the inverter
                // rating and the matrix would flatten for no visible reason.
                scenario.inverter.battery_power_kw = value;
            }
            SensitivityParam::ReserveSocPct => {
                let battery = require_battery(scenario, self)?;
                battery.reserve_soc_pct = value;
            }
            SensitivityParam::InitialCapacityPct => {
                require_battery(scenario, self)?;
                scenario.initial_capacity_pct = value;
            }
            SensitivityParam::ImportPriceMultiplier => {
                scenario.tariff.import_price_multiplier = value;
            }
            SensitivityParam::DemandPricePerKwMonthUsd => {
                if scenario.tariff.demand_prices_per_kw_month_usd.is_empty() {
                    return Err(Error::validation(
                        "validation.sensitivity_no_demand_charge",
                        "sensitivity.axis",
                        "this scenario has no demand charge, so there is no demand price to vary. \
                         Define one on the scenario's tariff first.",
                    ));
                }
                for price in scenario.tariff.demand_prices_per_kw_month_usd.values_mut() {
                    *price = value;
                }
            }
            SensitivityParam::CalendarFadePctPerYear => {
                scenario.degradation.calendar_fade_pct_per_year = value;
            }
            SensitivityParam::CapexUsd => {
                scenario.economics.capex_usd = value;
            }
            SensitivityParam::DiscountRatePctPerYear => {
                scenario.economics.discount_rate_pct_per_year = value;
            }
        }
        Ok(())
    }
}

fn require_battery(
    scenario: &mut Scenario,
    param: SensitivityParam,
) -> Result<&mut crate::schema::BatterySpec> {
    scenario.battery.as_mut().ok_or_else(|| {
        Error::validation(
            "validation.sensitivity_needs_battery",
            "sensitivity.axis",
            format!(
                "'{}' can only be varied on a scenario that has a battery.",
                param.label()
            ),
        )
    })
}

/// One axis of a study.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensitivityAxis {
    /// What to vary.
    pub param: SensitivityParam,
    /// The values to try, in the order they will appear.
    pub values: Vec<f64>,
}

/// A study to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensitivityRequest {
    /// The scenario to perturb. Its unperturbed form is *not* included as a cell;
    /// if you want it, put its current value on an axis.
    pub scenario_id: String,
    /// One or two axes.
    pub axes: Vec<SensitivityAxis>,
    /// Move the installed cost with the battery-energy axis, at this price per kWh of
    /// nameplate. Without it every cell keeps the scenario's installed cost, so a larger
    /// battery costs the same as a smaller one and the warning ledger says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capex_per_kwh_usd: Option<f64>,
}

/// What each cell reports.
///
/// A deliberately small set. A sensitivity matrix is read at a glance, and thirty
/// columns of it is a table nobody checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensitivityMetrics {
    /// Installed cost in this cell, so that a reader can see whether it moved.
    pub installed_cost_usd: f64,
    /// Grid cost over one year.
    pub annual_grid_cost_usd: f64,
    /// Saving against the project's baseline, over one year.
    pub annual_savings_usd: f64,
    /// Net present value over the scenario's horizon.
    pub npv_usd: f64,
    /// Years to repay the capital, if it is repaid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub simple_payback_years: Option<f64>,
    /// Highest mean import power.
    pub peak_import_kw: f64,
    /// Energy imported over the simulated period.
    pub grid_import_kwh: f64,
    /// Critical load nothing could serve.
    pub unmet_critical_kwh: f64,
    /// Share of load met without the grid.
    pub self_sufficiency_pct: f64,
    /// Lowest state of charge reached.
    pub min_soc_pct: f64,
    /// Battery throughput in full usable cycles.
    pub equivalent_full_cycles: f64,
}

/// One cell of the matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensitivityCell {
    /// The value taken on each axis, in axis order.
    pub axis_values: Vec<f64>,
    /// What that produced.
    pub metrics: SensitivityMetrics,
}

/// A completed study.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensitivityResult {
    /// The version of the model this was produced by. A study's numbers depend on it, so
    /// its identity must change with it.
    pub model_version: String,
    /// The scenario that was perturbed.
    pub scenario_id: String,
    /// Its name.
    pub scenario_name: String,
    /// The baseline savings are measured against. Price axes apply to it as well; see
    /// [`SensitivityParam::moves_baseline`].
    pub baseline_scenario_id: String,
    /// The axes, echoed so a chart can label itself from the result alone.
    pub axes: Vec<SensitivityAxis>,
    /// The price the installed cost moved at, if the study asked for that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capex_per_kwh_usd: Option<f64>,
    /// Cells, row-major: first axis outer, second axis inner.
    pub cells: Vec<SensitivityCell>,
    /// Everything the reader must know.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl SensitivityResult {
    /// The cell at `(i, j)`, where `j` is ignored for a one-axis study.
    #[must_use]
    pub fn cell(&self, i: usize, j: usize) -> Option<&SensitivityCell> {
        let inner = self.axes.get(1).map_or(1, |axis| axis.values.len());
        self.cells.get(i * inner + j)
    }
}

/// Run a sensitivity study.
pub fn run(project: &Project, request: &SensitivityRequest) -> Result<SensitivityResult> {
    let grid = project.validate_runnable()?;

    let scenario = project.scenario(&request.scenario_id).ok_or_else(|| {
        Error::validation(
            "validation.unknown_scenario",
            "sensitivity.scenario_id",
            format!("no scenario has the id '{}'", request.scenario_id),
        )
    })?;

    validate_axes(&request.axes, scenario)?;
    validate_capex_scaling(request)?;

    let baseline_spec = project.baseline().ok_or_else(|| {
        Error::validation(
            "validation.unknown_baseline",
            "project.baseline_scenario_id",
            "the baseline scenario does not exist",
        )
    })?;

    // Build and validate every perturbed scenario before running any of them.
    let combinations = combinations(&request.axes);
    let mut prepared = Vec::with_capacity(combinations.len());
    for values in &combinations {
        let mut candidate = scenario.clone();
        for (axis, value) in request.axes.iter().zip(values) {
            axis.param.apply(&mut candidate, *value)?;
        }
        if let (Some(rate), Some(battery)) = (request.capex_per_kwh_usd, &candidate.battery) {
            candidate.economics.capex_usd = rate * battery.nameplate_energy_kwh;
        }

        // Every cell's saving is measured against the baseline at the same prices. Size,
        // cost, and rate axes describe the candidate, so the baseline stays as it is.
        let mut baseline = baseline_spec.clone();
        for axis in &request.axes {
            axis.param.apply_to_baseline(&mut baseline, &candidate);
        }

        for checked in [&candidate, &baseline] {
            checked.validate().map_err(|e| {
                Error::validation(
                    "validation.sensitivity_cell_invalid",
                    "sensitivity.axes",
                    format!(
                        "the combination {} produces an invalid scenario: {e}",
                        describe(&request.axes, values)
                    ),
                )
            })?;
            if let Some(dataset) = &project.dataset {
                checked.validate_against(dataset, &grid).map_err(|e| {
                    Error::validation(
                        "validation.sensitivity_cell_invalid",
                        "sensitivity.axes",
                        format!(
                            "the combination {} produces an invalid scenario: {e}",
                            describe(&request.axes, values)
                        ),
                    )
                })?;
            }
        }
        prepared.push((candidate, baseline));
    }

    let mut ledger = WarningLedger::new();
    if let Some(warning) = installed_cost_warning(request, scenario) {
        ledger.push(warning);
    }
    if request.axes.iter().any(|axis| axis.param.moves_baseline()) {
        ledger.push(Warning::info(
            "sensitivity.prices_move_baseline",
            format!(
                "import and demand price axes apply to '{}' and to the baseline '{}' in every \
                 cell, because a site pays the same prices whichever it picks. Each saving \
                 compares the two at the same price.",
                scenario.name, baseline_spec.name
            ),
        ));
    }

    // A baseline differs between cells only in its prices, so each distinct set of prices
    // is simulated once. A grid of 256 size cells needs one baseline run, not 256.
    let mut baselines: BTreeMap<Vec<u64>, engine::EngineResult> = BTreeMap::new();
    let mut cells = Vec::with_capacity(prepared.len());

    for ((candidate, baseline), values) in prepared.iter().zip(&combinations) {
        let prices: Vec<u64> = request
            .axes
            .iter()
            .zip(values)
            .filter(|(axis, _)| axis.param.moves_baseline())
            .map(|(_, value)| value.to_bits())
            .collect();
        if !baselines.contains_key(&prices) {
            let input = ResolvedInput::resolve(project, baseline, &grid)?;
            let result = engine::run(
                baseline,
                &input.as_input(&grid),
                EngineOptions {
                    retain_timeline: false,
                },
            )?;
            baselines.insert(prices.clone(), result);
        }
        let baseline_result = &baselines[&prices];
        let cell = evaluate_cell(project, candidate, &grid, baseline_result, &mut ledger)?;
        cells.push(SensitivityCell {
            axis_values: values.clone(),
            metrics: cell,
        });
    }

    ledger.push(Warning::info(
        "sensitivity.per_cell_simulation",
        format!(
            "each of these {} cells is a full simulation of the data set, not an interpolation. \
             Cells vary only the axes shown; every other assumption is the scenario's.",
            cells.len()
        ),
    ));

    Ok(SensitivityResult {
        model_version: MODEL_VERSION.to_string(),
        scenario_id: scenario.id.clone(),
        scenario_name: scenario.name.clone(),
        baseline_scenario_id: baseline_spec.id.clone(),
        axes: request.axes.clone(),
        capex_per_kwh_usd: request.capex_per_kwh_usd,
        cells,
        warnings: ledger.into_sorted(),
    })
}

fn evaluate_cell(
    project: &Project,
    candidate: &Scenario,
    grid: &IntervalGrid,
    baseline: &engine::EngineResult,
    ledger: &mut WarningLedger,
) -> Result<SensitivityMetrics> {
    let input = ResolvedInput::resolve(project, candidate, grid)?;
    let result = engine::run(
        candidate,
        &input.as_input(grid),
        EngineOptions {
            retain_timeline: false,
        },
    )?;
    let economics = economics::evaluate(
        candidate,
        &result.energy,
        &result.cost,
        &baseline.cost,
        // A cell's own warnings are folded into the study's ledger, deduplicated
        // by code, so 256 cells cannot produce 256 copies of the same sentence.
        ledger,
    )?;

    Ok(SensitivityMetrics {
        installed_cost_usd: candidate.economics.capex_usd,
        annual_grid_cost_usd: economics.scenario_annual_grid_cost_usd,
        annual_savings_usd: economics.year_one_gross_savings_usd,
        npv_usd: economics.npv_usd,
        simple_payback_years: economics.simple_payback_years,
        peak_import_kw: result.energy.peak_import_kw,
        grid_import_kwh: result.energy.grid_import_kwh,
        unmet_critical_kwh: result.energy.unmet_critical_kwh,
        self_sufficiency_pct: result.energy.self_sufficiency_pct,
        min_soc_pct: result.energy.min_soc_pct,
        equivalent_full_cycles: result.energy.equivalent_full_cycles,
    })
}

fn validate_axes(axes: &[SensitivityAxis], scenario: &Scenario) -> Result<()> {
    if axes.is_empty() {
        return Err(Error::validation(
            "validation.sensitivity_no_axes",
            "sensitivity.axes",
            "a sensitivity study needs at least one axis",
        ));
    }
    if axes.len() > limits::MAX_SENSITIVITY_AXES {
        return Err(Error::limit(
            "limit.sensitivity_axes",
            limits::MAX_SENSITIVITY_AXES as u64,
            axes.len() as u64,
            format!(
                "{} axes; Tonans charts up to {}. A third axis is a table nobody reads.",
                axes.len(),
                limits::MAX_SENSITIVITY_AXES
            ),
        ));
    }
    if axes.len() == 2 && axes[0].param == axes[1].param {
        return Err(Error::validation(
            "validation.sensitivity_duplicate_axis",
            "sensitivity.axes",
            "both axes vary the same parameter",
        ));
    }

    let mut cells = 1usize;
    for axis in axes {
        if axis.values.is_empty() {
            return Err(Error::validation(
                "validation.sensitivity_empty_axis",
                "sensitivity.axes",
                format!("the '{}' axis has no values", axis.param.label()),
            ));
        }
        if axis.values.len() > limits::MAX_SENSITIVITY_VALUES_PER_AXIS {
            return Err(Error::limit(
                "limit.sensitivity_values",
                limits::MAX_SENSITIVITY_VALUES_PER_AXIS as u64,
                axis.values.len() as u64,
                format!(
                    "the '{}' axis has {} values; the limit is {}",
                    axis.param.label(),
                    axis.values.len(),
                    limits::MAX_SENSITIVITY_VALUES_PER_AXIS
                ),
            ));
        }
        for (index, value) in axis.values.iter().enumerate() {
            if !value.is_finite() {
                return Err(Error::validation(
                    "validation.non_finite",
                    "sensitivity.axes",
                    format!(
                        "value {index} on the '{}' axis is not a finite number",
                        axis.param.label()
                    ),
                ));
            }
            if axis.values[..index]
                .iter()
                .any(|v| (v - value).abs() < f64::EPSILON)
            {
                return Err(Error::validation(
                    "validation.sensitivity_duplicate_value",
                    "sensitivity.axes",
                    format!(
                        "the value {value} appears twice on the '{}' axis",
                        axis.param.label()
                    ),
                ));
            }
        }
        if axis.param.needs_battery() && scenario.battery.is_none() {
            return Err(Error::validation(
                "validation.sensitivity_needs_battery",
                "sensitivity.axes",
                format!(
                    "'{}' can only be varied on a scenario that has a battery, and '{}' does not.",
                    axis.param.label(),
                    scenario.name
                ),
            ));
        }
        cells *= axis.values.len();
    }

    if cells > limits::MAX_SENSITIVITY_CELLS {
        return Err(Error::limit(
            "limit.sensitivity_cells",
            limits::MAX_SENSITIVITY_CELLS as u64,
            cells as u64,
            format!(
                "{cells} cells; the engine runs at most {}. Every cell is a full simulation.",
                limits::MAX_SENSITIVITY_CELLS
            ),
        ));
    }
    Ok(())
}

fn validate_capex_scaling(request: &SensitivityRequest) -> Result<()> {
    let Some(rate) = request.capex_per_kwh_usd else {
        return Ok(());
    };
    let refuse = |code: &'static str, message: &str| {
        Err(Error::validation(
            code,
            "sensitivity.capex_per_kwh_usd",
            message,
        ))
    };
    if !rate.is_finite() || rate < 0.0 {
        return refuse(
            "validation.sensitivity_bad_capex_rate",
            "the installed cost per kWh must be a finite number of at least 0",
        );
    }
    if !has_axis(request, SensitivityParam::BatteryEnergyKwh) {
        return refuse(
            "validation.sensitivity_capex_needs_energy_axis",
            "installed cost can only move with battery energy when battery energy is an axis",
        );
    }
    if has_axis(request, SensitivityParam::CapexUsd) {
        return refuse(
            "validation.sensitivity_capex_twice",
            "installed cost is already an axis, so it cannot also move with battery energy",
        );
    }
    Ok(())
}

fn has_axis(request: &SensitivityRequest, param: SensitivityParam) -> bool {
    request.axes.iter().any(|axis| axis.param == param)
}

/// What the reader must know about the installed cost across the cells.
fn installed_cost_warning(request: &SensitivityRequest, scenario: &Scenario) -> Option<Warning> {
    let power_axis = has_axis(request, SensitivityParam::BatteryPowerKw);
    if let Some(rate) = request.capex_per_kwh_usd {
        let mut message = format!(
            "installed cost moves with battery energy in this study, at {rate} USD per kWh of \
             nameplate."
        );
        // Each of these makes one direction look cheaper than it is, so it is a caution
        // rather than a note.
        if power_axis {
            message.push_str(
                " Battery power does not move it, so more power costs nothing here and net \
                 present value favours it.",
            );
        }
        let replacement = scenario.economics.replacement.is_some();
        if replacement {
            message.push_str(
                " The replacement cost does not move with it either, so larger batteries are \
                 charged the same replacement as smaller ones.",
            );
        }
        return Some(if power_axis || replacement {
            Warning::caution("sensitivity.installed_cost_scaled", message)
        } else {
            Warning::info("sensitivity.installed_cost_scaled", message)
        });
    }
    let sizes_vary = has_axis(request, SensitivityParam::BatteryEnergyKwh) || power_axis;
    (sizes_vary && !has_axis(request, SensitivityParam::CapexUsd)).then(|| {
        Warning::caution(
            "sensitivity.installed_cost_held",
            format!(
                "every cell keeps the scenario's installed cost of {} USD whatever its battery \
                 size, so net present value and payback favour larger batteries. Treat them as \
                 upper bounds, or vary installed cost as well.",
                scenario.economics.capex_usd
            ),
        )
    })
}

/// Every combination of axis values, row-major.
fn combinations(axes: &[SensitivityAxis]) -> Vec<Vec<f64>> {
    match axes {
        [] => Vec::new(),
        [first] => first.values.iter().map(|v| vec![*v]).collect(),
        [first, second, ..] => {
            let mut out = Vec::with_capacity(first.values.len() * second.values.len());
            for a in &first.values {
                for b in &second.values {
                    out.push(vec![*a, *b]);
                }
            }
            out
        }
    }
}

fn describe(axes: &[SensitivityAxis], values: &[f64]) -> String {
    axes.iter()
        .zip(values)
        .map(|(axis, value)| format!("{} = {value}", axis.param.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The demand-period label a study writes back when a tariff has none.
///
/// Exposed so that callers building a request can tell whether a demand axis is
/// even meaningful for a given scenario.
#[must_use]
pub fn default_demand_period() -> &'static str {
    TariffSpec::DEFAULT_DEMAND_PERIOD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
    use crate::run;
    use crate::schema::{BatterySpec, DispatchPolicy, GapPolicy, InverterSpec, SeriesKind, Site};
    use chrono::{DateTime, Utc};

    /// Two days of hourly load, a no-storage baseline, and a battery scenario.
    fn study_project() -> Project {
        let now: DateTime<Utc> = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        let mut csv = String::from("t,load\n");
        for hour in 0..48 {
            let day = 1 + hour / 24;
            let load = if hour % 24 >= 17 && hour % 24 < 21 {
                80
            } else {
                20
            };
            csv.push_str(&format!(
                "2024-01-{day:02}T{:02}:00:00Z,{load}\n",
                hour % 24
            ));
        }
        let mut series = BTreeMap::new();
        series.insert(
            SeriesKind::Load,
            MappedColumn {
                header: "load".to_string(),
                unit: ImportUnit::Kilowatt,
            },
        );
        let request = ImportRequest {
            filename: "t.csv".to_string(),
            timezone: "UTC".to_string(),
            interval_minutes: 60,
            gap_policy: GapPolicy::Reject,
            mapping: ColumnMapping {
                timestamp: "t".to_string(),
                series,
                demand_period: None,
                ignore_columns: Vec::new(),
            },
        };
        let mut project = Project::new("p", "P", Site::default(), now).unwrap();
        project.dataset = Some(
            csvimport::import(csv.as_bytes(), &request, now)
                .unwrap()
                .dataset,
        );

        let mut candidate = Scenario::no_storage_baseline();
        candidate.id = "candidate".to_string();
        candidate.name = "Candidate".to_string();
        candidate.battery = Some(BatterySpec {
            nameplate_energy_kwh: 100.0,
            max_charge_kw: 50.0,
            max_discharge_kw: 50.0,
            soc_min_pct: 10.0,
            soc_max_pct: 100.0,
            reserve_soc_pct: 10.0,
            charge_efficiency_pct: 95.0,
            discharge_efficiency_pct: 95.0,
            self_discharge_pct_per_month: 0.0,
            temperature_derate: None,
        });
        candidate.inverter = InverterSpec {
            battery_power_kw: 50.0,
            site_export_limit_kw: None,
        };
        candidate.dispatch = DispatchPolicy::PeakLimit {
            import_limit_kw: 60.0,
            allow_grid_charge: true,
            grid_charge_target_soc_pct: 90.0,
        };
        candidate.initial_soc_pct = 50.0;
        candidate.economics.capex_usd = 10_000.0;
        project.scenarios.push(candidate);
        project
    }

    fn energy_axis(values: Vec<f64>) -> SensitivityAxis {
        SensitivityAxis {
            param: SensitivityParam::BatteryEnergyKwh,
            values,
        }
    }

    fn request(axes: Vec<SensitivityAxis>, rate: Option<f64>) -> SensitivityRequest {
        SensitivityRequest {
            scenario_id: "candidate".to_string(),
            axes,
            capex_per_kwh_usd: rate,
        }
    }

    #[test]
    fn scaled_cost_equals_stating_that_cost_on_the_cell_by_hand() {
        let project = study_project();
        let scaled = run(
            &project,
            &request(vec![energy_axis(vec![100.0, 200.0])], Some(500.0)),
        )
        .unwrap();

        // The same two cells with the cost written out: 500 x 100 and 500 x 200.
        for (index, (energy, cost)) in [(100.0, 50_000.0), (200.0, 100_000.0)].iter().enumerate() {
            let stated = run(
                &project,
                &request(
                    vec![
                        energy_axis(vec![*energy]),
                        SensitivityAxis {
                            param: SensitivityParam::CapexUsd,
                            values: vec![*cost],
                        },
                    ],
                    None,
                ),
            )
            .unwrap();
            assert_eq!(
                scaled.cells[index].metrics, stated.cells[0].metrics,
                "cell at {energy} kWh"
            );
        }
        assert_eq!(scaled.capex_per_kwh_usd, Some(500.0));
    }

    #[test]
    fn without_scaling_the_numbers_do_not_move_and_the_ledger_says_cost_is_held() {
        let project = study_project();
        let held = run(&project, &request(vec![energy_axis(vec![100.0])], None)).unwrap();
        let stated = run(
            &project,
            &request(
                vec![
                    energy_axis(vec![100.0]),
                    SensitivityAxis {
                        param: SensitivityParam::CapexUsd,
                        values: vec![10_000.0],
                    },
                ],
                None,
            ),
        )
        .unwrap();
        assert_eq!(held.cells[0].metrics, stated.cells[0].metrics);
        assert_eq!(held.capex_per_kwh_usd, None);

        let code = |study: &SensitivityResult, code: &str| {
            study.warnings.iter().any(|warning| warning.code == code)
        };
        assert!(code(&held, "sensitivity.installed_cost_held"));
        // With cost on an axis the reader can see it moving, so there is nothing to flag.
        assert!(!code(&stated, "sensitivity.installed_cost_held"));
        let scaled = run(
            &project,
            &request(vec![energy_axis(vec![100.0])], Some(500.0)),
        )
        .unwrap();
        assert!(code(&scaled, "sensitivity.installed_cost_scaled"));
        assert!(!code(&scaled, "sensitivity.installed_cost_held"));
    }

    #[test]
    fn scaling_is_refused_where_it_could_not_mean_anything() {
        let project = study_project();
        let refusal = |axes, rate| run(&project, &request(axes, Some(rate))).unwrap_err();

        let capex = SensitivityAxis {
            param: SensitivityParam::CapexUsd,
            values: vec![1.0],
        };
        assert_eq!(
            refusal(vec![capex.clone()], 500.0).code(),
            "validation.sensitivity_capex_needs_energy_axis"
        );
        assert_eq!(
            refusal(vec![energy_axis(vec![100.0]), capex], 500.0).code(),
            "validation.sensitivity_capex_twice"
        );
        assert_eq!(
            refusal(vec![energy_axis(vec![100.0])], -1.0).code(),
            "validation.sensitivity_bad_capex_rate"
        );
        assert_eq!(
            refusal(vec![energy_axis(vec![100.0])], f64::NAN).code(),
            "validation.sensitivity_bad_capex_rate"
        );
    }

    #[test]
    fn a_request_written_before_the_rate_existed_still_reads_and_serializes_the_same() {
        let request: SensitivityRequest = serde_json::from_str(
            r#"{"scenario_id":"s","axes":[{"param":"capex_usd","values":[1]}]}"#,
        )
        .unwrap();
        assert_eq!(request.capex_per_kwh_usd, None);
        // The request is unchanged. The study it produces is not, and that is why
        // MODEL_VERSION moved: see `every_result_names_the_model_that_produced_it`.
        assert!(!serde_json::to_string(&request)
            .unwrap()
            .contains("capex_per_kwh_usd"));
    }

    #[test]
    fn every_result_names_the_model_that_produced_it() {
        let project = study_project();
        let study = run(&project, &request(vec![energy_axis(vec![100.0])], None)).unwrap();
        assert_eq!(study.model_version, MODEL_VERSION);
    }

    #[test]
    fn a_grid_of_sizes_measures_every_cell_against_one_unchanged_baseline() {
        let project = study_project();
        let study = run(
            &project,
            &request(vec![energy_axis(vec![50.0, 100.0, 200.0])], Some(500.0)),
        )
        .unwrap();
        let baseline_annual = run::run_project(&project, DateTime::from_timestamp(0, 0).unwrap())
            .unwrap()
            .baseline()
            .unwrap()
            .economics
            .scenario_annual_grid_cost_usd;
        for cell in &study.cells {
            let m = &cell.metrics;
            assert!(
                (m.annual_savings_usd - (baseline_annual - m.annual_grid_cost_usd)).abs() < 1e-6,
                "{cell:?}"
            );
        }
        assert!(!study
            .warnings
            .iter()
            .any(|w| w.code == "sensitivity.prices_move_baseline"));
    }

    #[test]
    fn a_price_axis_moves_the_baseline_and_says_so() {
        let mut project = study_project();
        project.scenarios[1].tariff.demand_prices_per_kw_month_usd =
            BTreeMap::from([("all".to_string(), 10.0)]);
        // The baseline has no demand charge of its own, and takes the candidate's periods.
        assert!(project.scenarios[0]
            .tariff
            .demand_prices_per_kw_month_usd
            .is_empty());

        for (param, values) in [
            (SensitivityParam::ImportPriceMultiplier, vec![0.5, 1.5]),
            (SensitivityParam::DemandPricePerKwMonthUsd, vec![5.0, 20.0]),
        ] {
            let study = run(
                &project,
                &request(
                    vec![SensitivityAxis {
                        param,
                        values: values.clone(),
                    }],
                    None,
                ),
            )
            .unwrap();
            assert!(study
                .warnings
                .iter()
                .any(|w| w.code == "sensitivity.prices_move_baseline"));

            for (index, value) in values.iter().enumerate() {
                let mut by_hand = project.clone();
                for scenario in &mut by_hand.scenarios {
                    param
                        .apply(scenario, *value)
                        .or_else(|_| {
                            scenario.tariff.demand_prices_per_kw_month_usd =
                                BTreeMap::from([("all".to_string(), *value)]);
                            Ok::<(), Error>(())
                        })
                        .unwrap();
                }
                let result =
                    run::run_project(&by_hand, DateTime::from_timestamp(0, 0).unwrap()).unwrap();
                let candidate = result.scenario("candidate").unwrap();
                assert_eq!(
                    study.cells[index].metrics.npv_usd, candidate.economics.npv_usd,
                    "{param:?} at {value}"
                );
                assert_eq!(
                    study.cells[index].metrics.annual_savings_usd,
                    candidate.economics.year_one_gross_savings_usd,
                );
            }
        }
    }

    #[test]
    fn each_cell_reports_the_cost_it_used() {
        let project = study_project();
        let scaled = run(
            &project,
            &request(vec![energy_axis(vec![100.0, 200.0])], Some(500.0)),
        )
        .unwrap();
        assert_eq!(scaled.cells[0].metrics.installed_cost_usd, 50_000.0);
        assert_eq!(scaled.cells[1].metrics.installed_cost_usd, 100_000.0);
        let held = run(
            &project,
            &request(vec![energy_axis(vec![100.0, 200.0])], None),
        )
        .unwrap();
        assert_eq!(held.cells[1].metrics.installed_cost_usd, 10_000.0);
    }

    #[test]
    fn a_scaled_study_cautions_about_what_does_not_scale() {
        let project = study_project();
        let severity = |axes, rate| {
            let study = run(&project, &request(axes, rate)).unwrap();
            let warning = study
                .warnings
                .iter()
                .find(|w| w.code == "sensitivity.installed_cost_scaled")
                .cloned()
                .unwrap();
            (warning.severity, warning.message)
        };
        let power = SensitivityAxis {
            param: SensitivityParam::BatteryPowerKw,
            values: vec![25.0, 50.0],
        };
        let (level, message) = severity(vec![energy_axis(vec![100.0]), power], Some(500.0));
        assert_eq!(level, crate::error::Severity::Caution);
        assert!(message.contains("more power costs nothing"), "{message}");
        // With nothing left out, the rate is only a note.
        let (level, _) = severity(vec![energy_axis(vec![100.0])], Some(500.0));
        assert_eq!(level, crate::error::Severity::Info);
    }

    #[test]
    fn combinations_are_row_major_and_stable() {
        let axes = vec![
            SensitivityAxis {
                param: SensitivityParam::BatteryEnergyKwh,
                values: vec![100.0, 200.0],
            },
            SensitivityAxis {
                param: SensitivityParam::CapexUsd,
                values: vec![1.0, 2.0, 3.0],
            },
        ];
        let combos = combinations(&axes);
        assert_eq!(combos.len(), 6);
        assert_eq!(combos[0], vec![100.0, 1.0]);
        assert_eq!(combos[1], vec![100.0, 2.0]);
        assert_eq!(combos[2], vec![100.0, 3.0]);
        assert_eq!(combos[3], vec![200.0, 1.0]);
        assert_eq!(combos[5], vec![200.0, 3.0]);
    }

    #[test]
    fn a_third_axis_is_refused() {
        let axis = SensitivityAxis {
            param: SensitivityParam::CapexUsd,
            values: vec![1.0],
        };
        let axes = vec![axis.clone(), axis.clone(), axis];
        let error = validate_axes(&axes, &Scenario::no_storage_baseline()).unwrap_err();
        assert_eq!(error.code(), "limit.sensitivity_axes");
    }

    #[test]
    fn the_cell_cap_is_enforced_before_anything_runs() {
        let axes = vec![
            SensitivityAxis {
                param: SensitivityParam::CapexUsd,
                values: (0..16).map(f64::from).collect(),
            },
            SensitivityAxis {
                param: SensitivityParam::DiscountRatePctPerYear,
                values: (0..16).map(f64::from).collect(),
            },
        ];
        // 16 x 16 = 256, exactly the cap: allowed.
        assert!(validate_axes(&axes, &Scenario::no_storage_baseline()).is_ok());
    }

    #[test]
    fn a_battery_axis_on_a_batteryless_scenario_is_refused() {
        let axes = vec![SensitivityAxis {
            param: SensitivityParam::BatteryEnergyKwh,
            values: vec![100.0],
        }];
        let error = validate_axes(&axes, &Scenario::no_storage_baseline()).unwrap_err();
        assert_eq!(error.code(), "validation.sensitivity_needs_battery");
    }

    #[test]
    fn duplicate_axes_and_duplicate_values_are_refused() {
        let axes = vec![
            SensitivityAxis {
                param: SensitivityParam::CapexUsd,
                values: vec![1.0],
            },
            SensitivityAxis {
                param: SensitivityParam::CapexUsd,
                values: vec![2.0],
            },
        ];
        assert_eq!(
            validate_axes(&axes, &Scenario::no_storage_baseline())
                .unwrap_err()
                .code(),
            "validation.sensitivity_duplicate_axis"
        );

        let axes = vec![SensitivityAxis {
            param: SensitivityParam::CapexUsd,
            values: vec![1.0, 1.0],
        }];
        assert_eq!(
            validate_axes(&axes, &Scenario::no_storage_baseline())
                .unwrap_err()
                .code(),
            "validation.sensitivity_duplicate_value"
        );
    }

    #[test]
    fn a_non_finite_axis_value_is_refused() {
        let axes = vec![SensitivityAxis {
            param: SensitivityParam::CapexUsd,
            values: vec![f64::NAN],
        }];
        assert_eq!(
            validate_axes(&axes, &Scenario::no_storage_baseline())
                .unwrap_err()
                .code(),
            "validation.non_finite"
        );
    }

    #[test]
    fn varying_battery_power_moves_the_inverter_with_it() {
        let mut scenario = Scenario::no_storage_baseline();
        scenario.battery = Some(crate::schema::BatterySpec {
            nameplate_energy_kwh: 100.0,
            max_charge_kw: 10.0,
            max_discharge_kw: 10.0,
            soc_min_pct: 10.0,
            soc_max_pct: 100.0,
            reserve_soc_pct: 20.0,
            charge_efficiency_pct: 95.0,
            discharge_efficiency_pct: 95.0,
            self_discharge_pct_per_month: 0.0,
            temperature_derate: None,
        });
        scenario.inverter.battery_power_kw = 10.0;
        scenario.initial_soc_pct = 50.0;

        SensitivityParam::BatteryPowerKw
            .apply(&mut scenario, 75.0)
            .unwrap();
        let battery = scenario.battery.as_ref().unwrap();
        assert!((battery.max_charge_kw - 75.0).abs() < 1e-12);
        assert!((battery.max_discharge_kw - 75.0).abs() < 1e-12);
        // Without this, the axis would flatten past 10 kW for no visible reason.
        assert!((scenario.inverter.battery_power_kw - 75.0).abs() < 1e-12);
    }

    #[test]
    fn a_demand_axis_on_a_tariff_with_no_demand_charge_is_refused() {
        let mut scenario = Scenario::no_storage_baseline();
        let error = SensitivityParam::DemandPricePerKwMonthUsd
            .apply(&mut scenario, 15.0)
            .unwrap_err();
        assert_eq!(error.code(), "validation.sensitivity_no_demand_charge");
    }
}
