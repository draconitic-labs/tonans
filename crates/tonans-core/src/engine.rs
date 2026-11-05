//! The deterministic single-bus interval engine.
//!
//! # The model, in full
//!
//! One electrical bus. Every quantity is energy in kWh over one interval, or the
//! mean power in kW that implies. There is no voltage, no reactive power, no
//! impedance, and no time resolution finer than the interval. Within an interval,
//! everything is a constant.
//!
//! Each interval is evaluated in a fixed order, and the order is part of the
//! model:
//!
//! 1. **Standing loss.** Self-discharge is taken off stored energy first, as a
//!    fraction of what is stored, pro-rated over a 730-hour month.
//! 2. **Derating.** Ambient temperature fixes this interval's power ceiling and
//!    charge ceiling, by linear interpolation over the derating table.
//! 3. **Envelope.** The usable state-of-charge window, the reserve floor, and the
//!    charge and discharge power caps are computed. The battery's caps and the
//!    inverter's rating are combined by taking the smaller. All of them are *at
//!    the AC bus*.
//! 4. **Dispatch.** The named policy proposes one signed setpoint. Grid-connected
//!    only; during an outage the policy is bypassed (see below).
//! 5. **Clamping.** The setpoint is clipped to the envelope. Charge and discharge
//!    are two branches of one signed decision, so **simultaneous charge and
//!    discharge is impossible by construction**, not by a later check.
//! 6. **Grid.** Import or export, never both, falls out of the bus balance. An
//!    export ceiling is enforced by first reducing battery discharge and then
//!    curtailing generation — in that order, because discharge is the controllable
//!    one.
//! 7. **Storage.** Stored energy is updated by the asymmetric efficiencies.
//! 8. **Money.** Import cost and export credit are accrued at this interval's
//!    price; monthly peaks are tracked for the demand charge.
//!
//! ## During an outage
//!
//! Grid import and export are both zero. Non-critical load is **shed** — Tonans
//! assumes a transfer switch that isolates critical circuits, and reports the shed
//! energy separately so the assumption is visible. Generation serves critical load
//! first, then charges the battery, then is curtailed. The battery may discharge
//! *below the reserve floor*, down to the hard minimum: the reserve exists for
//! precisely this moment. Anything the battery and generation cannot cover is
//! **unmet critical load**, and it is never negative and never hidden.
//!
//! ## Efficiency, and where the losses live
//!
//! Charge and discharge efficiencies are stated at the AC bus and carry *all* of
//! the conversion loss. The inverter contributes a power ceiling and nothing else.
//! Splitting the loss across both is the most common way an energy model quietly
//! overstates a battery, and Tonans does not do it. See `docs/MODEL.md`.
//!
//! ## What holds, always
//!
//! Every one of these is asserted per interval and over the whole run, in
//! `tests/invariants.rs`, over thousands of pseudo-random projects:
//!
//! * `generation_used + import + discharge = served_load + export + charge`
//! * `Δstored = charge × η_c − discharge / η_d − standing loss`
//! * `charge × discharge = 0`
//! * `import × export = 0`
//! * `0 ≤ soc ≤ soc_max`, and `soc ≥ soc_min` outside an outage's final depletion
//! * `unmet ≥ 0`, `curtailed ≥ 0`, `0 ≤ curtailed ≤ generation`
//! * import and export are both zero during an outage

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{finite, Error, Result, Warning, WarningLedger};
use crate::schema::{
    BatterySpec, DispatchPolicy, GapPolicy, InverterSpec, RateSource, Scenario, SeriesKind,
    TariffSpec,
};
use crate::timegrid::IntervalGrid;

/// Hours in the nominal month used to pro-rate self-discharge: 365 × 24 / 12.
const HOURS_PER_NOMINAL_MONTH: f64 = 730.0;

/// Energy below this is treated as zero when deciding whether something happened.
/// Well above f64 noise, far below anything a meter could resolve.
const EPSILON_KWH: f64 = 1e-9;

/// The residual of the bus balance that we are willing to attribute to floating
/// point. Anything larger is a defect and is reported as one.
const BALANCE_TOLERANCE_KWH: f64 = 1e-6;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// The per-interval data one run needs, already resolved from the project.
#[derive(Debug, Clone)]
pub struct EngineInput<'a> {
    /// The interval grid.
    pub grid: &'a IntervalGrid,
    /// Site load, kW. Required.
    pub load_kw: &'a [f64],
    /// Critical load, kW. Zeros if none was imported.
    pub critical_load_kw: &'a [f64],
    /// On-site generation at the AC bus, kW. Zeros if none was imported.
    pub generation_kw: &'a [f64],
    /// Import price, already multiplied and resolved from flat or series.
    pub import_price_per_kwh_usd: &'a [f64],
    /// Export credit, already multiplied and resolved from flat or series.
    pub export_price_per_kwh_usd: &'a [f64],
    /// Ambient temperature, if a series was imported.
    pub temperature_c: Option<&'a [f64]>,
    /// Demand-charge period label per interval, as an index into `demand_labels`.
    pub demand_period_index: &'a [u16],
    /// The distinct demand-charge period labels.
    pub demand_labels: &'a [String],
    /// Whether the grid is down in each interval.
    pub outage: &'a [bool],
}

