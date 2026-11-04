use chrono::{TimeZone, Utc};
use std::collections::BTreeMap;

use tonans_core::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
use tonans_core::engine::{self, EngineInput, EngineOptions, EngineResult};
use tonans_core::run;
use tonans_core::schema::{
    BatterySpec, DispatchPolicy, GapPolicy, InverterSpec, OutageWindow, Project, Scenario,
    ScheduleEntry, SeriesKind, Site, TemperatureDerate, TemperatureDeratePoint,
};
use tonans_core::timegrid::IntervalGrid;

const TOLERANCE: f64 = 1e-9;

fn start() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn battery() -> BatterySpec {
    BatterySpec {
        nameplate_energy_kwh: 100.0,
        max_charge_kw: 100.0,
        max_discharge_kw: 100.0,
        soc_min_pct: 0.0,
        soc_max_pct: 100.0,
        reserve_soc_pct: 0.0,
        charge_efficiency_pct: 100.0,
        discharge_efficiency_pct: 100.0,
        self_discharge_pct_per_month: 0.0,
        temperature_derate: None,
    }
}

fn scenario(dispatch: DispatchPolicy, initial_soc_pct: f64, battery: BatterySpec) -> Scenario {
    let mut scenario = Scenario::no_storage_baseline();
    scenario.id = "storage".to_string();
    scenario.name = "Storage".to_string();
    scenario.inverter = InverterSpec {
        battery_power_kw: 100.0,
        site_export_limit_kw: None,
    };
    scenario.battery = Some(battery);
    scenario.dispatch = dispatch;
    scenario.initial_soc_pct = initial_soc_pct;
    scenario
}

fn simulate(
    scenario: &Scenario,
    load_kw: &[f64],
    generation_kw: &[f64],
    temperature_c: Option<&[f64]>,
) -> EngineResult {
    assert_eq!(load_kw.len(), generation_kw.len());
    let count = load_kw.len();
    let grid = IntervalGrid::new(start(), 60, count, chrono_tz::UTC).unwrap();
    let zero = vec![0.0; count];
    let periods = vec![0; count];
    let outage = vec![false; count];
    let labels = vec!["all".to_string()];
    let input = EngineInput {
        grid: &grid,
        load_kw,
        critical_load_kw: &zero,
        generation_kw,
        import_price_per_kwh_usd: &zero,
        export_price_per_kwh_usd: &zero,
        temperature_c,
        demand_period_index: &periods,
        demand_labels: &labels,
        outage: &outage,
    };
    engine::run(scenario, &input, EngineOptions::default()).unwrap()
}

fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= TOLERANCE,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn grid_charging_stops_exactly_at_each_policy_target() {
    let mut spec = battery();
    spec.charge_efficiency_pct = 80.0;
    for dispatch in [
        DispatchPolicy::PeakLimit {
            import_limit_kw: 100.0,
            allow_grid_charge: true,
            grid_charge_target_soc_pct: 50.0,
        },
        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct: 50.0,
            allow_grid_charge: true,
        },
    ] {
        let result = simulate(
            &scenario(dispatch, 49.0, spec.clone()),
            &[0.0],
            &[0.0],
            None,
        );
        close(result.timeline[0].battery_charge_kwh, 1.25);
        close(result.energy.final_stored_energy_kwh, 50.0);
        close(result.energy.max_soc_pct, 50.0);
    }
}

#[test]
fn direct_generation_serving_load_is_not_penalized_by_grid_charging() {
    let dispatch = DispatchPolicy::PeakLimit {
        import_limit_kw: 5.0,
        allow_grid_charge: true,
        grid_charge_target_soc_pct: 55.0,
    };
    let result = simulate(&scenario(dispatch, 50.0, battery()), &[10.0], &[10.0], None);
    close(result.energy.generation_to_load_kwh, 10.0);
    close(result.energy.grid_to_battery_kwh, 5.0);
    close(result.energy.self_consumption_pct, 100.0);
    close(result.energy.self_sufficiency_pct, 100.0);
}

