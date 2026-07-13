//! The invariants the model may never break.
//!
//! `PRODUCT-SPEC.md` names six: interval energy conservation, bounded state of
//! charge, no simultaneous charge and discharge, nonnegative unmet load,
//! deterministic repeat output, and consistent money totals. `engine`'s module
//! documentation names a few more, and promises they hold "per interval and over
//! the whole run … over thousands of pseudo-random projects". This file is where
//! both of those promises are kept.
//!
//! Each invariant is one function. Every function is asserted twice: once against a
//! realistic two-week project with load, generation, a time-of-use tariff, a demand
//! charge over two billed months, and an outage the site does not survive; and once
//! against two thousand pseudo-random scenarios drawn from a fixed seed, which is
//! how a case nobody thought to write down gets caught.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::{DateTime, NaiveDate, TimeDelta, TimeZone, Utc};
use tonans_core::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
use tonans_core::engine::{
    self, CostSummary, EnergySummary, EngineOptions, IntervalRow, ResolvedInput,
};
use tonans_core::licensing::Entitlements;
use tonans_core::run::{self, RunResult};
use tonans_core::schema::{
    BatterySpec, DegradationSpec, DispatchPolicy, EconomicsSpec, GapPolicy, InverterSpec,
    OutageSpec, OutageWindow, Project, RateSource, ReplacementSpec, Scenario, ScheduleEntry,
    SeriesKind, Site, TariffSpec, TemperatureDerate, TemperatureDeratePoint,
};

/// The residual the engine itself is willing to attribute to floating point. Every
/// tolerance here is the same one, so this suite cannot be looser than the model.
const TOLERANCE_KWH: f64 = 1e-6;

/// State of charge is a percentage of a fixed reference; its error is of the same
/// order as the energy it is computed from.
const TOLERANCE_PCT: f64 = 1e-6;

/// One cent. Money is accumulated at full precision and rounded once, at the
/// boundary where it becomes an output, so the identities below must close to well
/// inside the smallest unit a reader can see.
const TOLERANCE_USD: f64 = 0.01;

// ---------------------------------------------------------------------------
// One scenario's output, as the invariants need it
// ---------------------------------------------------------------------------

struct Simulated<'a> {
    /// Names the case in every failure message. A sweep failure is worthless if it
    /// cannot be reproduced.
    context: String,
    scenario: &'a Scenario,
    timeline: &'a [IntervalRow],
    energy: &'a EnergySummary,
    cost: &'a CostSummary,
}

fn sum(timeline: &[IntervalRow], of: impl Fn(&IntervalRow) -> f64) -> f64 {
    timeline.iter().map(of).sum()
}