impl EngineInput<'_> {
    fn check(&self) -> Result<()> {
        let n = self.grid.count();
        let lengths = [
            ("load_kw", self.load_kw.len()),
            ("critical_load_kw", self.critical_load_kw.len()),
            ("generation_kw", self.generation_kw.len()),
            (
                "import_price_per_kwh_usd",
                self.import_price_per_kwh_usd.len(),
            ),
            (
                "export_price_per_kwh_usd",
                self.export_price_per_kwh_usd.len(),
            ),
            ("demand_period_index", self.demand_period_index.len()),
            ("outage", self.outage.len()),
        ];
        for (name, length) in lengths {
            if length != n {
                return Err(Error::validation(
                    "validation.series_length",
                    format!("engine.{name}"),
                    format!("'{name}' has {length} values but the grid has {n} intervals"),
                ));
            }
        }
        if let Some(temperature) = self.temperature_c {
            if temperature.len() != n {
                return Err(Error::validation(
                    "validation.series_length",
                    "engine.temperature_c",
                    format!(
                        "temperature has {} values but the grid has {n} intervals",
                        temperature.len()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// How much of the run to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineOptions {
    /// Keep every interval. A sensitivity sweep sets this false: 256 cells × a
    /// year of intervals is a lot of memory for numbers nobody will chart.
    pub retain_timeline: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            retain_timeline: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// One interval, fully accounted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalRow {
    /// Start of the interval.
    pub start_utc: DateTime<Utc>,
    /// Site load presented to the bus.
    pub load_kwh: f64,
    /// The part of the load that must survive an outage.
    pub critical_load_kwh: f64,
    /// Load actually served. Equals `load_kwh` unless the grid is down.
    pub served_load_kwh: f64,
    /// Non-critical load shed because the grid was down.
    pub shed_noncritical_kwh: f64,
    /// Critical load that nothing could serve. Never negative.
    pub unmet_critical_kwh: f64,
    /// Generation the site had available.
    pub generation_available_kwh: f64,
    /// Generation that reached the bus.
    pub generation_used_kwh: f64,
    /// Generation serving load directly.
    pub generation_to_load_kwh: f64,
    /// Generation sent into storage at the AC bus.
    pub generation_to_battery_kwh: f64,
    /// Generation exported directly.
    pub generation_to_grid_kwh: f64,
    /// Generation thrown away: no load, no room in the battery, no export.
    pub curtailed_kwh: f64,
    /// Energy drawn from the bus into the battery.
    pub battery_charge_kwh: f64,
    /// Energy delivered from the battery to the bus.
    pub battery_discharge_kwh: f64,
    /// Energy in the battery at the end of the interval.
    pub stored_energy_kwh: f64,
    /// State of charge at the end of the interval, against nameplate × retention.
    pub soc_pct: f64,
    /// Energy taken from the grid.
    pub grid_import_kwh: f64,
    /// Grid energy serving load directly.
    pub grid_to_load_kwh: f64,
    /// Grid energy sent into storage at the AC bus.
    pub grid_to_battery_kwh: f64,
    /// Energy sent to the grid.
    pub grid_export_kwh: f64,
    /// Battery output serving load.
    pub battery_to_load_kwh: f64,
    /// Battery output exported to the grid.
    pub battery_to_grid_kwh: f64,
    /// Load served by battery energy originally charged from onsite generation.
    pub generation_origin_battery_to_load_kwh: f64,
    /// Load served by battery energy originally charged from the grid.
    pub grid_origin_battery_to_load_kwh: f64,
    /// Load served by energy present at the start of the model, whose source is unknown.
    pub initial_origin_battery_to_load_kwh: f64,
    /// Exported battery energy originally charged from onsite generation.
    pub generation_origin_battery_to_grid_kwh: f64,
    /// Exported battery energy originally charged from the grid.
    pub grid_origin_battery_to_grid_kwh: f64,
    /// Exported energy present at the start of the model, whose source is unknown.
    pub initial_origin_battery_to_grid_kwh: f64,
    /// Loss in conversion, both directions.
    pub conversion_loss_kwh: f64,
    /// Standing loss from the battery.
    pub self_discharge_loss_kwh: f64,
    /// Cost of this interval's import.
    pub import_cost_usd: f64,
    /// Credit for this interval's export.
    pub export_credit_usd: f64,
    /// Whether the grid was down.
    pub outage: bool,
}

/// The energy story of a whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergySummary {
    /// Intervals simulated.
    pub interval_count: usize,
    /// Hours simulated.
    pub duration_hours: f64,
    /// Total site load.
    pub load_kwh: f64,
    /// Total load served.
    pub served_load_kwh: f64,
    /// Total critical load.
    pub critical_load_kwh: f64,
    /// Total critical load nothing could serve.
    pub unmet_critical_kwh: f64,
    /// Total non-critical load shed during outages.
    pub shed_noncritical_kwh: f64,
    /// Total generation available.
    pub generation_available_kwh: f64,
    /// Total generation that reached the bus.
    pub generation_used_kwh: f64,
    /// Generation serving load directly.
    pub generation_to_load_kwh: f64,
    /// Generation sent into storage at the AC bus.
    pub generation_to_battery_kwh: f64,
    /// Generation exported directly.
    pub generation_to_grid_kwh: f64,
    /// Total generation thrown away.
    pub curtailed_kwh: f64,
    /// Total energy into the battery, at the bus.
    pub battery_charge_kwh: f64,
    /// Total energy out of the battery, at the bus.
    pub battery_discharge_kwh: f64,
    /// Total conversion loss.
    pub conversion_loss_kwh: f64,
    /// Total standing loss.
    pub self_discharge_loss_kwh: f64,
    /// Total grid import.
    pub grid_import_kwh: f64,
    /// Grid energy serving load directly.
    pub grid_to_load_kwh: f64,
    /// Grid energy sent into storage at the AC bus.
    pub grid_to_battery_kwh: f64,
    /// Total grid export.
    pub grid_export_kwh: f64,
    /// Battery output serving load.
    pub battery_to_load_kwh: f64,
    /// Battery output exported to the grid.
    pub battery_to_grid_kwh: f64,
    /// Load served by battery energy originally charged from onsite generation.
    pub generation_origin_battery_to_load_kwh: f64,
    /// Load served by battery energy originally charged from the grid.
    pub grid_origin_battery_to_load_kwh: f64,
    /// Load served by energy present at the start of the model, whose source is unknown.
    pub initial_origin_battery_to_load_kwh: f64,
    /// Exported battery energy originally charged from onsite generation.
    pub generation_origin_battery_to_grid_kwh: f64,
    /// Exported battery energy originally charged from the grid.
    pub grid_origin_battery_to_grid_kwh: f64,
    /// Exported energy present at the start of the model, whose source is unknown.
    pub initial_origin_battery_to_grid_kwh: f64,
    /// Highest mean import power in any one interval.
    pub peak_import_kw: f64,
    /// Highest mean export power in any one interval.
    pub peak_export_kw: f64,
    /// Energy in the battery before the first interval.
    pub initial_stored_energy_kwh: f64,
    /// Energy in the battery after the last.
    pub final_stored_energy_kwh: f64,
    /// Lowest state of charge reached.
    pub min_soc_pct: f64,
    /// Highest state of charge reached.
    pub max_soc_pct: f64,
    /// Share of available generation used directly on site or retained in storage,
    /// less generation-origin battery energy later exported. Zero with no generation.
    pub self_consumption_pct: f64,
    /// Share of served load met by onsite generation, directly or after storage.
    /// Grid-charged and unknown initial battery energy are conservatively excluded.
    pub self_sufficiency_pct: f64,
    /// Battery throughput, at the bus, expressed in full usable cycles.
    pub equivalent_full_cycles: f64,
    /// The bus balance, summed. Should be zero; published so a reader can check.
    ///
    /// This number is the model's own audit. If it is not within a rounding error
    /// of zero, nothing else on the page is trustworthy, and Tonans says so rather
    /// than letting the reader assume.
    pub energy_balance_residual_kwh: f64,
}

/// One outage, replayed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutageResult {
    /// When the grid went away.
    pub start_utc: DateTime<Utc>,
    /// When it came back.
    pub end_utc: DateTime<Utc>,
    /// What the user called it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// How long it lasted.
    pub duration_hours: f64,
    /// Critical load over the window.
    pub critical_load_kwh: f64,
    /// Critical load that was served.
    pub critical_served_kwh: f64,
    /// Critical load that was not.
    pub unmet_critical_kwh: f64,
    /// Non-critical load shed over the window.
    pub shed_noncritical_kwh: f64,
    /// Generation over the window.
    pub generation_used_kwh: f64,
    /// State of charge when the grid went away.
    pub soc_at_start_pct: f64,
    /// Lowest state of charge during the window.
    pub min_soc_pct: f64,
    /// Whether every kWh of critical load was served.
    pub survived: bool,
    /// Hours of critical load served without interruption from the outage's start.
    ///
    /// Equals `duration_hours` when the site survived the whole window. This is a
    /// *lower bound* on survivability: the outage may simply have ended before the
    /// battery ran out.
    pub survival_hours: f64,
    /// When critical load first went unserved, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_unmet_utc: Option<DateTime<Utc>>,
    /// Whether the battery reached its hard minimum during the window.
    pub reserve_depleted: bool,
}

/// The demand charge for one month and one period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemandChargeLine {
    /// Local calendar year.
    pub year: i32,
    /// Local calendar month, 1-12.
    pub month: u32,
    /// The tariff period this peak was measured in.
    pub period: String,
    /// The highest mean import power in this month and period.
    pub peak_import_kw: f64,
    /// The price applied to it.
    pub price_per_kw_month_usd: f64,
    /// The resulting charge.
    pub charge_usd: f64,
}

/// What the grid cost, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSummary {
    /// Cost of imported energy.
    pub import_energy_cost_usd: f64,
    /// Credit for exported energy.
    pub export_credit_usd: f64,
    /// Total demand charge.
    pub demand_charge_usd: f64,
    /// Total fixed charge.
    pub fixed_charge_usd: f64,
    /// The four above, combined. Export credit reduces the bill.
    pub total_grid_cost_usd: f64,
    /// Local calendar months the data set spans.
    pub billed_month_count: usize,
    /// Every demand-charge line, in calendar then period order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub demand_charge_lines: Vec<DemandChargeLine>,
}

/// One scenario, simulated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineResult {
    /// The energy story.
    pub energy: EnergySummary,
    /// The money story.
    pub cost: CostSummary,
    /// Every outage, replayed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outages: Vec<OutageResult>,
    /// Everything the reader must know before trusting the numbers above.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Every interval, if [`EngineOptions::retain_timeline`] was set.
    #[serde(skip)]
    pub timeline: Vec<IntervalRow>,
}

// ---------------------------------------------------------------------------
// The battery's per-interval envelope
// ---------------------------------------------------------------------------

/// A resolved battery, with everything the interval loop needs pre-computed.
struct Battery {
    /// Nameplate × initial retention. Temperature-independent, so state of charge
    /// is measured against a fixed reference and can never leave `0..=soc_max_pct`.
    reference_kwh: f64,
    soc_min_kwh: f64,
    soc_max_pct: f64,
    soc_min_pct: f64,
    reserve_kwh: f64,
    max_charge_kw: f64,
    max_discharge_kw: f64,
    charge_efficiency: f64,
    discharge_efficiency: f64,
    self_discharge_per_hour: f64,
    spec: BatterySpec,
    stored_kwh: f64,
    origins: StoredOrigins,
}

#[derive(Debug, Default, Clone, Copy)]
struct StoredOrigins {
    generation_kwh: f64,
    grid_kwh: f64,
    initial_kwh: f64,
}