#[test]
fn battery_export_does_not_hide_directly_self_consumed_generation() {
    let dispatch = DispatchPolicy::ManualSchedule {
        entries: vec![ScheduleEntry {
            start_utc: start(),
            end_utc: start() + chrono::TimeDelta::hours(1),
            battery_power_kw: 5.0,
        }],
    };
    let result = simulate(&scenario(dispatch, 50.0, battery()), &[5.0], &[10.0], None);
    close(result.energy.generation_to_load_kwh, 5.0);
    close(result.energy.generation_to_grid_kwh, 5.0);
    close(result.energy.battery_to_grid_kwh, 5.0);
    close(result.energy.self_consumption_pct, 50.0);
}

#[test]
fn unknown_initial_energy_is_excluded_from_self_sufficiency() {
    let result = simulate(
        &scenario(DispatchPolicy::SelfConsumption, 50.0, battery()),
        &[10.0],
        &[0.0],
        None,
    );
    close(result.energy.initial_origin_battery_to_load_kwh, 10.0);
    close(result.energy.self_sufficiency_pct, 0.0);
    assert!(result
        .warnings
        .iter()
        .any(|warning| { warning.code == "model.initial_energy_excluded_from_self_sufficiency" }));
}

#[test]
fn stored_generation_retains_its_origin_when_it_later_serves_load() {
    let result = simulate(
        &scenario(DispatchPolicy::SelfConsumption, 0.0, battery()),
        &[0.0, 9.0],
        &[10.0, 0.0],
        None,
    );
    close(result.energy.generation_to_battery_kwh, 10.0);
    close(result.energy.generation_origin_battery_to_load_kwh, 9.0);
    close(result.energy.self_consumption_pct, 100.0);
    close(result.energy.self_sufficiency_pct, 100.0);
}

#[test]
fn mixed_generation_and_grid_storage_is_attributed_pro_rata() {
    let dispatch = DispatchPolicy::ManualSchedule {
        entries: vec![
            ScheduleEntry {
                start_utc: start(),
                end_utc: start() + chrono::TimeDelta::hours(1),
                battery_power_kw: -10.0,
            },
            ScheduleEntry {
                start_utc: start() + chrono::TimeDelta::hours(1),
                end_utc: start() + chrono::TimeDelta::hours(2),
                battery_power_kw: 10.0,
            },
        ],
    };
    let result = simulate(
        &scenario(dispatch, 0.0, battery()),
        &[0.0, 10.0],
        &[5.0, 0.0],
        None,
    );
    close(result.energy.generation_to_battery_kwh, 5.0);
    close(result.energy.grid_to_battery_kwh, 5.0);
    close(result.energy.generation_origin_battery_to_load_kwh, 5.0);
    close(result.energy.grid_origin_battery_to_load_kwh, 5.0);
    close(result.energy.self_sufficiency_pct, 50.0);
}

#[test]
fn cold_capacity_is_inaccessible_until_warming_without_destroying_energy() {
    let mut spec = battery();
    spec.soc_min_pct = 10.0;
    spec.reserve_soc_pct = 10.0;
    spec.temperature_derate = Some(TemperatureDerate {
        points: vec![
            TemperatureDeratePoint {
                temperature_c: -10.0,
                power_limit_pct: 100.0,
                usable_capacity_pct: 50.0,
            },
            TemperatureDeratePoint {
                temperature_c: 25.0,
                power_limit_pct: 100.0,
                usable_capacity_pct: 100.0,
            },
        ],
    });
    let dispatch = DispatchPolicy::ManualSchedule {
        entries: vec![ScheduleEntry {
            start_utc: start(),
            end_utc: start() + chrono::TimeDelta::hours(3),
            battery_power_kw: 100.0,
        }],
    };
    let result = simulate(
        &scenario(dispatch, 100.0, spec),
        &[100.0, 100.0, 100.0],
        &[0.0, 0.0, 0.0],
        Some(&[-10.0, -10.0, 25.0]),
    );
    close(result.timeline[0].battery_discharge_kwh, 45.0);
    close(result.timeline[0].stored_energy_kwh, 55.0);
    close(result.timeline[1].battery_discharge_kwh, 0.0);
    close(result.timeline[1].stored_energy_kwh, 55.0);
    close(result.timeline[2].battery_discharge_kwh, 45.0);
    close(result.timeline[2].stored_energy_kwh, 10.0);
}