/// The state-of-charge floor a dispatch policy holds on top of the battery's own
/// hard minimum and reserve.
fn policy_floor_pct(dispatch: &DispatchPolicy) -> f64 {
    match dispatch {
        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct,
            ..
        } => *reserve_target_soc_pct,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Invariant 1: interval energy conservation
// ---------------------------------------------------------------------------

/// `generation_used + import + discharge = served_load + export + charge`, every
/// interval; the run's published residual is zero; and every total is the sum of
/// the intervals it claims to summarize.
fn conservation(sim: &Simulated<'_>) {
    let context = &sim.context;

    for (index, row) in sim.timeline.iter().enumerate() {
        let into_bus = row.generation_used_kwh + row.grid_import_kwh + row.battery_discharge_kwh;
        let out_of_bus = row.served_load_kwh + row.grid_export_kwh + row.battery_charge_kwh;
        assert!(
            (into_bus - out_of_bus).abs() <= TOLERANCE_KWH,
            "{context}: interval {index} does not balance by {residual} kWh. \
             In: generation {generation}, import {import}, discharge {discharge}. \
             Out: served load {served}, export {export}, charge {charge}.",
            residual = into_bus - out_of_bus,
            generation = row.generation_used_kwh,
            import = row.grid_import_kwh,
            discharge = row.battery_discharge_kwh,
            served = row.served_load_kwh,
            export = row.grid_export_kwh,
            charge = row.battery_charge_kwh,
        );

        // Load presented to the bus is served, shed, or unmet. There is no fourth
        // place for it to go.
        let accounted = row.served_load_kwh + row.shed_noncritical_kwh + row.unmet_critical_kwh;
        assert!(
            (row.load_kwh - accounted).abs() <= TOLERANCE_KWH,
            "{context}: interval {index} presents {load} kWh of load but accounts for \
             {accounted} kWh (served {served}, shed {shed}, unmet {unmet})",
            load = row.load_kwh,
            served = row.served_load_kwh,
            shed = row.shed_noncritical_kwh,
            unmet = row.unmet_critical_kwh,
        );

        // Generation available is used or curtailed, and nothing else.
        let generation = row.generation_used_kwh + row.curtailed_kwh;
        assert!(
            (row.generation_available_kwh - generation).abs() <= TOLERANCE_KWH,
            "{context}: interval {index} had {available} kWh of generation but used \
             {used} and curtailed {curtailed}",
            available = row.generation_available_kwh,
            used = row.generation_used_kwh,
            curtailed = row.curtailed_kwh,
        );

        for (name, whole, parts) in [
            (
                "generation_used",
                row.generation_used_kwh,
                row.generation_to_load_kwh
                    + row.generation_to_battery_kwh
                    + row.generation_to_grid_kwh,
            ),
            (
                "grid_import",
                row.grid_import_kwh,
                row.grid_to_load_kwh + row.grid_to_battery_kwh,
            ),
            (
                "grid_export",
                row.grid_export_kwh,
                row.generation_to_grid_kwh + row.battery_to_grid_kwh,
            ),
            (
                "battery_discharge",
                row.battery_discharge_kwh,
                row.battery_to_load_kwh + row.battery_to_grid_kwh,
            ),
            (
                "battery_to_load origins",
                row.battery_to_load_kwh,
                row.generation_origin_battery_to_load_kwh
                    + row.grid_origin_battery_to_load_kwh
                    + row.initial_origin_battery_to_load_kwh,
            ),
            (
                "battery_to_grid origins",
                row.battery_to_grid_kwh,
                row.generation_origin_battery_to_grid_kwh
                    + row.grid_origin_battery_to_grid_kwh
                    + row.initial_origin_battery_to_grid_kwh,
            ),
        ] {
            assert!(
                (whole - parts).abs() <= TOLERANCE_KWH,
                "{context}: interval {index} {name} is {whole} kWh but its source-flow parts total {parts} kWh"
            );
        }
    }

    assert!(
        sim.energy.energy_balance_residual_kwh.abs() <= TOLERANCE_KWH,
        "{context}: the run's own energy balance leaves {residual} kWh unaccounted. \
         Nothing else in this result is trustworthy.",
        residual = sim.energy.energy_balance_residual_kwh,
    );

    for (name, total, summed) in [
        (
            "load_kwh",
            sim.energy.load_kwh,
            sum(sim.timeline, |r| r.load_kwh),
        ),
        (
            "served_load_kwh",
            sim.energy.served_load_kwh,
            sum(sim.timeline, |r| r.served_load_kwh),
        ),
        (
            "unmet_critical_kwh",
            sim.energy.unmet_critical_kwh,
            sum(sim.timeline, |r| r.unmet_critical_kwh),
        ),
        (
            "shed_noncritical_kwh",
            sim.energy.shed_noncritical_kwh,
            sum(sim.timeline, |r| r.shed_noncritical_kwh),
        ),
        (
            "grid_import_kwh",
            sim.energy.grid_import_kwh,
            sum(sim.timeline, |r| r.grid_import_kwh),
        ),
        (
            "grid_export_kwh",
            sim.energy.grid_export_kwh,
            sum(sim.timeline, |r| r.grid_export_kwh),
        ),
        (
            "generation_to_load_kwh",
            sim.energy.generation_to_load_kwh,
            sum(sim.timeline, |r| r.generation_to_load_kwh),
        ),
        (
            "generation_to_battery_kwh",
            sim.energy.generation_to_battery_kwh,
            sum(sim.timeline, |r| r.generation_to_battery_kwh),
        ),
        (
            "grid_to_load_kwh",
            sim.energy.grid_to_load_kwh,
            sum(sim.timeline, |r| r.grid_to_load_kwh),
        ),
        (
            "grid_to_battery_kwh",
            sim.energy.grid_to_battery_kwh,
            sum(sim.timeline, |r| r.grid_to_battery_kwh),
        ),
        (
            "battery_to_load_kwh",
            sim.energy.battery_to_load_kwh,
            sum(sim.timeline, |r| r.battery_to_load_kwh),
        ),
        (
            "battery_charge_kwh",
            sim.energy.battery_charge_kwh,
            sum(sim.timeline, |r| r.battery_charge_kwh),
        ),
        (
            "battery_discharge_kwh",
            sim.energy.battery_discharge_kwh,
            sum(sim.timeline, |r| r.battery_discharge_kwh),
        ),
        (
            "curtailed_kwh",
            sim.energy.curtailed_kwh,
            sum(sim.timeline, |r| r.curtailed_kwh),
        ),
        (
            "conversion_loss_kwh",
            sim.energy.conversion_loss_kwh,
            sum(sim.timeline, |r| r.conversion_loss_kwh),
        ),
        (
            "self_discharge_loss_kwh",
            sim.energy.self_discharge_loss_kwh,
            sum(sim.timeline, |r| r.self_discharge_loss_kwh),
        ),
    ] {
        assert!(
            (total - summed).abs() <= TOLERANCE_KWH,
            "{context}: the run reports {total} kWh of {name}, but its intervals add \
             up to {summed} kWh"
        );
    }
}

// ---------------------------------------------------------------------------
// Invariant 2: bounded state of charge
// ---------------------------------------------------------------------------

/// `0 <= soc <= 100`; the battery never rises above its usable ceiling; it is never
/// *discharged* below its floor; and stored energy moves by exactly the asymmetric
/// efficiencies and the standing loss.
fn state_of_charge(sim: &Simulated<'_>) {
    let context = &sim.context;

    let Some(battery) = sim.scenario.battery.as_ref() else {
        for (index, row) in sim.timeline.iter().enumerate() {
            assert!(
                row.soc_pct.abs() < TOLERANCE_PCT && row.stored_energy_kwh.abs() < TOLERANCE_KWH,
                "{context}: interval {index} has no battery but reports {soc}% state of \
                 charge and {stored} kWh stored",
                soc = row.soc_pct,
                stored = row.stored_energy_kwh,
            );
        }
        return;
    };

    let reference_kwh = battery.nameplate_energy_kwh * sim.scenario.initial_capacity_pct / 100.0;
    let charge_efficiency = battery.charge_efficiency_pct / 100.0;
    let discharge_efficiency = battery.discharge_efficiency_pct / 100.0;
    let mut stored_kwh = sim.energy.initial_stored_energy_kwh;

    for (index, row) in sim.timeline.iter().enumerate() {
        assert!(
            (0.0..=100.0).contains(&row.soc_pct),
            "{context}: interval {index} is at {soc}% state of charge, outside 0 to 100",
            soc = row.soc_pct,
        );
        assert!(
            row.soc_pct <= battery.soc_max_pct + TOLERANCE_PCT,
            "{context}: interval {index} is at {soc}%, above the battery's usable \
             ceiling of {ceiling}%",
            soc = row.soc_pct,
            ceiling = battery.soc_max_pct,
        );
        assert!(
            row.stored_energy_kwh >= -TOLERANCE_KWH,
            "{context}: interval {index} stores {stored} kWh, which is negative",
            stored = row.stored_energy_kwh,
        );

        // The floor dispatch may not cross. During an outage it is the hard minimum:
        // the reserve exists for precisely that moment. Otherwise it is the highest
        // of the hard minimum, the reserve, and any floor the policy holds.
        //
        // The bound is stated on *discharge* rather than on state of charge itself,
        // because standing loss is taken off stored energy before the envelope is
        // computed: a battery already sitting on its floor drifts below it, and no
        // dispatch decision put it there. What the model guarantees — and what
        // matters to a reader trusting the reserve — is that nothing the battery is
        // *asked* to do ever takes it below the floor.
        if row.battery_discharge_kwh > 0.0 {
            let floor_pct = if row.outage {
                battery.soc_min_pct
            } else {
                battery
                    .soc_min_pct
                    .max(battery.reserve_soc_pct)
                    .max(policy_floor_pct(&sim.scenario.dispatch))
            };
            assert!(
                row.soc_pct >= floor_pct - TOLERANCE_PCT,
                "{context}: interval {index} discharged {discharge} kWh and left the \
                 battery at {soc}%, below its {floor}% floor",
                discharge = row.battery_discharge_kwh,
                soc = row.soc_pct,
                floor = floor_pct,
            );
        }

        let expected_kwh = stored_kwh - row.self_discharge_loss_kwh
            + row.battery_charge_kwh * charge_efficiency
            - row.battery_discharge_kwh / discharge_efficiency;
        assert!(
            (row.stored_energy_kwh - expected_kwh).abs() <= TOLERANCE_KWH,
            "{context}: interval {index} stores {stored} kWh, but {previous} kWh less \
             {loss} standing loss plus {charge} charged at {charge_efficiency} less \
             {discharge} discharged at {discharge_efficiency} is {expected_kwh} kWh",
            stored = row.stored_energy_kwh,
            previous = stored_kwh,
            loss = row.self_discharge_loss_kwh,
            charge = row.battery_charge_kwh,
            discharge = row.battery_discharge_kwh,
        );

        // Conversion loss is the whole of what the round trip did not return, and it
        // lives in the battery's two efficiencies — never split with the inverter.
        let expected_loss_kwh = row.battery_charge_kwh * (1.0 - charge_efficiency)
            + row.battery_discharge_kwh * (1.0 / discharge_efficiency - 1.0);
        assert!(
            (row.conversion_loss_kwh - expected_loss_kwh).abs() <= TOLERANCE_KWH,
            "{context}: interval {index} reports {loss} kWh of conversion loss where the \
             efficiencies imply {expected_loss_kwh} kWh",
            loss = row.conversion_loss_kwh,
        );

        // State of charge is measured against a fixed reference, so a derating table
        // can shrink the headroom without silently rebasing the percentage.
        assert!(
            (row.soc_pct - row.stored_energy_kwh / reference_kwh * 100.0).abs() <= TOLERANCE_PCT,
            "{context}: interval {index} reports {soc}% but stores {stored} kWh against \
             a {reference_kwh} kWh reference",
            soc = row.soc_pct,
            stored = row.stored_energy_kwh,
        );

        assert!(
            sim.energy.min_soc_pct <= row.soc_pct + TOLERANCE_PCT
                && sim.energy.max_soc_pct >= row.soc_pct - TOLERANCE_PCT,
            "{context}: interval {index} is at {soc}%, outside the run's reported range \
             of {min}% to {max}%",
            soc = row.soc_pct,
            min = sim.energy.min_soc_pct,
            max = sim.energy.max_soc_pct,
        );

        stored_kwh = row.stored_energy_kwh;
    }

    assert!(
        (sim.energy.final_stored_energy_kwh - stored_kwh).abs() <= TOLERANCE_KWH,
        "{context}: the run ends holding {final_kwh} kWh but its last interval stored \
         {stored_kwh} kWh",
        final_kwh = sim.energy.final_stored_energy_kwh,
    );
    assert!(
        sim.energy.min_soc_pct >= 0.0
            && sim.energy.max_soc_pct <= battery.soc_max_pct + TOLERANCE_PCT,
        "{context}: the run's state of charge ranged {min}% to {max}%, outside 0 to the \
         battery's {ceiling}% ceiling",
        min = sim.energy.min_soc_pct,
        max = sim.energy.max_soc_pct,
        ceiling = battery.soc_max_pct,
    );
}

// ---------------------------------------------------------------------------
// Invariant 3: no simultaneous charge and discharge
// ---------------------------------------------------------------------------

/// The battery never charges and discharges in the same interval, and the site never
/// imports and exports in the same interval. Both are exact: they are two branches
/// of one signed decision, so there is no rounding to forgive.
fn exclusivity(sim: &Simulated<'_>) {
    let context = &sim.context;
    for (index, row) in sim.timeline.iter().enumerate() {
        assert!(
            !(row.battery_charge_kwh > 0.0 && row.battery_discharge_kwh > 0.0),
            "{context}: interval {index} charges {charge} kWh and discharges {discharge} \
             kWh at the same time",
            charge = row.battery_charge_kwh,
            discharge = row.battery_discharge_kwh,
        );
        assert!(
            !(row.grid_import_kwh > 0.0 && row.grid_export_kwh > 0.0),
            "{context}: interval {index} imports {import} kWh and exports {export} kWh \
             at the same time",
            import = row.grid_import_kwh,
            export = row.grid_export_kwh,
        );
    }
}

// ---------------------------------------------------------------------------
// Invariant 4: nonnegative unmet load
// ---------------------------------------------------------------------------

/// Nothing the model reports as a quantity of energy is ever negative, unmet load
/// never exceeds the critical load it is a shortfall of, and curtailment never
/// exceeds the generation it is thrown away from.
fn nonnegativity(sim: &Simulated<'_>) {
    let context = &sim.context;
    for (index, row) in sim.timeline.iter().enumerate() {
        for (name, value) in [
            ("unmet_critical_kwh", row.unmet_critical_kwh),
            ("shed_noncritical_kwh", row.shed_noncritical_kwh),
            ("curtailed_kwh", row.curtailed_kwh),
            ("served_load_kwh", row.served_load_kwh),
            ("generation_used_kwh", row.generation_used_kwh),
            ("grid_import_kwh", row.grid_import_kwh),
            ("grid_export_kwh", row.grid_export_kwh),
            ("battery_charge_kwh", row.battery_charge_kwh),
            ("battery_discharge_kwh", row.battery_discharge_kwh),
            ("conversion_loss_kwh", row.conversion_loss_kwh),
            ("self_discharge_loss_kwh", row.self_discharge_loss_kwh),
        ] {
            assert!(
                value >= -TOLERANCE_KWH,
                "{context}: interval {index} reports {value} kWh of {name}, which is negative"
            );
        }

        assert!(
            row.unmet_critical_kwh <= row.critical_load_kwh + TOLERANCE_KWH,
            "{context}: interval {index} leaves {unmet} kWh of critical load unserved out \
             of {critical} kWh",
            unmet = row.unmet_critical_kwh,
            critical = row.critical_load_kwh,
        );
        assert!(
            row.curtailed_kwh <= row.generation_available_kwh + TOLERANCE_KWH,
            "{context}: interval {index} curtails {curtailed} kWh of {available} kWh \
             available",
            curtailed = row.curtailed_kwh,
            available = row.generation_available_kwh,
        );
    }

    for (name, value) in [
        ("unmet_critical_kwh", sim.energy.unmet_critical_kwh),
        ("shed_noncritical_kwh", sim.energy.shed_noncritical_kwh),
        ("curtailed_kwh", sim.energy.curtailed_kwh),
        ("conversion_loss_kwh", sim.energy.conversion_loss_kwh),
        (
            "self_discharge_loss_kwh",
            sim.energy.self_discharge_loss_kwh,
        ),
        ("peak_import_kw", sim.energy.peak_import_kw),
        ("peak_export_kw", sim.energy.peak_export_kw),
    ] {
        assert!(
            value >= -TOLERANCE_KWH,
            "{context}: the run reports {value} of {name}, which is negative"
        );
    }
}

// ---------------------------------------------------------------------------
// Invariant 5 (engine, not the spec's six): the grid is gone during an outage
// ---------------------------------------------------------------------------

/// During an outage there is no import and no export, non-critical load is shed in
/// full, and outside one there is nothing shed and nothing unmet.
fn islanding(sim: &Simulated<'_>) {
    let context = &sim.context;
    for (index, row) in sim.timeline.iter().enumerate() {
        if row.outage {
            assert!(
                row.grid_import_kwh == 0.0 && row.grid_export_kwh == 0.0,
                "{context}: interval {index} is inside an outage but imports {import} kWh \
                 and exports {export} kWh",
                import = row.grid_import_kwh,
                export = row.grid_export_kwh,
            );
            assert!(
                (row.shed_noncritical_kwh - (row.load_kwh - row.critical_load_kwh)).abs()
                    <= TOLERANCE_KWH,
                "{context}: interval {index} is inside an outage with {load} kWh of load \
                 and {critical} kWh critical, but sheds {shed} kWh",
                load = row.load_kwh,
                critical = row.critical_load_kwh,
                shed = row.shed_noncritical_kwh,
            );
        } else {
            assert!(
                row.unmet_critical_kwh == 0.0 && row.shed_noncritical_kwh == 0.0,
                "{context}: interval {index} is grid-connected but leaves {unmet} kWh \
                 unmet and sheds {shed} kWh",
                unmet = row.unmet_critical_kwh,
                shed = row.shed_noncritical_kwh,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant 6: consistent money totals
// ---------------------------------------------------------------------------

/// Every money figure is the sum of the figures beneath it, to the cent.
fn money(sim: &Simulated<'_>) {
    let context = &sim.context;
    let cost = sim.cost;

    let components = cost.import_energy_cost_usd - cost.export_credit_usd
        + cost.demand_charge_usd
        + cost.fixed_charge_usd;
    assert!(
        (cost.total_grid_cost_usd - components).abs() <= TOLERANCE_USD,
        "{context}: the grid cost does not add up. Import {import} - export credit \
         {credit} + demand {demand} + fixed {fixed} = {components}, but the total says \
         {total}.",
        import = cost.import_energy_cost_usd,
        credit = cost.export_credit_usd,
        demand = cost.demand_charge_usd,
        fixed = cost.fixed_charge_usd,
        total = cost.total_grid_cost_usd,
    );

    let import_cost_usd = sum(sim.timeline, |r| r.import_cost_usd);
    assert!(
        (cost.import_energy_cost_usd - import_cost_usd).abs() <= TOLERANCE_USD,
        "{context}: the run charges {charged} for imported energy, but its intervals add \
         up to {import_cost_usd}",
        charged = cost.import_energy_cost_usd,
    );

    let export_credit_usd = sum(sim.timeline, |r| r.export_credit_usd);
    assert!(
        (cost.export_credit_usd - export_credit_usd).abs() <= TOLERANCE_USD,
        "{context}: the run credits {credited} for exported energy, but its intervals add \
         up to {export_credit_usd}",
        credited = cost.export_credit_usd,
    );

    let lines_usd: f64 = cost
        .demand_charge_lines
        .iter()
        .map(|line| line.charge_usd)
        .sum();
    assert!(
        (cost.demand_charge_usd - lines_usd).abs() <= TOLERANCE_USD,
        "{context}: the demand charge is {demand} but its lines add up to {lines_usd}",
        demand = cost.demand_charge_usd,
    );
    for line in &cost.demand_charge_lines {
        assert!(
            (line.charge_usd - line.peak_import_kw * line.price_per_kw_month_usd).abs()
                <= TOLERANCE_USD,
            "{context}: the {year}-{month} {period} demand line charges {charge} for a \
             {peak} kW peak at {price} per kW-month",
            year = line.year,
            month = line.month,
            period = line.period,
            charge = line.charge_usd,
            peak = line.peak_import_kw,
            price = line.price_per_kw_month_usd,
        );
        assert!(
            line.peak_import_kw >= 0.0 && line.charge_usd >= 0.0,
            "{context}: a demand line reports a negative peak or charge"
        );
    }

    let fixed_usd = sim.scenario.tariff.fixed_charge_usd_per_month * cost.billed_month_count as f64;
    assert!(
        (cost.fixed_charge_usd - fixed_usd).abs() <= TOLERANCE_USD,
        "{context}: the fixed charge is {fixed} for {months} billed month(s) at {rate} \
         per month, which should be {fixed_usd}",
        fixed = cost.fixed_charge_usd,
        months = cost.billed_month_count,
        rate = sim.scenario.tariff.fixed_charge_usd_per_month,
    );
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Every scenario of the realistic project, checked against every invariant.
fn flagship() -> RunResult {
    run::run_project(&flagship_project(), Entitlements::evaluation(), now())
        .expect("the flagship project runs")
}

fn simulated<'a>(result: &'a RunResult, id: &str) -> Simulated<'a> {
    let scenario = result
        .scenario(id)
        .unwrap_or_else(|| panic!("the fixture has a '{id}' scenario"));
    Simulated {
        context: format!("scenario '{id}'"),
        scenario: &scenario.scenario,
        timeline: &scenario.timeline,
        energy: &scenario.energy,
        cost: &scenario.cost,
    }
}

const SCENARIOS: [&str; 3] = ["baseline", "self-consumption", "peak-limit"];

#[test]
fn interval_energy_is_conserved() {
    let result = flagship();
    for id in SCENARIOS {
        conservation(&simulated(&result, id));
    }

    // Not vacuous: the site really does move energy in every direction the balance
    // has a term for.
    let storage = result.scenario("peak-limit").expect("a storage scenario");
    assert!(storage.energy.grid_import_kwh > 0.0, "nothing was imported");
    assert!(storage.energy.grid_export_kwh > 0.0, "nothing was exported");
    assert!(
        storage.energy.battery_charge_kwh > 0.0 && storage.energy.battery_discharge_kwh > 0.0,
        "the battery never moved energy, so conservation proved nothing"
    );
    assert!(
        storage.energy.generation_used_kwh > 0.0,
        "no generation reached the bus"
    );
}

#[test]
fn state_of_charge_stays_within_the_usable_envelope() {
    let result = flagship();
    for id in SCENARIOS {
        state_of_charge(&simulated(&result, id));
    }

    // The envelope is only worth asserting if the battery actually approached it.
    let storage = result.scenario("peak-limit").expect("a storage scenario");
    assert!(
        storage.energy.max_soc_pct > storage.energy.min_soc_pct + 10.0,
        "the battery barely moved: state of charge ranged only {min}% to {max}%",
        min = storage.energy.min_soc_pct,
        max = storage.energy.max_soc_pct,
    );
}

#[test]
fn charge_and_discharge_never_happen_together() {
    let result = flagship();
    for id in SCENARIOS {
        exclusivity(&simulated(&result, id));
    }
}

#[test]
fn unmet_and_shed_load_are_never_negative() {
    let result = flagship();
    for id in SCENARIOS {
        nonnegativity(&simulated(&result, id));
    }

    // The outage has to bite, or the unmet-load path is never taken and this test
    // asserts nothing about the branch it exists to guard.
    let storage = result.scenario("peak-limit").expect("a storage scenario");
    assert!(
        storage.energy.shed_noncritical_kwh > 0.0,
        "the outage shed no non-critical load"
    );
    assert!(
        storage.energy.unmet_critical_kwh > 0.0,
        "the battery carried the whole outage, so the unmet-critical branch never ran. \
         Lengthen the outage in the fixture rather than dropping this assertion."
    );
    let outage = storage
        .outages
        .first()
        .expect("the fixture models an outage");
    assert!(!outage.survived, "the fixture's outage was survivable");
    // Temperature derating may make the remaining stored energy temporarily
    // inaccessible before the hard SOC minimum is reached. The unmet-load and
    // survival assertions above exercise that branch without mislabelling it as
    // physical depletion.
}

#[test]
fn an_outage_takes_the_grid_away_entirely() {
    let result = flagship();
    for id in SCENARIOS {
        islanding(&simulated(&result, id));
    }
}

#[test]
fn the_cost_components_sum_to_the_total() {
    let result = flagship();
    for id in SCENARIOS {
        money(&simulated(&result, id));
    }

    // Every money term the identity has must be non-zero somewhere, or the identity
    // is being checked against a column of zeros.
    let storage = result.scenario("peak-limit").expect("a storage scenario");
    assert!(storage.cost.import_energy_cost_usd > 0.0);
    assert!(storage.cost.export_credit_usd > 0.0);
    assert!(storage.cost.demand_charge_usd > 0.0);
    assert!(storage.cost.fixed_charge_usd > 0.0);
    assert!(
        storage.cost.billed_month_count == 2,
        "the fixture is meant to span two billed months, not {}",
        storage.cost.billed_month_count
    );
    assert!(
        !storage.cost.demand_charge_lines.is_empty(),
        "the demand charge has no lines behind it"
    );
}

#[test]
fn the_same_project_run_twice_produces_identical_results() {
    let project = flagship_project();
    let first = run::run_project(&project, Entitlements::evaluation(), now()).expect("a run");
    let second = run::run_project(&project, Entitlements::evaluation(), now()).expect("a rerun");

    assert_eq!(
        first.model_input_hash, second.model_input_hash,
        "the same inputs hashed differently"
    );
    assert_eq!(
        first.results_hash().expect("a results hash"),
        second.results_hash().expect("a results hash"),
        "the same inputs produced different results"
    );
    assert!(
        first == second,
        "two runs of one project differ in a field the hashes do not cover"
    );

    // The injected clock reaches `generated_utc` and nothing else. Rerunning an hour
    // later must not move one number, and `results_hash` is what proves it.
    let later = run::run_project(
        &project,
        Entitlements::evaluation(),
        now() + TimeDelta::hours(1),
    )
    .expect("a later run");
    assert_ne!(
        later.generated_utc, first.generated_utc,
        "the injected clock never reached the result"
    );
    assert_eq!(
        later.model_input_hash, first.model_input_hash,
        "the wall clock leaked into the model input hash"
    );
    assert_eq!(
        later.results_hash().expect("a results hash"),
        first.results_hash().expect("a results hash"),
        "the wall clock changed a number"
    );
}

#[test]
fn two_independently_built_identical_projects_agree() {
    let first = run::run_project(&flagship_project(), Entitlements::evaluation(), now())
        .expect("the first project runs");
    let second = run::run_project(&flagship_project(), Entitlements::evaluation(), now())
        .expect("the second project runs");

    assert_eq!(
        first.project_hash, second.project_hash,
        "two identical projects hashed differently"
    );
    assert_eq!(
        first.model_input_hash, second.model_input_hash,
        "two identical projects have different model inputs"
    );
    assert_eq!(
        first.results_hash().expect("a results hash"),
        second.results_hash().expect("a results hash"),
        "two identical projects produced different results"
    );
}

/// Two thousand scenarios drawn from one seed. This is the test that catches the
/// case nobody thought to write down: every battery envelope, every dispatch policy,
/// flat and series tariffs, export ceilings, derating tables, and outages that land
/// anywhere in the data.
#[test]
fn every_invariant_holds_over_thousands_of_pseudo_random_scenarios() {
    let project = sweep_project();
    let dataset = project
        .dataset
        .as_ref()
        .expect("the sweep project has data");
    let grid = project
        .grid()
        .expect("the grid resolves")
        .expect("the sweep project has data");
    let mut rng = Rng(0x7A1E_5EED_0000_0001);
    let mut reached: BTreeMap<&str, usize> = BTreeMap::new();

    for draw in 0..SWEEP_DRAWS {
        let scenario = random_scenario(&mut rng);
        scenario
            .validate()
            .unwrap_or_else(|error| panic!("draw {draw} built an invalid scenario: {error}"));
        scenario
            .validate_against(dataset, &grid)
            .unwrap_or_else(|error| panic!("draw {draw} does not fit the data: {error}"));

        let input = ResolvedInput::resolve(&project, &scenario, &grid)
            .unwrap_or_else(|error| panic!("draw {draw} could not be resolved: {error}"));
        let result = engine::run(&scenario, &input.as_input(&grid), EngineOptions::default())
            .unwrap_or_else(|error| panic!("draw {draw} failed to run: {error}"));

        let sim = Simulated {
            context: format!("draw {draw} ({})", describe(&scenario)),
            scenario: &scenario,
            timeline: &result.timeline,
            energy: &result.energy,
            cost: &result.cost,
        };
        conservation(&sim);
        state_of_charge(&sim);
        exclusivity(&sim);
        nonnegativity(&sim);
        islanding(&sim);
        money(&sim);

        let energy = &result.energy;
        for (branch, taken) in [
            ("charged", energy.battery_charge_kwh > 0.0),
            ("discharged", energy.battery_discharge_kwh > 0.0),
            ("imported", energy.grid_import_kwh > 0.0),
            ("exported", energy.grid_export_kwh > 0.0),
            ("curtailed", energy.curtailed_kwh > 0.0),
            ("islanded", !scenario.outage.windows.is_empty()),
            ("shed load", energy.shed_noncritical_kwh > 0.0),
            ("left critical load unmet", energy.unmet_critical_kwh > 0.0),
            ("hit the state-of-charge ceiling", {
                let ceiling = scenario.battery.as_ref().map_or(0.0, |b| b.soc_max_pct);
                energy.max_soc_pct >= ceiling - 0.1
            }),
            ("charged from the grid", {
                matches!(
                    scenario.dispatch,
                    DispatchPolicy::PeakLimit {
                        allow_grid_charge: true,
                        ..
                    } | DispatchPolicy::ReserveFirst {
                        allow_grid_charge: true,
                        ..
                    }
                ) && energy.battery_charge_kwh > 0.0
            }),
        ] {
            if taken {
                *reached.entry(branch).or_default() += 1;
            }
        }
    }

    // A sweep that never reaches a branch has not tested it. Every path the engine
    // documents must be taken by a decent share of the draws, or the two thousand
    // runs above are two thousand runs of the same easy case.
    let floor = SWEEP_DRAWS / 40;
    for branch in [
        "charged",
        "discharged",
        "imported",
        "exported",
        "curtailed",
        "islanded",
        "shed load",
        "left critical load unmet",
        "hit the state-of-charge ceiling",
        "charged from the grid",
    ] {
        let count = reached.get(branch).copied().unwrap_or(0);
        assert!(
            count >= floor,
            "only {count} of {SWEEP_DRAWS} draws ever {branch}, below the {floor} this \
             sweep needs to have exercised that branch at all. Widen the draw, do not \
             lower the floor."
        );
    }
}

/// Enough of a scenario to reproduce a sweep failure by hand.
fn describe(scenario: &Scenario) -> String {
    let mut text = format!("{} ", scenario.dispatch.name());
    if let Some(battery) = &scenario.battery {
        write!(
            text,
            "{nameplate:.1} kWh, soc {min:.1}-{max:.1}%, reserve {reserve:.1}%, \
             eta {charge:.1}/{discharge:.1}%, start {initial:.1}%",
            nameplate = battery.nameplate_energy_kwh,
            min = battery.soc_min_pct,
            max = battery.soc_max_pct,
            reserve = battery.reserve_soc_pct,
            charge = battery.charge_efficiency_pct,
            discharge = battery.discharge_efficiency_pct,
            initial = scenario.initial_soc_pct,
        )
        .expect("writing to a String cannot fail");
    }
    text
}

// ---------------------------------------------------------------------------
// A deterministic source of pseudo-randomness
// ---------------------------------------------------------------------------

/// `SplitMix64`. Not cryptographic, and not meant to be: it exists so that the sweep
/// is reproducible from its seed alone, on any machine, forever — which is the same
/// property the crate under test claims for itself.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A float in `low..=high`.
    fn range(&mut self, low: f64, high: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        low + unit * (high - low)
    }

    /// A whole number below `limit`.
    fn below(&mut self, limit: usize) -> usize {
        (self.next_u64() % limit as u64) as usize
    }

    fn flip(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

const SWEEP_DRAWS: usize = 2_000;

fn random_scenario(rng: &mut Rng) -> Scenario {
    let soc_min_pct = rng.range(0.0, 20.0);
    let soc_max_pct = rng.range(soc_min_pct + 25.0, 100.0);

    let battery = BatterySpec {
        nameplate_energy_kwh: rng.range(5.0, 500.0),
        max_charge_kw: rng.range(1.0, 200.0),
        max_discharge_kw: rng.range(1.0, 200.0),
        soc_min_pct,
        soc_max_pct,
        reserve_soc_pct: rng.range(soc_min_pct, soc_max_pct),
        charge_efficiency_pct: rng.range(70.0, 100.0),
        discharge_efficiency_pct: rng.range(70.0, 100.0),
        self_discharge_pct_per_month: rng.range(0.0, 8.0),
        temperature_derate: if rng.flip() { Some(derate()) } else { None },
    };

    let dispatch = match rng.below(4) {
        0 => DispatchPolicy::SelfConsumption,
        1 => DispatchPolicy::PeakLimit {
            import_limit_kw: rng.range(0.0, 150.0),
            allow_grid_charge: rng.flip(),
            grid_charge_target_soc_pct: rng.range(soc_min_pct, soc_max_pct),
        },
        2 => DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct: rng.range(soc_min_pct, soc_max_pct),
            allow_grid_charge: rng.flip(),
        },
        _ => DispatchPolicy::ManualSchedule {
            entries: random_schedule(rng),
        },
    };

    let site_export_limit_kw = if rng.flip() {
        Some(rng.range(0.0, 120.0))
    } else {
        None
    };

    let mut demand_prices = BTreeMap::new();
    demand_prices.insert("on-peak".to_string(), rng.range(0.0, 30.0));
    demand_prices.insert("off-peak".to_string(), rng.range(0.0, 12.0));

    Scenario {
        id: "candidate".to_string(),
        name: "Candidate".to_string(),
        battery: Some(battery),
        inverter: InverterSpec {
            battery_power_kw: rng.range(1.0, 200.0),
            site_export_limit_kw,
        },
        dispatch,
        tariff: TariffSpec {
            // A negative import price is legitimate in a wholesale market, and the
            // money identities must hold through one.
            import: if rng.flip() {
                RateSource::Series
            } else {
                RateSource::Flat {
                    price_per_kwh_usd: rng.range(-0.05, 0.45),
                }
            },
            export: if rng.flip() {
                RateSource::Series
            } else {
                RateSource::Flat {
                    price_per_kwh_usd: rng.range(0.0, 0.2),
                }
            },
            import_price_multiplier: rng.range(0.5, 2.0),
            export_price_multiplier: rng.range(0.0, 2.0),
            demand_prices_per_kw_month_usd: demand_prices,
            fixed_charge_usd_per_month: rng.range(0.0, 200.0),
        },
        economics: EconomicsSpec::default(),
        degradation: DegradationSpec::default(),
        outage: random_outage(rng),
        initial_soc_pct: rng.range(soc_min_pct, soc_max_pct),
        initial_capacity_pct: rng.range(40.0, 100.0),
    }
}

/// Setpoint windows: sorted, non-overlapping, and aligned to the interval grid, as
/// the schema requires.
fn random_schedule(rng: &mut Rng) -> Vec<ScheduleEntry> {
    let mut entries = Vec::new();
    let mut cursor = 0usize;
    while entries.len() < 4 {
        let start = cursor + rng.below(8);
        let end = start + 1 + rng.below(6);
        if end > SWEEP_INTERVALS {
            break;
        }
        entries.push(ScheduleEntry {
            start_utc: sweep_interval_utc(start),
            end_utc: sweep_interval_utc(end),
            battery_power_kw: rng.range(-150.0, 150.0),
        });
        cursor = end;
    }
    entries
}

fn random_outage(rng: &mut Rng) -> OutageSpec {
    let mut windows = Vec::new();
    let mut cursor = 0usize;
    for _ in 0..rng.below(3) {
        let start = cursor + rng.below(12);
        let end = start + 1 + rng.below(14);
        if end > SWEEP_INTERVALS {
            break;
        }
        windows.push(OutageWindow {
            start_utc: sweep_interval_utc(start),
            end_utc: sweep_interval_utc(end),
            label: String::new(),
        });
        cursor = end;
    }
    OutageSpec { windows }
}

// ---------------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------------

/// The clock is injected. This crate never reads one.
const NOW_UNIX: i64 = 1_767_225_600;

const TIMEZONE: &str = "America/New_York";

/// Fourteen days of hourly data, starting at local midnight on 25 January 2024 and
/// running into February, so the demand charge spans two billed months. Both
/// fixtures sit in January, where `America/New_York` is a flat UTC-05:00 and no
/// local hour is ambiguous.
const FLAGSHIP_INTERVALS: usize = 336;

/// Three days, for the sweep. Short enough that two thousand runs are quick; long
/// enough to hold two demand-charge periods, several dispatch cycles, and an outage
/// anywhere in it.
const SWEEP_INTERVALS: usize = 72;

const LOAD_KW: [f64; 24] = [
    28.0, 26.0, 25.0, 25.0, 26.0, 30.0, 38.0, 52.0, 68.0, 78.0, 84.0, 88.0, 90.0, 92.0, 88.0, 82.0,
    76.0, 70.0, 64.0, 58.0, 50.0, 42.0, 36.0, 31.0,
];

/// On-site generation, by hour of the local day, before cloud cover. The array is
/// deliberately larger than the site's peak load: an oversized array behind a
/// constrained interconnection is the whole reason a site has an export ceiling, and
/// it is what puts the export and curtailment branches of the engine under test.
const PV_KW: [f64; 24] = [
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 8.0, 36.0, 76.0, 116.0, 144.0, 160.0, 156.0, 132.0, 92.0,
    44.0, 12.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
];

const TEMPERATURE_C: [f64; 24] = [
    -4.0, -4.5, -5.0, -5.0, -4.5, -4.0, -3.0, -1.5, 0.5, 2.5, 4.5, 6.0, 7.0, 7.5, 7.0, 6.0, 4.5,
    3.0, 1.5, 0.5, -0.5, -1.5, -2.5, -3.5,
];

/// Cloud cover, one factor per day of the week.
const SKY: [f64; 7] = [1.0, 0.85, 0.35, 1.0, 0.9, 0.55, 0.95];

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(NOW_UNIX, 0).expect("a valid instant")
}

fn flagship_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2024, 1, 25).expect("a real date")
}

fn sweep_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2024, 1, 8).expect("a real date")
}