impl StoredOrigins {
    fn total(self) -> f64 {
        self.generation_kwh + self.grid_kwh + self.initial_kwh
    }
}

impl Battery {
    fn new(
        spec: &BatterySpec,
        inverter: &InverterSpec,
        initial_soc_pct: f64,
        initial_capacity_pct: f64,
    ) -> Self {
        let reference_kwh = spec.nameplate_energy_kwh * initial_capacity_pct / 100.0;
        let stored_kwh = reference_kwh * initial_soc_pct / 100.0;
        Self {
            reference_kwh,
            soc_min_kwh: reference_kwh * spec.soc_min_pct / 100.0,
            soc_max_pct: spec.soc_max_pct,
            soc_min_pct: spec.soc_min_pct,
            reserve_kwh: reference_kwh * spec.reserve_soc_pct / 100.0,
            // The inverter is a ceiling on both directions. Taking the smaller of
            // the two here is the whole of its contribution: it adds no loss.
            max_charge_kw: spec.max_charge_kw.min(inverter.battery_power_kw),
            max_discharge_kw: spec.max_discharge_kw.min(inverter.battery_power_kw),
            charge_efficiency: spec.charge_efficiency_pct / 100.0,
            discharge_efficiency: spec.discharge_efficiency_pct / 100.0,
            self_discharge_per_hour: spec.self_discharge_pct_per_month
                / 100.0
                / HOURS_PER_NOMINAL_MONTH,
            spec: spec.clone(),
            stored_kwh,
            origins: StoredOrigins {
                initial_kwh: stored_kwh,
                ..StoredOrigins::default()
            },
        }
    }

    fn soc_pct(&self) -> f64 {
        if self.reference_kwh <= 0.0 {
            0.0
        } else {
            self.stored_kwh / self.reference_kwh * 100.0
        }
    }

    /// The AC energy deliverable from one full sweep of the usable window. The
    /// denominator of an equivalent full cycle.
    fn usable_energy_kwh(&self) -> f64 {
        self.reference_kwh * (self.soc_max_pct - self.soc_min_pct) / 100.0
            * self.discharge_efficiency
    }

    fn usable_internal_window_kwh(&self) -> f64 {
        self.reference_kwh * (self.soc_max_pct - self.soc_min_pct) / 100.0
    }

    fn withdraw(&mut self, requested_kwh: f64) -> StoredOrigins {
        let amount = requested_kwh.min(self.stored_kwh).max(0.0);
        if amount <= 0.0 || self.stored_kwh <= 0.0 {
            return StoredOrigins::default();
        }
        let fraction = (amount / self.stored_kwh).clamp(0.0, 1.0);
        let removed = StoredOrigins {
            generation_kwh: self.origins.generation_kwh * fraction,
            grid_kwh: self.origins.grid_kwh * fraction,
            initial_kwh: self.origins.initial_kwh * fraction,
        };
        self.origins.generation_kwh =
            (self.origins.generation_kwh - removed.generation_kwh).max(0.0);
        self.origins.grid_kwh = (self.origins.grid_kwh - removed.grid_kwh).max(0.0);
        self.origins.initial_kwh = (self.origins.initial_kwh - removed.initial_kwh).max(0.0);
        self.stored_kwh = (self.stored_kwh - amount).max(0.0);
        self.reconcile_origins();
        removed
    }

    fn deposit(&mut self, generation_bus_kwh: f64, grid_bus_kwh: f64) {
        let generation = generation_bus_kwh * self.charge_efficiency;
        let grid = grid_bus_kwh * self.charge_efficiency;
        self.origins.generation_kwh += generation;
        self.origins.grid_kwh += grid;
        self.stored_kwh += generation + grid;
        self.reconcile_origins();
    }

    fn reconcile_origins(&mut self) {
        let residual = self.stored_kwh - self.origins.total();
        if residual.abs() <= EPSILON_KWH {
            if self.origins.initial_kwh > 0.0 {
                self.origins.initial_kwh = (self.origins.initial_kwh + residual).max(0.0);
            } else if self.origins.generation_kwh > 0.0 {
                self.origins.generation_kwh = (self.origins.generation_kwh + residual).max(0.0);
            } else {
                self.origins.grid_kwh = (self.origins.grid_kwh + residual).max(0.0);
            }
        }
        debug_assert!((self.origins.total() - self.stored_kwh).abs() <= EPSILON_KWH * 10.0);
    }
}