#[test]
fn standing_loss_stops_at_the_hard_soc_minimum() {
    let mut spec = battery();
    spec.soc_min_pct = 20.0;
    spec.reserve_soc_pct = 20.0;
    spec.self_discharge_pct_per_month = 50.0;
    let result = simulate(
        &scenario(DispatchPolicy::SelfConsumption, 20.001, spec),
        &[0.0, 0.0],
        &[0.0, 0.0],
        None,
    );
    close(result.timeline[0].self_discharge_loss_kwh, 0.001);
    close(result.timeline[0].stored_energy_kwh, 20.0);
    close(result.timeline[1].self_discharge_loss_kwh, 0.0);
    close(result.timeline[1].stored_energy_kwh, 20.0);
}

#[test]
fn minimum_soc_includes_the_post_loss_state_before_same_interval_recharge() {
    let mut spec = battery();
    spec.soc_min_pct = 20.0;
    spec.reserve_soc_pct = 20.0;
    spec.self_discharge_pct_per_month = 50.0;
    let mut scenario = scenario(DispatchPolicy::SelfConsumption, 20.001, spec);
    scenario.outage.windows.push(OutageWindow {
        start_utc: start(),
        end_utc: start() + chrono::TimeDelta::hours(1),
        label: "Recharge after floor".to_string(),
    });
    let grid = IntervalGrid::new(start(), 60, 1, chrono_tz::UTC).unwrap();
    let zero = [0.0];
    let generation = [10.0];
    let periods = [0];
    let labels = ["all".to_string()];
    let outage = [true];
    let result = engine::run(
        &scenario,
        &EngineInput {
            grid: &grid,
            load_kw: &zero,
            critical_load_kw: &zero,
            generation_kw: &generation,
            import_price_per_kwh_usd: &zero,
            export_price_per_kwh_usd: &zero,
            temperature_c: None,
            demand_period_index: &periods,
            demand_labels: &labels,
            outage: &outage,
        },
        EngineOptions::default(),
    )
    .unwrap();

    close(result.timeline[0].soc_pct, 30.0);
    close(result.energy.min_soc_pct, 20.0);
    close(result.outages[0].min_soc_pct, 20.0);
    assert!(result.outages[0].reserve_depleted);
}

#[test]
fn results_hash_ignores_explanatory_prose_but_tracks_numeric_outputs() {
    let mut mapping = BTreeMap::new();
    mapping.insert(
        SeriesKind::Load,
        MappedColumn {
            header: "load_kw".to_string(),
            unit: ImportUnit::Kilowatt,
        },
    );
    let request = ImportRequest {
        filename: "meter.csv".to_string(),
        timezone: "UTC".to_string(),
        interval_minutes: 60,
        gap_policy: GapPolicy::Reject,
        mapping: ColumnMapping {
            timestamp: "timestamp".to_string(),
            series: mapping,
            demand_period: None,
            ignore_columns: Vec::new(),
        },
    };
    let dataset = csvimport::import(
        b"timestamp,load_kw\n2026-01-01T00:00:00Z,10\n",
        &request,
        start(),
    )
    .unwrap()
    .dataset;
    let mut project = Project::new(
        "hash-project",
        "Hash project",
        Site {
            timezone: "UTC".to_string(),
            currency: "USD".to_string(),
        },
        start(),
    )
    .unwrap();
    project.dataset = Some(dataset);

    let mut result = run::run_project(&project, start()).unwrap();
    let original = result.results_hash().unwrap();
    result.scenarios[0].economics.degradation_method = "wording changed only".to_string();
    assert_eq!(result.results_hash().unwrap(), original);
    result.scenarios[0].economics.npv_usd += 1.0;
    assert_ne!(result.results_hash().unwrap(), original);
}