/// Local midnight on `day`, as UTC. January in `America/New_York` is UTC-05:00.
fn local_midnight_utc(day: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&day.and_hms_opt(5, 0, 0).expect("a real time"))
}

fn flagship_interval_utc(index: usize) -> DateTime<Utc> {
    local_midnight_utc(flagship_start()) + TimeDelta::hours(index as i64)
}

fn sweep_interval_utc(index: usize) -> DateTime<Utc> {
    local_midnight_utc(sweep_start()) + TimeDelta::hours(index as i64)
}

fn csv(start: NaiveDate, intervals: usize) -> Vec<u8> {
    let mut text = String::from(
        "timestamp,load_kw,critical_kw,pv_kw,import_usd_kwh,export_usd_kwh,temperature_c,period\n",
    );
    let first = start.and_hms_opt(0, 0, 0).expect("a real time");

    for index in 0..intervals {
        let hour = index % 24;
        let day = index / 24;
        let local = first + TimeDelta::hours(index as i64);

        let load = LOAD_KW[hour];
        let on_peak = (14..=19).contains(&hour);

        writeln!(
            text,
            "{timestamp}-05:00,{load:.3},{critical:.3},{generation:.3},{import_price:.4},\
             {export_price:.4},{temperature:.2},{period}",
            timestamp = local.format("%Y-%m-%dT%H:%M:%S"),
            critical = load * 0.45,
            generation = PV_KW[hour] * SKY[day % SKY.len()],
            import_price = if on_peak { 0.28 } else { 0.09 } + (day % 5) as f64 * 0.005,
            export_price = 0.04 + (day % 3) as f64 * 0.005,
            temperature = TEMPERATURE_C[hour],
            period = if on_peak { "on-peak" } else { "off-peak" },
        )
        .expect("writing to a String cannot fail");
    }
    text.into_bytes()
}