/// This interval's ceilings, after temperature.
struct Envelope {
    /// Most AC energy the battery can absorb, given power cap and headroom.
    max_charge_kwh: f64,
    /// Most AC energy the battery can deliver, given power cap and available energy.
    max_discharge_kwh: f64,
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Simulate one scenario.
pub fn run(
    scenario: &Scenario,
    input: &EngineInput<'_>,
    options: EngineOptions,
) -> Result<EngineResult> {
    input.check()?;

    let grid = input.grid;
    let n = grid.count();
    let h = grid.hours();
    let mut ledger = WarningLedger::new();

    let mut battery = scenario.battery.as_ref().map(|spec| {
        Battery::new(
            spec,
            &scenario.inverter,
            scenario.initial_soc_pct,
            scenario.initial_capacity_pct,
        )
    });

    let export_limit_kwh = scenario.inverter.site_export_limit_kw.map(|kw| kw * h);

    // Only an explicit manual schedule may command the battery to export. The
    // three rule-based policies never propose more discharge than the site is
    // consuming, so this flag changes nothing for them; it exists so that an
    // analyst who wants to model a merchant discharge can, and so that nothing
    // else can do it by accident.
    let is_manual_schedule = matches!(scenario.dispatch, DispatchPolicy::ManualSchedule { .. });
    let allow_battery_export = is_manual_schedule;
    let schedule = ScheduleLookup::build(&scenario.dispatch, grid);

    let mut timeline: Vec<IntervalRow> = if options.retain_timeline {
        Vec::with_capacity(n)
    } else {
        Vec::new()
    };

    let mut totals = Totals::default();
    let mut demand = DemandTracker::new(input.demand_labels);
    let mut months = MonthTracker::default();
    let mut clamped_setpoints = 0usize;
    let mut temperature_clamped = false;
    let initial_stored_kwh = battery.as_ref().map_or(0.0, |b| b.stored_kwh);
    let mut min_soc_pct = battery.as_ref().map_or(0.0, Battery::soc_pct);
    let mut max_soc_pct = min_soc_pct;
    let mut soc_by_interval: Vec<f64> = if scenario.outage.windows.is_empty() {
        Vec::new()
    } else {
        Vec::with_capacity(n + 1)
    };
    let mut minimum_soc_by_interval: Vec<f64> = if scenario.outage.windows.is_empty() {
        Vec::new()
    } else {
        Vec::with_capacity(n)
    };
    if !scenario.outage.windows.is_empty() {
        soc_by_interval.push(min_soc_pct);
    }

    for i in 0..n {
        let outage = input.outage[i];
        let load_kwh = input.load_kw[i] * h;
        let critical_kwh = input.critical_load_kw[i] * h;
        let generation_kwh = input.generation_kw[i] * h;
        let mut interval_min_soc_pct = battery.as_ref().map_or(0.0, Battery::soc_pct);

        // 1. Standing loss, before anything else can use the energy.
        let mut self_discharge_loss_kwh = 0.0;
        if let Some(b) = &mut battery {
            let loss = (b.stored_kwh * b.self_discharge_per_hour * h)
                .min((b.stored_kwh - b.soc_min_kwh).max(0.0))
                .max(0.0);
            b.withdraw(loss);
            self_discharge_loss_kwh = loss;
            interval_min_soc_pct = interval_min_soc_pct.min(b.soc_pct());
            min_soc_pct = min_soc_pct.min(interval_min_soc_pct);
        }

        // 2 and 3. Temperature, then this interval's envelope.
        let envelope = match &battery {
            None => Envelope {
                max_charge_kwh: 0.0,
                max_discharge_kwh: 0.0,
            },
            Some(b) => {
                let (power_factor, capacity_factor) =
                    match (&b.spec.temperature_derate, input.temperature_c) {
                        (Some(derate), Some(temperatures)) => {
                            let t = temperatures[i];
                            let lowest = derate.points[0].temperature_c;
                            let highest = derate.points[derate.points.len() - 1].temperature_c;
                            if t < lowest || t > highest {
                                temperature_clamped = true;
                            }
                            derate.factors_at(t)
                        }
                        _ => (1.0, 1.0),
                    };

                // Capacity derating lowers the *charge ceiling*. It does not
                // destroy energy already stored: a pack that cools down does not
                // lose its charge, it loses headroom. Tonans does not model the
                // loss of deliverable energy from an already-charged cold pack —
                // see the limitations section of docs/MODEL.md.
                let soc_max_kwh = b.reference_kwh * b.soc_max_pct / 100.0;
                let headroom_kwh = (soc_max_kwh - b.stored_kwh).max(0.0);
                let temperature_floor_kwh =
                    b.soc_min_kwh + b.usable_internal_window_kwh() * (1.0 - capacity_factor);

                let floor_kwh = if outage {
                    // The reserve exists for this. Discharge to the hard floor.
                    b.soc_min_kwh.max(temperature_floor_kwh)
                } else {
                    b.soc_min_kwh
                        .max(b.reserve_kwh)
                        .max(policy_floor_kwh(&scenario.dispatch, b))
                        .max(temperature_floor_kwh)
                };
                let available_kwh = (b.stored_kwh - floor_kwh).max(0.0);

                Envelope {
                    max_charge_kwh: (headroom_kwh / b.charge_efficiency)
                        .min(b.max_charge_kw * power_factor * h)
                        .max(0.0),
                    max_discharge_kwh: (available_kwh * b.discharge_efficiency)
                        .min(b.max_discharge_kw * power_factor * h)
                        .max(0.0),
                }
            }
        };

        // 4 through 6.
        let flows = if outage {
            island(critical_kwh, load_kwh, generation_kwh, &envelope)
        } else {
            let net_kwh = load_kwh - generation_kwh;
            let desired_kwh = match &battery {
                None => 0.0,
                Some(b) => {
                    let context = DispatchContext {
                        net_load_kw: net_kwh / h,
                        soc_pct: b.soc_pct(),
                        max_charge_kw: envelope.max_charge_kwh / h,
                        max_discharge_kw: envelope.max_discharge_kwh / h,
                        target_charge_kw: policy_target_charge_kw(&scenario.dispatch, b, h),
                    };
                    let proposed =
                        setpoint_kw(&scenario.dispatch, &context, schedule.as_ref(), i) * h;

                    // A rule-based policy never proposes more than the envelope
                    // allows, because the envelope is in its context. A manual
                    // schedule can, and when it does the user must be told: the
                    // schedule they wrote is not the schedule that ran.
                    if is_manual_schedule {
                        let clamped = clamp_signed(
                            proposed,
                            envelope.max_charge_kwh,
                            envelope.max_discharge_kwh,
                        );
                        if (clamped - proposed).abs() > EPSILON_KWH {
                            clamped_setpoints += 1;
                        }
                    }
                    proposed
                }
            };
            grid_connected(
                net_kwh,
                generation_kwh,
                desired_kwh,
                &envelope,
                allow_battery_export,
                export_limit_kwh,
            )
        };

        // 7. Storage update, with the asymmetric efficiencies.
        let mut conversion_loss_kwh = 0.0;
        let mut origin_flows = BatteryOriginFlows::default();
        if let Some(b) = &mut battery {
            let into_storage = flows.charge_kwh * b.charge_efficiency;
            let from_storage = if flows.discharge_kwh > 0.0 {
                finite(
                    "battery_discharge/eta",
                    flows.discharge_kwh / b.discharge_efficiency,
                )?
            } else {
                0.0
            };
            conversion_loss_kwh =
                (flows.charge_kwh - into_storage) + (from_storage - flows.discharge_kwh);
            let removed = b.withdraw(from_storage);
            b.deposit(flows.generation_to_battery_kwh, flows.grid_to_battery_kwh);
            origin_flows = BatteryOriginFlows::from_discharge(
                removed,
                b.discharge_efficiency,
                flows.battery_to_load_kwh,
                flows.battery_to_grid_kwh,
            );

            // Both directions were clipped to the envelope, so this can only be
            // floating-point dust. Snapping it keeps state of charge inside its
            // bounds exactly, rather than at 100.00000000000001%.
            if b.stored_kwh < 0.0 && b.stored_kwh > -EPSILON_KWH {
                b.stored_kwh = 0.0;
            }
            if b.stored_kwh < 0.0 {
                return Err(Error::non_finite("stored_energy_kwh", b.stored_kwh));
            }
            let soc = b.soc_pct();
            interval_min_soc_pct = interval_min_soc_pct.min(soc);
            min_soc_pct = min_soc_pct.min(soc);
            max_soc_pct = max_soc_pct.max(soc);
        }

        // 8. Money.
        let import_price = input.import_price_per_kwh_usd[i];
        let export_price = input.export_price_per_kwh_usd[i];
        let import_cost_usd = flows.import_kwh * import_price;
        let export_credit_usd = flows.export_kwh * export_price;

        let (year, month) = grid.local_month_of(i);
        months.observe(year, month);
        if flows.import_kwh > 0.0 {
            let period = usize::from(input.demand_period_index[i]);
            demand.observe(year, month, period, flows.import_kwh / h);
        }

        // Accumulate.
        let served_load_kwh = load_kwh - flows.shed_kwh - flows.unmet_kwh;
        totals.load_kwh += load_kwh;
        totals.critical_load_kwh += critical_kwh;
        totals.served_load_kwh += served_load_kwh;
        totals.shed_noncritical_kwh += flows.shed_kwh;
        totals.unmet_critical_kwh += flows.unmet_kwh;
        totals.generation_available_kwh += generation_kwh;
        totals.generation_used_kwh += flows.generation_used_kwh;
        totals.generation_to_load_kwh += flows.generation_to_load_kwh;
        totals.generation_to_battery_kwh += flows.generation_to_battery_kwh;
        totals.generation_to_grid_kwh += flows.generation_to_grid_kwh;
        totals.curtailed_kwh += flows.curtailed_kwh;
        totals.battery_charge_kwh += flows.charge_kwh;
        totals.battery_discharge_kwh += flows.discharge_kwh;
        totals.conversion_loss_kwh += conversion_loss_kwh;
        totals.self_discharge_loss_kwh += self_discharge_loss_kwh;
        totals.grid_import_kwh += flows.import_kwh;
        totals.grid_to_load_kwh += flows.grid_to_load_kwh;
        totals.grid_to_battery_kwh += flows.grid_to_battery_kwh;
        totals.grid_export_kwh += flows.export_kwh;
        totals.battery_to_load_kwh += flows.battery_to_load_kwh;
        totals.battery_to_grid_kwh += flows.battery_to_grid_kwh;
        totals.generation_origin_battery_to_load_kwh += origin_flows.generation_to_load_kwh;
        totals.grid_origin_battery_to_load_kwh += origin_flows.grid_to_load_kwh;
        totals.initial_origin_battery_to_load_kwh += origin_flows.initial_to_load_kwh;
        totals.generation_origin_battery_to_grid_kwh += origin_flows.generation_to_grid_kwh;
        totals.grid_origin_battery_to_grid_kwh += origin_flows.grid_to_grid_kwh;
        totals.initial_origin_battery_to_grid_kwh += origin_flows.initial_to_grid_kwh;
        totals.peak_import_kw = totals.peak_import_kw.max(flows.import_kwh / h);
        totals.peak_export_kw = totals.peak_export_kw.max(flows.export_kwh / h);
        totals.import_cost_usd += import_cost_usd;
        totals.export_credit_usd += export_credit_usd;

        // The bus balance, checked here rather than trusted.
        let residual = flows.generation_used_kwh + flows.import_kwh + flows.discharge_kwh
            - served_load_kwh
            - flows.export_kwh
            - flows.charge_kwh;
        totals.balance_residual_kwh += residual;
        if residual.abs() > BALANCE_TOLERANCE_KWH {
            return Err(Error::non_finite(
                format!("energy_balance_residual_kwh at interval {i}"),
                residual,
            ));
        }

        let stored_kwh = battery.as_ref().map_or(0.0, |b| b.stored_kwh);
        let soc_pct = battery.as_ref().map_or(0.0, Battery::soc_pct);
        if !scenario.outage.windows.is_empty() {
            soc_by_interval.push(soc_pct);
            minimum_soc_by_interval.push(interval_min_soc_pct);
        }

        if options.retain_timeline {
            timeline.push(IntervalRow {
                start_utc: grid.start_of(i),
                load_kwh,
                critical_load_kwh: critical_kwh,
                served_load_kwh,
                shed_noncritical_kwh: flows.shed_kwh,
                unmet_critical_kwh: flows.unmet_kwh,
                generation_available_kwh: generation_kwh,
                generation_used_kwh: flows.generation_used_kwh,
                generation_to_load_kwh: flows.generation_to_load_kwh,
                generation_to_battery_kwh: flows.generation_to_battery_kwh,
                generation_to_grid_kwh: flows.generation_to_grid_kwh,
                curtailed_kwh: flows.curtailed_kwh,
                battery_charge_kwh: flows.charge_kwh,
                battery_discharge_kwh: flows.discharge_kwh,
                stored_energy_kwh: stored_kwh,
                soc_pct,
                grid_import_kwh: flows.import_kwh,
                grid_to_load_kwh: flows.grid_to_load_kwh,
                grid_to_battery_kwh: flows.grid_to_battery_kwh,
                grid_export_kwh: flows.export_kwh,
                battery_to_load_kwh: flows.battery_to_load_kwh,
                battery_to_grid_kwh: flows.battery_to_grid_kwh,
                generation_origin_battery_to_load_kwh: origin_flows.generation_to_load_kwh,
                grid_origin_battery_to_load_kwh: origin_flows.grid_to_load_kwh,
                initial_origin_battery_to_load_kwh: origin_flows.initial_to_load_kwh,
                generation_origin_battery_to_grid_kwh: origin_flows.generation_to_grid_kwh,
                grid_origin_battery_to_grid_kwh: origin_flows.grid_to_grid_kwh,
                initial_origin_battery_to_grid_kwh: origin_flows.initial_to_grid_kwh,
                conversion_loss_kwh,
                self_discharge_loss_kwh,
                import_cost_usd,
                export_credit_usd,
                outage,
            });
        }
    }

    // ---- Summaries -------------------------------------------------------

    let final_stored_kwh = battery.as_ref().map_or(0.0, |b| b.stored_kwh);
    let usable_energy_kwh = battery.as_ref().map_or(0.0, Battery::usable_energy_kwh);
    let equivalent_full_cycles = if usable_energy_kwh > EPSILON_KWH {
        totals.battery_discharge_kwh / usable_energy_kwh
    } else {
        0.0
    };

    let self_consumption_pct = if totals.generation_available_kwh > EPSILON_KWH {
        ((totals.generation_to_load_kwh + totals.generation_to_battery_kwh
            - totals.generation_origin_battery_to_grid_kwh)
            / totals.generation_available_kwh
            * 100.0)
            .clamp(0.0, 100.0)
    } else {
        0.0
    };
    let self_sufficiency_pct = if totals.served_load_kwh > EPSILON_KWH {
        ((totals.generation_to_load_kwh + totals.generation_origin_battery_to_load_kwh)
            / totals.served_load_kwh
            * 100.0)
            .clamp(0.0, 100.0)
    } else {
        0.0
    };

    let energy = EnergySummary {
        interval_count: n,
        duration_hours: n as f64 * h,
        load_kwh: totals.load_kwh,
        served_load_kwh: totals.served_load_kwh,
        critical_load_kwh: totals.critical_load_kwh,
        unmet_critical_kwh: totals.unmet_critical_kwh,
        shed_noncritical_kwh: totals.shed_noncritical_kwh,
        generation_available_kwh: totals.generation_available_kwh,
        generation_used_kwh: totals.generation_used_kwh,
        generation_to_load_kwh: totals.generation_to_load_kwh,
        generation_to_battery_kwh: totals.generation_to_battery_kwh,
        generation_to_grid_kwh: totals.generation_to_grid_kwh,
        curtailed_kwh: totals.curtailed_kwh,
        battery_charge_kwh: totals.battery_charge_kwh,
        battery_discharge_kwh: totals.battery_discharge_kwh,
        conversion_loss_kwh: totals.conversion_loss_kwh,
        self_discharge_loss_kwh: totals.self_discharge_loss_kwh,
        grid_import_kwh: totals.grid_import_kwh,
        grid_to_load_kwh: totals.grid_to_load_kwh,
        grid_to_battery_kwh: totals.grid_to_battery_kwh,
        grid_export_kwh: totals.grid_export_kwh,
        battery_to_load_kwh: totals.battery_to_load_kwh,
        battery_to_grid_kwh: totals.battery_to_grid_kwh,
        generation_origin_battery_to_load_kwh: totals.generation_origin_battery_to_load_kwh,
        grid_origin_battery_to_load_kwh: totals.grid_origin_battery_to_load_kwh,
        initial_origin_battery_to_load_kwh: totals.initial_origin_battery_to_load_kwh,
        generation_origin_battery_to_grid_kwh: totals.generation_origin_battery_to_grid_kwh,
        grid_origin_battery_to_grid_kwh: totals.grid_origin_battery_to_grid_kwh,
        initial_origin_battery_to_grid_kwh: totals.initial_origin_battery_to_grid_kwh,
        peak_import_kw: totals.peak_import_kw,
        peak_export_kw: totals.peak_export_kw,
        initial_stored_energy_kwh: initial_stored_kwh,
        final_stored_energy_kwh: final_stored_kwh,
        min_soc_pct,
        max_soc_pct,
        self_consumption_pct,
        self_sufficiency_pct,
        equivalent_full_cycles,
        energy_balance_residual_kwh: totals.balance_residual_kwh,
    };

    let cost = demand.finish(&scenario.tariff, &months, &totals);
    let outages = replay_outages(
        scenario,
        input,
        &soc_by_interval,
        &minimum_soc_by_interval,
        &timeline,
        options,
    )?;

    // ---- The warning ledger ---------------------------------------------

    if clamped_setpoints > 0 {
        ledger.push(Warning::caution(
            "model.manual_setpoint_clamped",
            format!(
                "{clamped_setpoints} of {n} manual setpoints asked for more power or energy than the \
                 battery could deliver, and were clipped to what it can do. The schedule you wrote \
                 is not the schedule that ran."
            ),
        ));
    }
    if temperature_clamped {
        ledger.push(Warning::info(
            "model.temperature_outside_derate_table",
            "the temperature series goes outside the range of your derating table. Outside it, the \
             nearest table entry is held constant rather than extrapolated.",
        ));
    }
    if totals.curtailed_kwh > EPSILON_KWH {
        ledger.push(Warning::info(
            "model.generation_curtailed",
            format!(
                "{curtailed:.0} kWh of generation ({pct:.1}% of what was available) could not be \
                 used, stored, or exported, and was curtailed.",
                curtailed = totals.curtailed_kwh,
                pct = if totals.generation_available_kwh > EPSILON_KWH {
                    totals.curtailed_kwh / totals.generation_available_kwh * 100.0
                } else {
                    0.0
                }
            ),
        ));
    }
    if totals.unmet_critical_kwh > EPSILON_KWH {
        ledger.push(Warning::critical(
            "model.unmet_critical_load",
            format!(
                "{:.1} kWh of critical load went unserved during the modelled outages. This site \
                 does not survive them as configured.",
                totals.unmet_critical_kwh
            ),
        ));
    }
    if battery.is_some() && totals.battery_discharge_kwh <= EPSILON_KWH {
        ledger.push(Warning::caution(
            "model.battery_never_discharged",
            "the battery never discharged. Check the dispatch policy, the reserve floor, and the \
             initial state of charge: as configured, this battery does nothing.",
        ));
    }
    if grid_charging_enabled(&scenario.dispatch) {
        ledger.push(Warning::info(
            "model.grid_charging_enabled",
            "this scenario charges the battery from the grid. That raises imported energy and can \
             raise the demand charge.",
        ));
    }
    if totals.initial_origin_battery_to_load_kwh > EPSILON_KWH {
        ledger.push(Warning::info(
            "model.initial_energy_excluded_from_self_sufficiency",
            format!(
                "{:.1} kWh of load was served from energy present in the battery before the model \
                 began. Its source is unknown, so it is excluded from the self-sufficiency \
                 percentage.",
                totals.initial_origin_battery_to_load_kwh
            ),
        ));
    }
    if energy.energy_balance_residual_kwh.abs() > BALANCE_TOLERANCE_KWH {
        ledger.push(Warning::critical(
            "model.energy_balance_residual",
            format!(
                "the energy balance does not close: {:.6} kWh unaccounted. Do not use these results.",
                energy.energy_balance_residual_kwh
            ),
        ));
    }

    Ok(EngineResult {
        energy,
        cost,
        outages,
        warnings: ledger.into_sorted(),
        timeline,
    })
}

// ---------------------------------------------------------------------------
// Flows
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct Flows {
    charge_kwh: f64,
    discharge_kwh: f64,
    import_kwh: f64,
    export_kwh: f64,
    generation_used_kwh: f64,
    curtailed_kwh: f64,
    unmet_kwh: f64,
    shed_kwh: f64,
    generation_to_load_kwh: f64,
    generation_to_battery_kwh: f64,
    generation_to_grid_kwh: f64,
    grid_to_load_kwh: f64,
    grid_to_battery_kwh: f64,
    battery_to_load_kwh: f64,
    battery_to_grid_kwh: f64,
}

impl Flows {
    fn attribute(mut self, served_load_kwh: f64) -> Self {
        self.generation_to_load_kwh = self.generation_used_kwh.min(served_load_kwh).max(0.0);
        let remaining_load = (served_load_kwh - self.generation_to_load_kwh).max(0.0);
        self.battery_to_load_kwh = self.discharge_kwh.min(remaining_load).max(0.0);
        self.battery_to_grid_kwh = (self.discharge_kwh - self.battery_to_load_kwh).max(0.0);

        let remaining_generation =
            (self.generation_used_kwh - self.generation_to_load_kwh).max(0.0);
        self.generation_to_battery_kwh = self.charge_kwh.min(remaining_generation).max(0.0);
        self.generation_to_grid_kwh =
            (remaining_generation - self.generation_to_battery_kwh).max(0.0);
        self.grid_to_battery_kwh = (self.charge_kwh - self.generation_to_battery_kwh).max(0.0);
        self.grid_to_load_kwh = (remaining_load - self.battery_to_load_kwh).max(0.0);

        debug_assert!(
            (self.import_kwh - self.grid_to_load_kwh - self.grid_to_battery_kwh).abs()
                <= BALANCE_TOLERANCE_KWH
        );
        debug_assert!(
            (self.export_kwh - self.generation_to_grid_kwh - self.battery_to_grid_kwh).abs()
                <= BALANCE_TOLERANCE_KWH
        );
        self
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct BatteryOriginFlows {
    generation_to_load_kwh: f64,
    grid_to_load_kwh: f64,
    initial_to_load_kwh: f64,
    generation_to_grid_kwh: f64,
    grid_to_grid_kwh: f64,
    initial_to_grid_kwh: f64,
}

impl BatteryOriginFlows {
    fn from_discharge(
        removed: StoredOrigins,
        discharge_efficiency: f64,
        battery_to_load_kwh: f64,
        battery_to_grid_kwh: f64,
    ) -> Self {
        let total_output = battery_to_load_kwh + battery_to_grid_kwh;
        if total_output <= EPSILON_KWH {
            return Self::default();
        }
        let load_fraction = (battery_to_load_kwh / total_output).clamp(0.0, 1.0);
        let generation_output = removed.generation_kwh * discharge_efficiency;
        let grid_output = removed.grid_kwh * discharge_efficiency;
        let initial_output = removed.initial_kwh * discharge_efficiency;
        let result = Self {
            generation_to_load_kwh: generation_output * load_fraction,
            grid_to_load_kwh: grid_output * load_fraction,
            initial_to_load_kwh: initial_output * load_fraction,
            generation_to_grid_kwh: generation_output * (1.0 - load_fraction),
            grid_to_grid_kwh: grid_output * (1.0 - load_fraction),
            initial_to_grid_kwh: initial_output * (1.0 - load_fraction),
        };
        debug_assert!(
            (result.total() - total_output).abs() <= BALANCE_TOLERANCE_KWH,
            "battery origin allocation does not close"
        );
        result
    }

    fn total(self) -> f64 {
        self.generation_to_load_kwh
            + self.grid_to_load_kwh
            + self.initial_to_load_kwh
            + self.generation_to_grid_kwh
            + self.grid_to_grid_kwh
            + self.initial_to_grid_kwh
    }
}

/// Grid-connected. The bus absorbs whatever is left over.
fn grid_connected(
    net_kwh: f64,
    generation_kwh: f64,
    desired_kwh: f64,
    envelope: &Envelope,
    allow_battery_export: bool,
    export_limit_kwh: Option<f64>,
) -> Flows {
    // One signed decision. Charge and discharge cannot both be non-zero, because
    // they are two branches of the same `if`.
    let mut charge_kwh = 0.0;
    let mut discharge_kwh = 0.0;
    if desired_kwh > 0.0 {
        discharge_kwh = desired_kwh.min(envelope.max_discharge_kwh);
        if !allow_battery_export {
            // A rule-based policy never sends stored energy to the grid.
            discharge_kwh = discharge_kwh.min(net_kwh.max(0.0));
        }
    } else if desired_kwh < 0.0 {
        charge_kwh = (-desired_kwh).min(envelope.max_charge_kwh);
    }

    let mut generation_used_kwh = generation_kwh;
    let mut curtailed_kwh = 0.0;

    // What the grid must make up, positive importing.
    let mut grid_kwh = net_kwh + charge_kwh - discharge_kwh;

    if grid_kwh < 0.0 {
        if let Some(limit) = export_limit_kwh {
            let mut excess = -grid_kwh - limit;
            if excess > 0.0 {
                // Reduce discharge before curtailing: stored energy kept is worth
                // more than generation thrown away, and discharge is the flow we
                // actually control.
                let reduce = excess.min(discharge_kwh);
                discharge_kwh -= reduce;
                excess -= reduce;
                if excess > 0.0 {
                    let curtail = excess.min(generation_used_kwh);
                    generation_used_kwh -= curtail;
                    curtailed_kwh += curtail;
                    excess -= curtail;
                }
                grid_kwh = net_kwh + curtailed_kwh + charge_kwh - discharge_kwh;
                // `excess` can only remain positive if the export ceiling is below
                // what load alone forces onto the grid, which cannot happen: net
                // load is only negative because of generation, and generation has
                // just been curtailed to zero if needed.
                debug_assert!(
                    excess <= EPSILON_KWH,
                    "export limit could not be met: {excess}"
                );
            }
        }
    }

    let (import_kwh, export_kwh) = if grid_kwh >= 0.0 {
        (grid_kwh, 0.0)
    } else {
        (0.0, -grid_kwh)
    };

    Flows {
        charge_kwh,
        discharge_kwh,
        import_kwh,
        export_kwh,
        generation_used_kwh,
        curtailed_kwh,
        unmet_kwh: 0.0,
        shed_kwh: 0.0,
        ..Flows::default()
    }
    .attribute((net_kwh + generation_kwh).max(0.0))
}

/// Islanded. No grid, non-critical load shed, critical load served or not.
fn island(critical_kwh: f64, load_kwh: f64, generation_kwh: f64, envelope: &Envelope) -> Flows {
    let shed_kwh = (load_kwh - critical_kwh).max(0.0);
    let deficit = critical_kwh - generation_kwh;

    if deficit > 0.0 {
        let discharge_kwh = deficit.min(envelope.max_discharge_kwh);
        Flows {
            charge_kwh: 0.0,
            discharge_kwh,
            import_kwh: 0.0,
            export_kwh: 0.0,
            generation_used_kwh: generation_kwh,
            curtailed_kwh: 0.0,
            unmet_kwh: (deficit - discharge_kwh).max(0.0),
            shed_kwh,
            ..Flows::default()
        }
        .attribute((critical_kwh - (deficit - discharge_kwh).max(0.0)).max(0.0))
    } else {
        let surplus = -deficit;
        let charge_kwh = surplus.min(envelope.max_charge_kwh);
        let curtailed_kwh = surplus - charge_kwh;
        Flows {
            charge_kwh,
            discharge_kwh: 0.0,
            import_kwh: 0.0,
            export_kwh: 0.0,
            generation_used_kwh: generation_kwh - curtailed_kwh,
            curtailed_kwh,
            unmet_kwh: 0.0,
            shed_kwh,
            ..Flows::default()
        }
        .attribute(critical_kwh)
    }
}

fn clamp_signed(desired_kwh: f64, max_charge_kwh: f64, max_discharge_kwh: f64) -> f64 {
    if desired_kwh > 0.0 {
        desired_kwh.min(max_discharge_kwh)
    } else {
        -(-desired_kwh).min(max_charge_kwh)
    }
}

// ---------------------------------------------------------------------------
// Dispatch policies
// ---------------------------------------------------------------------------

/// What a policy is allowed to see.
///
/// Deliberately nothing about the future: no price forecast, no load forecast, no
/// look-ahead of any kind. That is what makes these dispatch *rules* rather than
/// an optimiser, and it is why Tonans never claims an optimal result. The power
/// ceilings are here so that a policy can ask for "as much charge as possible"
/// without inventing a sentinel value for it.
struct DispatchContext {
    net_load_kw: f64,
    soc_pct: f64,
    max_charge_kw: f64,
    max_discharge_kw: f64,
    /// AC charging power that would land exactly on this policy's SOC target.
    target_charge_kw: f64,
}

/// A pre-resolved manual schedule: interval index to setpoint.
struct ScheduleLookup {
    setpoints: Vec<f64>,
}

impl ScheduleLookup {
    fn build(policy: &DispatchPolicy, grid: &IntervalGrid) -> Option<Self> {
        let DispatchPolicy::ManualSchedule { entries } = policy else {
            return None;
        };
        let mut setpoints = vec![0.0; grid.count()];
        for entry in entries {
            // Alignment was checked in `Scenario::validate_against`; an entry that
            // does not resolve here has already been rejected, so a miss is simply
            // skipped rather than panicking in a release binary.
            let (Some(start), Some(end)) = (
                grid.boundary_index(entry.start_utc),
                grid.boundary_index(entry.end_utc),
            ) else {
                continue;
            };
            for slot in setpoints.iter_mut().take(end.min(grid.count())).skip(start) {
                *slot = entry.battery_power_kw;
            }
        }
        Some(Self { setpoints })
    }
}

/// The state-of-charge floor this policy will not discharge below while
/// grid-connected, on top of the battery's own hard minimum and reserve.
fn policy_floor_kwh(policy: &DispatchPolicy, battery: &Battery) -> f64 {
    match policy {
        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct,
            ..
        } => battery.reference_kwh * reserve_target_soc_pct / 100.0,
        _ => 0.0,
    }
}

fn grid_charging_enabled(policy: &DispatchPolicy) -> bool {
    matches!(
        policy,
        DispatchPolicy::PeakLimit {
            allow_grid_charge: true,
            ..
        } | DispatchPolicy::ReserveFirst {
            allow_grid_charge: true,
            ..
        }
    )
}

fn policy_target_charge_kw(policy: &DispatchPolicy, battery: &Battery, hours: f64) -> f64 {
    let target_pct = match policy {
        DispatchPolicy::PeakLimit {
            grid_charge_target_soc_pct,
            ..
        } => Some(*grid_charge_target_soc_pct),
        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct,
            ..
        } => Some(*reserve_target_soc_pct),
        _ => None,
    };
    target_pct.map_or(battery.max_charge_kw, |target| {
        let target_kwh = battery.reference_kwh * target / 100.0;
        ((target_kwh - battery.stored_kwh).max(0.0) / battery.charge_efficiency / hours).max(0.0)
    })
}

/// The signed setpoint a policy proposes, in kW. Positive discharges.
fn setpoint_kw(
    policy: &DispatchPolicy,
    context: &DispatchContext,
    schedule: Option<&ScheduleLookup>,
    interval: usize,
) -> f64 {
    match policy {
        // Store surplus, cover deficit. Nothing else.
        DispatchPolicy::SelfConsumption => context.net_load_kw,

        DispatchPolicy::PeakLimit {
            import_limit_kw,
            allow_grid_charge,
            grid_charge_target_soc_pct,
        } => {
            if context.net_load_kw > *import_limit_kw {
                // Shave down to the ceiling, no further.
                context.net_load_kw - import_limit_kw
            } else if context.net_load_kw < 0.0 {
                // Surplus generation. Store it.
                context.net_load_kw
            } else if *allow_grid_charge && context.soc_pct < *grid_charge_target_soc_pct {
                // Fill the battery inside the import headroom we already pay for,
                // so that it is charged when the peak arrives.
                -(import_limit_kw - context.net_load_kw)
                    .max(0.0)
                    .min(context.target_charge_kw)
                    .min(context.max_charge_kw)
            } else {
                0.0
            }
        }

        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct,
            allow_grid_charge,
        } => {
            if context.soc_pct < *reserve_target_soc_pct {
                // Below the reserve, the battery only ever charges. Refilling the
                // outage reserve outranks every saving.
                if *allow_grid_charge {
                    // Surplus generation may continue above the reserve as ordinary
                    // self-consumption, but grid energy stops exactly at the target.
                    let surplus_kw = (-context.net_load_kw).max(0.0).min(context.max_charge_kw);
                    -surplus_kw.max(context.target_charge_kw.min(context.max_charge_kw))
                } else if context.net_load_kw < 0.0 {
                    // Surplus generation only.
                    context.net_load_kw
                } else {
                    0.0
                }
            } else {
                // At or above the reserve: behave as self-consumption. The floor in
                // `policy_floor_kwh` is what stops it dropping back below the target.
                context.net_load_kw.min(context.max_discharge_kw)
            }
        }

        DispatchPolicy::ManualSchedule { .. } => schedule
            .and_then(|s| s.setpoints.get(interval).copied())
            .unwrap_or(0.0),
    }
}

