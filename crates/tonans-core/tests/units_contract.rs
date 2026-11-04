//! The units contract: no number reaches a report without a unit.
//!
//! `units::unit_for_field` resolves a unit from a field's *name*. This file is the
//! enforcement of that promise. It runs a project whose result tree is populated in
//! every optional corner — a battery with a derating table, all four dispatch
//! policies, a demand charge over two billed months, an outage the site does not
//! survive, a replacement inside the horizon — serializes the result, walks every
//! numeric leaf, and fails if the name that leaf sits under cannot be resolved to a
//! unit.
//!
//! Adding a quantity to the schema, the engine, or the economics without a unit
//! suffix therefore fails the build rather than reaching a reader as a bare number.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, NaiveDate, TimeDelta, TimeZone, Utc};
use serde_json::Value;
use tonans_core::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
use tonans_core::run::{self, RunResult};
use tonans_core::schema::{
    BatterySpec, BenefitScaling, DegradationSpec, DispatchPolicy, EconomicsSpec, GapPolicy,
    InverterSpec, OutageSpec, OutageWindow, Project, RateSource, ReplacementSpec, Scenario,
    ScheduleEntry, SeriesKind, Site, TariffSpec, TemperatureDerate, TemperatureDeratePoint,
};
use tonans_core::units::unit_for_field;

// ---------------------------------------------------------------------------
// The one list of exceptions, and why each entry is on it
// ---------------------------------------------------------------------------

/// Numeric leaves that are legitimately *not* measured quantities.
///
/// Every entry is a label, an ordinal, or a bound on a count of things — a number
/// that names something rather than measuring it. Nothing a reader would call a
/// quantity belongs here; a quantity belongs in [`unit_for_field`], and the test
/// below refuses an entry the unit table can already resolve, so this list cannot
/// be used to bury a field that simply lacks a suffix.
const UNITLESS: &[&str] = &[
    // The on-disk schema version. An identifier, not a measurement.
    "schema_version",
    // A demand-charge line's calendar year and month, and a cash-flow row's ordinal
    // year (1..=horizon). These label the row. The *durations* in the same tree —
    // `horizon_years`, `simple_payback_years`, `max_horizon_years` — carry `_years`
    // and resolve through the unit table, as they must.
    "year",
    "month",
    // `Limits`: the compiled-in envelope. Each of these is a hard bound on a count
    // of things the software will accept — CSV rows, CSV columns, intervals,
    // scenarios, sensitivity cells, outage windows — not a quantity measured from
    // the data. The two limits that *are* dimensioned, `max_csv_bytes` and
    // `max_horizon_years`, resolve through the unit table and are deliberately
    // absent from this list.
    "max_csv_rows",
    "max_csv_columns",
    "max_intervals",
    "max_scenarios",
    "max_run_rows",
    "max_sensitivity_cells",
    "max_outage_windows",
    // A dimensionless fraction of a file, 0.0..=1.0. A bound, not a measurement.
    "max_synthesized_fraction",
];

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Walk {
    /// Every numeric leaf visited, counted. Guards against a vacuous pass.
    numeric_leaves: usize,
    /// Every distinct field name a numeric leaf sat under.
    names: BTreeSet<String>,
    /// Field name to the first JSON path it was seen at, for names with no unit.
    unlabelled: BTreeMap<String, String>,
}