fn import_request() -> ImportRequest {
    let mut series = BTreeMap::new();
    for (kind, header, unit) in [
        (SeriesKind::Load, "load_kw", ImportUnit::Kilowatt),
        (
            SeriesKind::CriticalLoad,
            "critical_kw",
            ImportUnit::Kilowatt,
        ),
        (SeriesKind::Generation, "pv_kw", ImportUnit::Kilowatt),
        (
            SeriesKind::ImportPrice,
            "import_usd_kwh",
            ImportUnit::UsdPerKilowattHour,
        ),
        (
            SeriesKind::ExportPrice,
            "export_usd_kwh",
            ImportUnit::UsdPerKilowattHour,
        ),
        (
            SeriesKind::Temperature,
            "temperature_c",
            ImportUnit::Celsius,
        ),
    ] {
        series.insert(
            kind,
            MappedColumn {
                header: header.to_string(),
                unit,
            },
        );
    }

    ImportRequest {
        filename: "meter.csv".to_string(),
        timezone: TIMEZONE.to_string(),
        interval_minutes: 60,
        gap_policy: GapPolicy::Reject,
        mapping: ColumnMapping {
            timestamp: "timestamp".to_string(),
            series,
            demand_period: Some("period".to_string()),
            ignore_columns: Vec::new(),
        },
    }
}