// ---------------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Totals {
    load_kwh: f64,
    critical_load_kwh: f64,
    served_load_kwh: f64,
    shed_noncritical_kwh: f64,
    unmet_critical_kwh: f64,
    generation_available_kwh: f64,
    generation_used_kwh: f64,
    generation_to_load_kwh: f64,
    generation_to_battery_kwh: f64,
    generation_to_grid_kwh: f64,
    curtailed_kwh: f64,
    battery_charge_kwh: f64,
    battery_discharge_kwh: f64,
    conversion_loss_kwh: f64,
    self_discharge_loss_kwh: f64,
    grid_import_kwh: f64,
    grid_to_load_kwh: f64,
    grid_to_battery_kwh: f64,
    grid_export_kwh: f64,
    battery_to_load_kwh: f64,
    battery_to_grid_kwh: f64,
    generation_origin_battery_to_load_kwh: f64,
    grid_origin_battery_to_load_kwh: f64,
    initial_origin_battery_to_load_kwh: f64,
    generation_origin_battery_to_grid_kwh: f64,
    grid_origin_battery_to_grid_kwh: f64,
    initial_origin_battery_to_grid_kwh: f64,
    peak_import_kw: f64,
    peak_export_kw: f64,
    import_cost_usd: f64,
    export_credit_usd: f64,
    balance_residual_kwh: f64,
}

