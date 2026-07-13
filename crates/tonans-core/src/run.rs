//! Running a project: every scenario, compared against the baseline.
//!
//! The baseline is simulated first, because every other scenario's economics is a
//! *difference* from it. Scenarios are then run in the order they appear in the
//! project, and that order is preserved through every result, export, and chart —
//! nothing here iterates a hash map.
//!
//! A [`RunResult`] carries its own provenance: the versions it was produced by,
//! the hash of the project, the hash of the inputs that actually determine the
//! numbers, the enforced limits, the warnings, and the disclaimer. It is meant to
//! be self-defending. Someone reading it a year from now should not have to ask
//! what produced it.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canonical::{canonical_hash, canonical_value, Hash};
use crate::economics::{self, EconomicsSummary};
use crate::engine::{
    self, CostSummary, EnergySummary, EngineOptions, IntervalRow, OutageResult, ResolvedInput,
};
use crate::error::{Error, Result, Warning, WarningLedger};
use crate::licensing::Entitlements;
use crate::limits::Limits;
use crate::schema::{Dataset, DemandPeriods, Project, Scenario, SeriesKind};
use crate::version::{
    APP_VERSION, MODEL_EXCLUSIONS, MODEL_VERSION, PLANNING_DISCLAIMER, SCHEMA_VERSION,
};

/// How one scenario compares to the baseline.
///
/// Signed so that a *negative* cost delta means cheaper than the baseline, which
/// is the direction a reader expects when the column is headed "change".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineComparison {
    /// The scenario everything is measured against.
    pub baseline_scenario_id: String,
    /// Whether this scenario *is* the baseline, in which case every delta is zero.
    pub is_baseline: bool,
    /// Grid cost, minus the baseline's. Negative is a saving.
    pub grid_cost_delta_usd: f64,
    /// Imported energy, minus the baseline's.
    pub grid_import_delta_kwh: f64,
    /// Exported energy, minus the baseline's.
    pub grid_export_delta_kwh: f64,
    /// Peak import, minus the baseline's.
    pub peak_import_delta_kw: f64,
    /// Unmet critical load, minus the baseline's. Negative means more survivable.
    pub unmet_critical_delta_kwh: f64,
    /// Curtailed generation, minus the baseline's.
    pub curtailed_delta_kwh: f64,
    /// Self-sufficiency, minus the baseline's.
    pub self_sufficiency_delta_pct: f64,
}

/// One scenario, run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// The scenario's id.
    pub scenario_id: String,
    /// Its name.
    pub scenario_name: String,
    /// The dispatch policy's name, for the scenario card.
    pub dispatch_policy: String,
    /// The exact assumptions this result was produced from. Carried in full so
    /// that a result can never be read next to assumptions that have since changed.
    pub scenario: Scenario,
    /// The energy story.
    pub energy: EnergySummary,
    /// The money story.
    pub cost: CostSummary,
    /// Outages, replayed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outages: Vec<OutageResult>,
    /// The economics.
    pub economics: EconomicsSummary,
    /// How it compares to the baseline.
    pub comparison: BaselineComparison,
    /// Everything a reader must know before trusting the numbers above.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Every interval. Never serialized into the summary JSON — it goes to the
    /// timeseries CSV, where a reader can actually work with it.
    #[serde(skip)]
    pub timeline: Vec<IntervalRow>,
}

/// A whole project, run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    /// Version of Tonans that produced this.
    pub app_version: String,
    /// Version of the energy model that produced it. Bumped whenever a number
    /// could change for an unchanged input.
    pub model_version: String,
    /// Version of the project schema it was read from.
    pub schema_version: u32,

    /// The project's name.
    pub project_name: String,
    /// Hash of the whole project file, including its data.
    pub project_hash: Hash,
    /// Hash of *only* the things that determine the numbers: the data set, the
    /// scenarios, and the model version.
    ///
    /// Two runs with the same `model_input_hash` must produce identical results.
    /// The determinism test asserts exactly that, and a reader can use it to tell
    /// whether a difference between two reports is real or cosmetic.
    pub model_input_hash: Hash,
    /// When the run happened. Excluded from every hash above, so that a rerun of
    /// the same inputs is bit-identical apart from this line.
    pub generated_utc: DateTime<Utc>,

    /// The baseline everything is compared against.
    pub baseline_scenario_id: String,
    /// Every scenario, in project order.
    pub scenarios: Vec<ScenarioResult>,

    /// Warnings about the project and its data, above the level of one scenario.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// The envelope this run was produced inside.
    pub limits: Limits,
    /// The entitlement tier in force, stamped so a report cannot be separated
    /// from the terms it was produced under.
    pub tier: crate::licensing::Tier,
    /// Whether reports from this run may be given to a third party.
    pub client_report_rights: bool,

    /// The planning-only disclaimer. Present in the data, not merely in the
    /// template, so that a machine-readable export carries it too.
    pub disclaimer: String,
    /// What this model does not do.
    pub model_exclusions: Vec<String>,
}