fn base_project(id: &str, name: &str, start: NaiveDate, intervals: usize) -> Project {
    let site = Site {
        timezone: TIMEZONE.to_string(),
        currency: "USD".to_string(),
    };
    let mut project = Project::new(id, name, site, now()).expect("the fixture project is valid");
    project.dataset = Some(
        csvimport::import(&csv(start, intervals), &import_request(), now())
            .expect("the fixture CSV imports cleanly")
            .dataset,
    );
    project
}

fn sweep_project() -> Project {
    base_project("sweep", "Sweep", sweep_start(), SWEEP_INTERVALS)
}

fn derate() -> TemperatureDerate {
    TemperatureDerate {
        points: vec![
            TemperatureDeratePoint {
                temperature_c: -10.0,
                power_limit_pct: 60.0,
                usable_capacity_pct: 85.0,
            },
            TemperatureDeratePoint {
                temperature_c: 0.0,
                power_limit_pct: 80.0,
                usable_capacity_pct: 92.0,
            },
            TemperatureDeratePoint {
                temperature_c: 25.0,
                power_limit_pct: 100.0,
                usable_capacity_pct: 100.0,
            },
        ],
    }
}

fn tariff() -> TariffSpec {
    let mut demand_prices = BTreeMap::new();
    demand_prices.insert("on-peak".to_string(), 18.5);
    demand_prices.insert("off-peak".to_string(), 4.25);
    TariffSpec {
        import: RateSource::Series,
        export: RateSource::Series,
        import_price_multiplier: 1.0,
        export_price_multiplier: 1.0,
        demand_prices_per_kw_month_usd: demand_prices,
        fixed_charge_usd_per_month: 45.0,
    }
}