#[derive(Debug, Default)]
struct MonthTracker {
    months: Vec<(i32, u32)>,
}

impl MonthTracker {
    fn observe(&mut self, year: i32, month: u32) {
        if self.months.last() != Some(&(year, month)) && !self.months.contains(&(year, month)) {
            self.months.push((year, month));
        }
    }
}

/// Monthly peak import per demand-charge period.
struct DemandTracker {
    labels: Vec<String>,
    /// `(year, month, period index) -> peak kW`, kept sorted so output order is
    /// calendar order and never depends on a hash.
    peaks: Vec<((i32, u32, usize), f64)>,
}

impl DemandTracker {
    fn new(labels: &[String]) -> Self {
        Self {
            labels: if labels.is_empty() {
                vec![TariffSpec::DEFAULT_DEMAND_PERIOD.to_string()]
            } else {
                labels.to_vec()
            },
            peaks: Vec::new(),
        }
    }

    fn observe(&mut self, year: i32, month: u32, period: usize, power_kw: f64) {
        let key = (year, month, period);
        match self.peaks.binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(index) => {
                let entry = &mut self.peaks[index].1;
                *entry = entry.max(power_kw);
            }
            Err(index) => self.peaks.insert(index, (key, power_kw)),
        }
    }

    fn finish(self, tariff: &TariffSpec, months: &MonthTracker, totals: &Totals) -> CostSummary {
        let mut lines = Vec::new();
        let mut demand_charge_usd = 0.0;

        for ((year, month, period), peak_kw) in self.peaks {
            let label = self
                .labels
                .get(period)
                .cloned()
                .unwrap_or_else(|| TariffSpec::DEFAULT_DEMAND_PERIOD.to_string());
            let Some(price) = tariff.demand_prices_per_kw_month_usd.get(&label) else {
                // Validation guarantees a price for every period the data uses, so
                // this is unreachable for a validated project. Skipping rather than
                // charging zero keeps a mis-built call from inventing a free month.
                continue;
            };
            let charge_usd = peak_kw * price;
            demand_charge_usd += charge_usd;
            lines.push(DemandChargeLine {
                year,
                month,
                period: label,
                peak_import_kw: peak_kw,
                price_per_kw_month_usd: *price,
                charge_usd,
            });
        }

        let billed_month_count = months.months.len();
        let fixed_charge_usd = tariff.fixed_charge_usd_per_month * billed_month_count as f64;
        let total_grid_cost_usd = totals.import_cost_usd - totals.export_credit_usd
            + demand_charge_usd
            + fixed_charge_usd;

        CostSummary {
            import_energy_cost_usd: totals.import_cost_usd,
            export_credit_usd: totals.export_credit_usd,
            demand_charge_usd,
            fixed_charge_usd,
            total_grid_cost_usd,
            billed_month_count,
            demand_charge_lines: lines,
        }
    }
}