impl RunResult {
    /// The scenario with the given id.
    #[must_use]
    pub fn scenario(&self, id: &str) -> Option<&ScenarioResult> {
        self.scenarios.iter().find(|s| s.scenario_id == id)
    }

    /// The baseline's result.
    #[must_use]
    pub fn baseline(&self) -> Option<&ScenarioResult> {
        self.scenario(&self.baseline_scenario_id)
    }

    /// Hash of the numeric model outputs. Two runs with the same
    /// [`Self::model_input_hash`] must match, even when presentation metadata,
    /// entitlement labels, warning prose, or the run time differ.
    pub fn results_hash(&self) -> Result<Hash> {
        let mut scenarios = Vec::with_capacity(self.scenarios.len());
        for scenario in &self.scenarios {
            let mut comparison = canonical_value(&scenario.comparison)?;
            if let Some(object) = comparison.as_object_mut() {
                object.remove("baseline_scenario_id");
            }
            let mut outages = Vec::with_capacity(scenario.outages.len());
            for outage in &scenario.outages {
                let mut value = canonical_value(outage)?;
                if let Some(object) = value.as_object_mut() {
                    object.remove("label");
                }
                outages.push(value);
            }
            let mut economics = canonical_value(&scenario.economics)?;
            if let Some(object) = economics.as_object_mut() {
                object.remove("degradation_method");
            }
            scenarios.push(NumericScenarioOutputs {
                energy: &scenario.energy,
                cost: &scenario.cost,
                outages,
                economics,
                comparison,
                timeline: &scenario.timeline,
            });
        }
        canonical_hash(&NumericOutputs {
            model_input_hash: &self.model_input_hash,
            scenarios,
        })
    }

    /// Every warning in the run, most severe first.
    #[must_use]
    pub fn all_warnings(&self) -> Vec<Warning> {
        let mut ledger = WarningLedger::new();
        ledger.extend(self.warnings.iter().cloned());
        for scenario in &self.scenarios {
            ledger.extend(scenario.warnings.iter().cloned());
        }
        ledger.into_sorted()
    }
}

#[derive(Serialize)]
struct NumericOutputs<'a> {
    model_input_hash: &'a Hash,
    scenarios: Vec<NumericScenarioOutputs<'a>>,
}

#[derive(Serialize)]
struct NumericScenarioOutputs<'a> {
    energy: &'a crate::engine::EnergySummary,
    cost: &'a crate::engine::CostSummary,
    outages: Vec<serde_json::Value>,
    economics: serde_json::Value,
    comparison: serde_json::Value,
    timeline: &'a [IntervalRow],
}

/// What determines the numbers, and nothing else. Hashed into
/// [`RunResult::model_input_hash`].
#[derive(Serialize)]
struct ModelInputs<'a> {
    model_version: &'static str,
    timezone: &'a str,
    dataset: Option<ModelDatasetInputs<'a>>,
    scenarios: Vec<serde_json::Value>,
    baseline_scenario_index: usize,
}

/// Only the imported values and grid fields that the model reads. Import time,
/// source filename, column names, and warnings belong in provenance, but changing
/// one of them must not pretend the numeric model inputs changed.
#[derive(Serialize)]
struct ModelDatasetInputs<'a> {
    interval_minutes: u32,
    start_utc: DateTime<Utc>,
    interval_count: usize,
    series: BTreeMap<SeriesKind, &'a [f64]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    demand_periods: &'a Option<DemandPeriods>,
}

impl<'a> From<&'a Dataset> for ModelDatasetInputs<'a> {
    fn from(dataset: &'a Dataset) -> Self {
        Self {
            interval_minutes: dataset.interval_minutes,
            start_utc: dataset.start_utc,
            interval_count: dataset.interval_count,
            series: dataset
                .series
                .iter()
                .map(|(kind, series)| (*kind, series.values.as_slice()))
                .collect(),
            demand_periods: &dataset.demand_periods,
        }
    }
}

fn model_scenarios(scenarios: &[Scenario]) -> Result<Vec<serde_json::Value>> {
    let mut values = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let mut value = canonical_value(scenario)?;
        if let Some(object) = value.as_object_mut() {
            object.remove("id");
            object.remove("name");
            if let Some(windows) = object
                .get_mut("outage")
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|outage| outage.get_mut("windows"))
                .and_then(serde_json::Value::as_array_mut)
            {
                for window in windows {
                    if let Some(window) = window.as_object_mut() {
                        window.remove("label");
                    }
                }
            }
        }
        values.push(value);
    }
    Ok(values)
}