/// Long enough overnight, and far enough into the data, that the battery cannot
/// carry the critical load to the end of it.
fn outage() -> OutageSpec {
    OutageSpec {
        windows: vec![OutageWindow {
            start_utc: flagship_interval_utc(138),
            end_utc: flagship_interval_utc(150),
            label: "Winter storm".to_string(),
        }],
    }
}

fn storage_scenario(id: &str, name: &str, dispatch: DispatchPolicy) -> Scenario {
    Scenario {
        id: id.to_string(),
        name: name.to_string(),
        battery: Some(BatterySpec {
            nameplate_energy_kwh: 200.0,
            max_charge_kw: 60.0,
            max_discharge_kw: 60.0,
            soc_min_pct: 10.0,
            soc_max_pct: 95.0,
            reserve_soc_pct: 25.0,
            charge_efficiency_pct: 95.0,
            discharge_efficiency_pct: 94.0,
            self_discharge_pct_per_month: 2.0,
            temperature_derate: Some(derate()),
        }),
        inverter: InverterSpec {
            battery_power_kw: 60.0,
            // A limited-export interconnection, so generation is curtailed rather
            // than sold, and the curtailment branch is taken.
            site_export_limit_kw: Some(25.0),
        },
        dispatch,
        tariff: tariff(),
        economics: EconomicsSpec {
            capex_usd: 180_000.0,
            annual_opex_usd: 900.0,
            horizon_years: 15,
            discount_rate_pct_per_year: 6.0,
            energy_price_escalation_pct_per_year: 2.5,
            replacement: Some(ReplacementSpec {
                cost_usd: 40_000.0,
                trigger_capacity_pct: 80.0,
            }),
        },
        degradation: DegradationSpec::default(),
        outage: outage(),
        initial_soc_pct: 50.0,
        initial_capacity_pct: 100.0,
    }
}

fn flagship_project() -> Project {
    let mut project = base_project(
        "invariants",
        "Cold store, Bay 4",
        flagship_start(),
        FLAGSHIP_INTERVALS,
    );

    // The baseline is priced on the same tariff and lives through the same outage as
    // every candidate, so the comparison is like for like — and so that the no-battery
    // islanding path is exercised too.
    let baseline = &mut project.scenarios[0];
    baseline.tariff = tariff();
    baseline.outage = outage();

    project.scenarios.push(storage_scenario(
        "self-consumption",
        "Storage, self-consumption",
        DispatchPolicy::SelfConsumption,
    ));
    project.scenarios.push(storage_scenario(
        "peak-limit",
        "Storage, peak limit",
        DispatchPolicy::PeakLimit {
            import_limit_kw: 55.0,
            allow_grid_charge: true,
            grid_charge_target_soc_pct: 80.0,
        },
    ));

    project.validate().expect("the fixture project is valid");
    project
}