// ---------------------------------------------------------------------------
// Outage replay
// ---------------------------------------------------------------------------

fn replay_outages(
    scenario: &Scenario,
    input: &EngineInput<'_>,
    soc_by_interval: &[f64],
    minimum_soc_by_interval: &[f64],
    timeline: &[IntervalRow],
    options: EngineOptions,
) -> Result<Vec<OutageResult>> {
    if scenario.outage.windows.is_empty() {
        return Ok(Vec::new());
    }
    if !options.retain_timeline {
        // A sensitivity sweep does not keep the timeline, so per-window detail is
        // not available. Its cells report totals only, and say so.
        return Ok(Vec::new());
    }

    let grid = input.grid;
    let h = grid.hours();
    let soc_min_pct = scenario.battery.as_ref().map_or(0.0, |b| b.soc_min_pct);
    let mut results = Vec::with_capacity(scenario.outage.windows.len());

    for window in &scenario.outage.windows {
        let (Some(start), Some(end)) = (
            grid.boundary_index(window.start_utc),
            grid.boundary_index(window.end_utc),
        ) else {
            // Alignment is validated before the run; an unaligned window cannot
            // reach here.
            continue;
        };

        let mut critical_load_kwh = 0.0;
        let mut unmet_critical_kwh = 0.0;
        let mut shed_noncritical_kwh = 0.0;
        let mut generation_used_kwh = 0.0;
        let mut min_soc_pct = f64::INFINITY;
        let mut first_unmet_utc = None;
        let mut survival_intervals = 0usize;
        let mut still_serving = true;

        for (offset, row) in timeline[start..end.min(timeline.len())].iter().enumerate() {
            critical_load_kwh += row.critical_load_kwh;
            unmet_critical_kwh += row.unmet_critical_kwh;
            shed_noncritical_kwh += row.shed_noncritical_kwh;
            generation_used_kwh += row.generation_used_kwh;
            min_soc_pct = min_soc_pct.min(
                minimum_soc_by_interval
                    .get(start + offset)
                    .copied()
                    .unwrap_or(row.soc_pct),
            );

            if row.unmet_critical_kwh > EPSILON_KWH {
                if first_unmet_utc.is_none() {
                    first_unmet_utc = Some(grid.start_of(start + offset));
                }
                still_serving = false;
            }
            if still_serving {
                survival_intervals += 1;
            }
        }
        if min_soc_pct.is_infinite() {
            min_soc_pct = soc_by_interval.get(start).copied().unwrap_or(0.0);
        }

        let soc_at_start_pct = soc_by_interval.get(start).copied().unwrap_or(0.0);
        results.push(OutageResult {
            start_utc: window.start_utc,
            end_utc: window.end_utc,
            label: window.label.clone(),
            duration_hours: (end - start) as f64 * h,
            critical_load_kwh,
            critical_served_kwh: finite(
                "critical_served_kwh",
                critical_load_kwh - unmet_critical_kwh,
            )?,
            unmet_critical_kwh,
            shed_noncritical_kwh,
            generation_used_kwh,
            soc_at_start_pct,
            min_soc_pct,
            survived: unmet_critical_kwh <= EPSILON_KWH,
            survival_hours: survival_intervals as f64 * h,
            first_unmet_utc,
            // "Depleted" means the battery hit its hard floor, not its reserve: the
            // reserve is what the outage is allowed to eat.
            reserve_depleted: min_soc_pct <= soc_min_pct + 1e-6 && scenario.battery.is_some(),
        });
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Resolving a project into engine input
// ---------------------------------------------------------------------------

/// The series a run needs, materialised from the project and the scenario.
///
/// Owns its buffers so that [`EngineInput`] can borrow them for the run.
#[derive(Debug, Clone)]
pub struct ResolvedInput {
    /// Load, kW.
    pub load_kw: Vec<f64>,
    /// Critical load, kW.
    pub critical_load_kw: Vec<f64>,
    /// Generation, kW.
    pub generation_kw: Vec<f64>,
    /// Import price after multiplier.
    pub import_price_per_kwh_usd: Vec<f64>,
    /// Export price after multiplier.
    pub export_price_per_kwh_usd: Vec<f64>,
    /// Temperature, if imported.
    pub temperature_c: Option<Vec<f64>>,
    /// Demand-charge period per interval.
    pub demand_period_index: Vec<u16>,
    /// The distinct period labels.
    pub demand_labels: Vec<String>,
    /// Outage mask.
    pub outage: Vec<bool>,
}

impl ResolvedInput {
    /// Materialise the series one scenario needs.
    pub fn resolve(
        project: &crate::schema::Project,
        scenario: &Scenario,
        grid: &IntervalGrid,
    ) -> Result<Self> {
        let dataset = project.dataset.as_ref().ok_or_else(|| {
            Error::validation(
                "validation.no_dataset",
                "project.dataset",
                "no interval data has been imported",
            )
        })?;
        let n = grid.count();

        let price = |source: &RateSource, kind: SeriesKind, multiplier: f64| -> Vec<f64> {
            let base = match source {
                RateSource::Flat { price_per_kwh_usd } => vec![*price_per_kwh_usd; n],
                RateSource::Series => dataset.values_or_zero(kind),
            };
            base.into_iter().map(|p| p * multiplier).collect()
        };

        let mut outage = vec![false; n];
        for window in &scenario.outage.windows {
            let (Some(start), Some(end)) = (
                grid.boundary_index(window.start_utc),
                grid.boundary_index(window.end_utc),
            ) else {
                return Err(Error::validation(
                    "validation.outage_not_aligned",
                    format!("scenario[{}].outage", scenario.id),
                    "an outage window does not align with the interval grid",
                ));
            };
            for slot in outage.iter_mut().take(end.min(n)).skip(start) {
                *slot = true;
            }
        }

        let (demand_labels, demand_period_index) = match &dataset.demand_periods {
            Some(periods) => (periods.labels.clone(), periods.index.clone()),
            None => (
                vec![TariffSpec::DEFAULT_DEMAND_PERIOD.to_string()],
                vec![0u16; n],
            ),
        };

        Ok(Self {
            load_kw: dataset.values_or_zero(SeriesKind::Load),
            critical_load_kw: dataset.values_or_zero(SeriesKind::CriticalLoad),
            generation_kw: dataset.values_or_zero(SeriesKind::Generation),
            import_price_per_kwh_usd: price(
                &scenario.tariff.import,
                SeriesKind::ImportPrice,
                scenario.tariff.import_price_multiplier,
            ),
            export_price_per_kwh_usd: price(
                &scenario.tariff.export,
                SeriesKind::ExportPrice,
                scenario.tariff.export_price_multiplier,
            ),
            temperature_c: dataset
                .series
                .get(&SeriesKind::Temperature)
                .map(|s| s.values.clone()),
            demand_period_index,
            demand_labels,
            outage,
        })
    }

    /// Borrow this as engine input.
    #[must_use]
    pub fn as_input<'a>(&'a self, grid: &'a IntervalGrid) -> EngineInput<'a> {
        EngineInput {
            grid,
            load_kw: &self.load_kw,
            critical_load_kw: &self.critical_load_kw,
            generation_kw: &self.generation_kw,
            import_price_per_kwh_usd: &self.import_price_per_kwh_usd,
            export_price_per_kwh_usd: &self.export_price_per_kwh_usd,
            temperature_c: self.temperature_c.as_deref(),
            demand_period_index: &self.demand_period_index,
            demand_labels: &self.demand_labels,
            outage: &self.outage,
        }
    }
}

/// Advisories about the data set that only matter once a run is on the table.
#[must_use]
pub fn dataset_advisories(dataset: &crate::schema::Dataset) -> Vec<Warning> {
    let mut warnings = Vec::new();
    if !matches!(dataset.gap_policy, GapPolicy::Reject) && dataset.synthesized_interval_count > 0 {
        warnings.push(Warning::critical(
            "data.results_depend_on_synthesized_intervals",
            format!(
                "{} interval(s) in this data set were synthesized, not measured. Every result \
                 derived from them inherits that.",
                dataset.synthesized_interval_count
            ),
        ));
    }
    warnings
}