/// Run every scenario in `project`.
///
/// `now_utc` is injected; this crate has no clock.
pub fn run_project(
    project: &Project,
    entitlements: Entitlements,
    now_utc: DateTime<Utc>,
) -> Result<RunResult> {
    entitlements.check_scenario_count(project.scenarios.len())?;
    let grid = project.validate_runnable()?;

    let baseline_spec = project.baseline().ok_or_else(|| {
        Error::validation(
            "validation.unknown_baseline",
            "project.baseline_scenario_id",
            "the baseline scenario does not exist",
        )
    })?;
    let baseline_scenario_index = project
        .scenarios
        .iter()
        .position(|scenario| scenario.id == project.baseline_scenario_id)
        .ok_or_else(|| {
            Error::validation(
                "validation.unknown_baseline",
                "project.baseline_scenario_id",
                "the baseline scenario does not exist",
            )
        })?;

    // The baseline first: every other scenario's economics is a difference from it.
    let baseline_input = ResolvedInput::resolve(project, baseline_spec, &grid)?;
    let baseline_engine = engine::run(
        baseline_spec,
        &baseline_input.as_input(&grid),
        EngineOptions::default(),
    )?;
    let baseline_cost = baseline_engine.cost.clone();
    let baseline_energy = baseline_engine.energy.clone();

    let mut scenarios = Vec::with_capacity(project.scenarios.len());
    for spec in &project.scenarios {
        let is_baseline = spec.id == baseline_spec.id;

        // Reuse the baseline's run rather than simulating it twice: identical
        // inputs, identical outputs, and it keeps a large project honest about
        // where its time goes.
        let engine_result = if is_baseline {
            baseline_engine.clone()
        } else {
            let input = ResolvedInput::resolve(project, spec, &grid)?;
            engine::run(spec, &input.as_input(&grid), EngineOptions::default())?
        };

        let mut ledger = WarningLedger::new();
        ledger.extend(engine_result.warnings.clone());

        let economics = economics::evaluate(
            spec,
            &engine_result.energy,
            &engine_result.cost,
            &baseline_cost,
            &mut ledger,
        )?;

        let comparison = BaselineComparison {
            baseline_scenario_id: baseline_spec.id.clone(),
            is_baseline,
            grid_cost_delta_usd: engine_result.cost.total_grid_cost_usd
                - baseline_cost.total_grid_cost_usd,
            grid_import_delta_kwh: engine_result.energy.grid_import_kwh
                - baseline_energy.grid_import_kwh,
            grid_export_delta_kwh: engine_result.energy.grid_export_kwh
                - baseline_energy.grid_export_kwh,
            peak_import_delta_kw: engine_result.energy.peak_import_kw
                - baseline_energy.peak_import_kw,
            unmet_critical_delta_kwh: engine_result.energy.unmet_critical_kwh
                - baseline_energy.unmet_critical_kwh,
            curtailed_delta_kwh: engine_result.energy.curtailed_kwh - baseline_energy.curtailed_kwh,
            self_sufficiency_delta_pct: engine_result.energy.self_sufficiency_pct
                - baseline_energy.self_sufficiency_pct,
        };

        scenarios.push(ScenarioResult {
            scenario_id: spec.id.clone(),
            scenario_name: spec.name.clone(),
            dispatch_policy: spec.dispatch.name().to_string(),
            scenario: spec.clone(),
            energy: engine_result.energy,
            cost: engine_result.cost,
            outages: engine_result.outages,
            economics,
            comparison,
            warnings: ledger.into_sorted(),
            timeline: engine_result.timeline,
        });
    }

    let mut project_ledger = WarningLedger::new();
    project_ledger.extend(project.advisories());
    if let Some(dataset) = &project.dataset {
        project_ledger.extend(dataset.import_warnings.clone());
        project_ledger.extend(engine::dataset_advisories(dataset));
    }

    let model_input_hash = canonical_hash(&ModelInputs {
        model_version: MODEL_VERSION,
        timezone: &project.site.timezone,
        dataset: project.dataset.as_ref().map(ModelDatasetInputs::from),
        scenarios: model_scenarios(&project.scenarios)?,
        baseline_scenario_index,
    })?;

    Ok(RunResult {
        app_version: APP_VERSION.to_string(),
        model_version: MODEL_VERSION.to_string(),
        schema_version: SCHEMA_VERSION,
        project_name: project.name.clone(),
        project_hash: canonical_hash(project)?,
        model_input_hash,
        generated_utc: now_utc,
        baseline_scenario_id: project.baseline_scenario_id.clone(),
        scenarios,
        warnings: project_ledger.into_sorted(),
        limits: Limits::current(),
        tier: entitlements.tier,
        client_report_rights: entitlements.client_report_rights,
        disclaimer: PLANNING_DISCLAIMER.to_string(),
        model_exclusions: MODEL_EXCLUSIONS.iter().map(|s| (*s).to_string()).collect(),
    })
}