/// Walk `value`, treating `key` as the name that labels any number found in it.
fn walk(value: &Value, key: &str, path: &str, out: &mut Walk) {
    match value {
        Value::Number(_) => {
            out.numeric_leaves += 1;
            out.names.insert(key.to_string());
            if unit_for_field(key).is_none() && !UNITLESS.contains(&key) {
                out.unlabelled
                    .entry(key.to_string())
                    .or_insert_with(|| path.to_string());
            }
        }
        Value::Array(items) => {
            // An array's elements inherit the array's own name: every element of
            // `replacement_years` is a year.
            for (index, item) in items.iter().enumerate() {
                walk(item, key, &format!("{path}[{index}]"), out);
            }
        }
        Value::Object(fields) => {
            // A field whose own name carries a unit and whose values are *all*
            // numbers is a labelled numeric map. `demand_prices_per_kw_month_usd`
            // is keyed by a user-supplied period label — "on-peak" is not a unit,
            // and never will be — but the map itself is named for its unit, and
            // every value in it is money per kW-month. Nothing else in the result
            // tree has this shape.
            let labelled_map = unit_for_field(key).is_some()
                && !fields.is_empty()
                && fields.values().all(Value::is_number);
            for (name, child) in fields {
                let inherited = if labelled_map { key } else { name.as_str() };
                walk(child, inherited, &format!("{path}/{name}"), out);
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

fn walk_run_result(result: &RunResult) -> Walk {
    let value = serde_json::to_value(result).expect("a run result serializes");
    let mut out = Walk::default();
    walk(&value, "", "", &mut out);
    out
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn every_numeric_leaf_in_a_populated_run_result_carries_a_unit() {
    let out = walk_run_result(&populated_run());

    assert!(
        out.unlabelled.is_empty(),
        "these numeric fields would reach a reader carrying no unit at all. Give each \
         one a name whose suffix `units::unit_for_field` resolves, or — if it is \
         genuinely not a measured quantity — add it to UNITLESS with the reason:\n{}",
        out.unlabelled
            .iter()
            .fold(String::new(), |mut text, (name, path)| {
                writeln!(text, "  {name}  (first seen at {path})")
                    .expect("writing to a String cannot fail");
                text
            })
    );
}

/// The contract is worth nothing if the walk never reached the numbers.
///
/// The count guards against an empty or half-serialized tree; the named fields
/// guard against a walk that stopped at the top level, since each one lives at the
/// bottom of a different branch — a battery inside a scenario, a point inside a
/// derating table, a line inside the demand charge, a year inside the cash flow, an
/// entry inside a manual schedule, a price inside a label-keyed map.
#[test]
fn the_walk_actually_reached_every_corner_of_the_tree() {
    let out = walk_run_result(&populated_run());

    assert!(
        out.numeric_leaves > 30,
        "the walk saw only {} numeric leaves, so the contract above proved nothing",
        out.numeric_leaves
    );

    for name in [
        "schema_version",                         // the root
        "max_csv_bytes",                          // limits
        "energy_balance_residual_kwh",            // the engine's own audit
        "nameplate_energy_kwh",                   // scenario.battery
        "usable_capacity_pct",                    // scenario.battery.temperature_derate.points[]
        "battery_power_kw",                       // scenario.inverter, and a schedule entry
        "import_limit_kw",                        // a peak-limit dispatch policy
        "reserve_target_soc_pct",                 // a reserve-first dispatch policy
        "demand_prices_per_kw_month_usd",         // the label-keyed price map
        "fixed_charge_usd_per_month",             // scenario.tariff, money per month
        "price_per_kw_month_usd",                 // cost.demand_charge_lines[]
        "soc_at_start_pct",                       // outages[]
        "cumulative_discounted_usd",              // economics.cash_flow[]
        "replacement_years",                      // economics.replacement_years[]
        "self_sufficiency_delta_pct",             // comparison
        "trigger_capacity_pct",                   // scenario.economics.replacement
        "throughput_cost_per_kwh_throughput_usd", // scenario.degradation
    ] {
        assert!(
            out.names.contains(name),
            "the walk never reached '{name}', so this run result is not populated \
             enough to enforce the contract. Fix the fixture, not the assertion."
        );
    }
}

/// [`UNITLESS`] may not carry an entry the result tree does not have, nor one the
/// unit table already resolves. A stale exception is how a contract quietly rots
/// into a formality: the excused name goes away, or grows a unit, and the list keeps
/// excusing it — so the next field to arrive under that name is excused too, silently.
#[test]
fn the_list_of_exceptions_holds_nothing_it_does_not_need_to() {
    let out = walk_run_result(&populated_run());

    for name in UNITLESS {
        assert!(
            out.names.contains(*name),
            "'{name}' is excused from the units contract, but no numeric field of that \
             name appears in a run result any more. Delete the entry."
        );
        assert!(
            unit_for_field(name).is_none(),
            "'{name}' is excused from the units contract, but `unit_for_field` resolves \
             it to {unit:?}. Delete the entry.",
            unit = unit_for_field(name),
        );
    }
}

/// The contract must have teeth: a new unlabelled number has to break it.
#[test]
fn an_unlabelled_number_added_to_the_tree_breaks_the_contract() {
    let mut value = serde_json::to_value(populated_run()).expect("a run result serializes");
    value["scenarios"][1]["energy"]["mystery"] = serde_json::json!(1.0);

    let mut out = Walk::default();
    walk(&value, "", "", &mut out);

    assert_eq!(
        out.unlabelled.keys().collect::<Vec<_>>(),
        vec!["mystery"],
        "a numeric field with no unit suffix must be caught by the walk"
    );
}

// ---------------------------------------------------------------------------
// The fixture: a project whose result tree has no empty corners
// ---------------------------------------------------------------------------

/// The clock is injected. This crate never reads one.
const NOW_UNIX: i64 = 1_767_225_600;

const TIMEZONE: &str = "America/New_York";

/// Fourteen days of hourly data. It starts at local midnight on 25 January 2024 and
/// runs into February, so the demand charge spans two billed months, and it is far
/// enough from a DST transition that every local hour is unambiguous.
const INTERVALS: usize = 336;

/// Site load, by hour of the local day.
const LOAD_KW: [f64; 24] = [
    28.0, 26.0, 25.0, 25.0, 26.0, 30.0, 38.0, 52.0, 68.0, 78.0, 84.0, 88.0, 90.0, 92.0, 88.0, 82.0,
    76.0, 70.0, 64.0, 58.0, 50.0, 42.0, 36.0, 31.0,
];

/// On-site generation, by hour of the local day, before cloud cover. Deliberately
/// larger than the site's peak load, so that the export and curtailment branches are
/// taken and their numbers appear in the tree.
const PV_KW: [f64; 24] = [
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 8.0, 36.0, 76.0, 116.0, 144.0, 160.0, 156.0, 132.0, 92.0,
    44.0, 12.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
];

/// Ambient temperature, by hour of the local day. A cold January, so the derating
/// table is actually exercised rather than sitting at 100%.
const TEMPERATURE_C: [f64; 24] = [
    -4.0, -4.5, -5.0, -5.0, -4.5, -4.0, -3.0, -1.5, 0.5, 2.5, 4.5, 6.0, 7.0, 7.5, 7.0, 6.0, 4.5,
    3.0, 1.5, 0.5, -0.5, -1.5, -2.5, -3.5,
];

/// Cloud cover, one factor per day of the week.
const SKY: [f64; 7] = [1.0, 0.85, 0.35, 1.0, 0.9, 0.55, 0.95];

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(NOW_UNIX, 0).expect("a valid instant")
}

/// Local midnight on 25 January 2024 in `America/New_York`, which is UTC-05:00.
fn grid_start_utc() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 25, 5, 0, 0)
        .single()
        .expect("a real instant")
}

fn interval_utc(index: usize) -> DateTime<Utc> {
    grid_start_utc() + TimeDelta::hours(index as i64)
}

fn csv() -> Vec<u8> {
    let mut text = String::from(
        "timestamp,load_kw,critical_kw,pv_kw,import_usd_kwh,export_usd_kwh,temperature_c,period\n",
    );
    let start = NaiveDate::from_ymd_opt(2024, 1, 25)
        .expect("a real date")
        .and_hms_opt(0, 0, 0)
        .expect("a real time");

    for index in 0..INTERVALS {
        let hour = index % 24;
        let day = index / 24;
        let local = start + TimeDelta::hours(index as i64);

        let load = LOAD_KW[hour];
        let critical = load * 0.45;
        let generation = PV_KW[hour] * SKY[day % SKY.len()];
        let on_peak = (14..=19).contains(&hour);
        let import_price = if on_peak { 0.28 } else { 0.09 } + (day % 5) as f64 * 0.005;
        let export_price = 0.04 + (day % 3) as f64 * 0.005;
        let period = if on_peak { "on-peak" } else { "off-peak" };

        writeln!(
            text,
            "{timestamp}-05:00,{load:.3},{critical:.3},{generation:.3},{import_price:.4},\
             {export_price:.4},{temperature:.2},{period}",
            timestamp = local.format("%Y-%m-%dT%H:%M:%S"),
            temperature = TEMPERATURE_C[hour],
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

fn battery() -> BatterySpec {
    BatterySpec {
        nameplate_energy_kwh: 200.0,
        max_charge_kw: 60.0,
        max_discharge_kw: 60.0,
        soc_min_pct: 10.0,
        soc_max_pct: 95.0,
        reserve_soc_pct: 25.0,
        charge_efficiency_pct: 95.0,
        discharge_efficiency_pct: 94.0,
        self_discharge_pct_per_month: 2.0,
        temperature_derate: Some(TemperatureDerate {
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
                TemperatureDeratePoint {
                    temperature_c: 45.0,
                    power_limit_pct: 85.0,
                    usable_capacity_pct: 96.0,
                },
            ],
        }),
    }
}

/// Deep enough into an overnight window that the battery cannot hold the critical
/// load to the end: the unmet-load and reserve-depletion branches are taken.
fn outage() -> OutageSpec {
    OutageSpec {
        windows: vec![OutageWindow {
            start_utc: interval_utc(138),
            end_utc: interval_utc(150),
            label: "Winter storm".to_string(),
        }],
    }
}

fn economics() -> EconomicsSpec {
    EconomicsSpec {
        capex_usd: 18_000.0,
        annual_opex_usd: 900.0,
        horizon_years: 15,
        discount_rate_pct_per_year: 6.0,
        energy_price_escalation_pct_per_year: 2.5,
        replacement: Some(ReplacementSpec {
            cost_usd: 40_000.0,
            trigger_capacity_pct: 80.0,
        }),
    }
}

fn degradation() -> DegradationSpec {
    DegradationSpec {
        calendar_fade_pct_per_year: 2.5,
        cycle_fade_pct_per_cycle: 0.005,
        throughput_cost_per_kwh_throughput_usd: 0.012,
        benefit_scaling: BenefitScaling::ProportionalToUsableCapacity,
    }
}

/// A storage scenario. `dispatch` is the only thing that varies between them, so
/// that one run covers every policy the schema can express.
fn storage_scenario(id: &str, name: &str, dispatch: DispatchPolicy) -> Scenario {
    Scenario {
        id: id.to_string(),
        name: name.to_string(),
        battery: Some(battery()),
        inverter: InverterSpec {
            battery_power_kw: 60.0,
            // A limited-export interconnection, so the curtailment branch is taken.
            site_export_limit_kw: Some(25.0),
        },
        dispatch,
        tariff: tariff(),
        economics: economics(),
        degradation: degradation(),
        outage: outage(),
        initial_soc_pct: 50.0,
        initial_capacity_pct: 100.0,
    }
}

fn populated_project() -> Project {
    let site = Site {
        timezone: TIMEZONE.to_string(),
        currency: "USD".to_string(),
    };
    let mut project = Project::new("units-contract", "Cold store, Bay 4", site, now())
        .expect("the fixture project is valid");

    project.dataset = Some(
        csvimport::import(&csv(), &import_request(), now())
            .expect("the fixture CSV imports cleanly")
            .dataset,
    );
    project.notes = "Fixture for the units contract.".to_string();

    // The baseline is priced on the same tariff and lives through the same outage as
    // every candidate, so the comparison is against a like-for-like site.
    let baseline = &mut project.scenarios[0];
    baseline.tariff = tariff();
    baseline.economics = economics();
    baseline.economics.capex_usd = 0.0;
    baseline.economics.replacement = None;
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
    project.scenarios.push(storage_scenario(
        "reserve-first",
        "Storage, reserve first",
        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct: 60.0,
            allow_grid_charge: true,
        },
    ));
    project.scenarios.push(storage_scenario(
        "manual-schedule",
        "Storage, manual schedule",
        DispatchPolicy::ManualSchedule {
            entries: vec![
                ScheduleEntry {
                    start_utc: interval_utc(10),
                    end_utc: interval_utc(14),
                    battery_power_kw: -40.0,
                },
                ScheduleEntry {
                    start_utc: interval_utc(38),
                    end_utc: interval_utc(44),
                    battery_power_kw: 55.0,
                },
            ],
        },
    ));

    project.validate().expect("the fixture project is valid");
    project
}

fn populated_run() -> RunResult {
    // One run carries all four dispatch policies, and a policy that is never run
    // is a policy whose numbers this contract never sees.
    run::run_project(&populated_project(), now()).expect("the fixture project runs")
}
