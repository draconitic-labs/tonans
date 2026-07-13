//! Normalized replay of the public SSC battery-capacity reference sequence.
//!
//! See `docs/BENCHMARK.md` for the pinned upstream fixture, expected values, and
//! scope boundary. This is a battery-state accounting cross-check, not a claim
//! of SAM certification or full SSC parity.

use chrono::{TimeDelta, TimeZone, Utc};

use tonans_core::engine::{self, EngineInput, EngineOptions};
use tonans_core::schema::{BatterySpec, DispatchPolicy, InverterSpec, Scenario, ScheduleEntry};
use tonans_core::timegrid::IntervalGrid;

const TOLERANCE: f64 = 0.01;

fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= TOLERANCE,
        "expected {expected} +/- {TOLERANCE}, got {actual}"
    );
}

#[test]
fn normalized_ssc_capacity_reference_reaches_the_same_floor_and_throughput() {
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let grid = IntervalGrid::new(start, 60, 4, chrono_tz::UTC).unwrap();
    let requested_kw = [1.5, 3.0, 490.0, 490.0];
    let entries = requested_kw
        .iter()
        .enumerate()
        .map(|(index, power)| ScheduleEntry {
            start_utc: start + TimeDelta::hours(index as i64),
            end_utc: start + TimeDelta::hours(index as i64 + 1),
            battery_power_kw: *power,
        })
        .collect();

    let mut scenario = Scenario::no_storage_baseline();
    scenario.id = "ssc-capacity-reference".to_string();
    scenario.name = "Normalized SSC capacity reference".to_string();
    scenario.battery = Some(BatterySpec {
        nameplate_energy_kwh: 1_000.0,
        max_charge_kw: 1_000.0,
        max_discharge_kw: 1_000.0,
        soc_min_pct: 15.0,
        soc_max_pct: 95.0,
        reserve_soc_pct: 15.0,
        charge_efficiency_pct: 100.0,
        discharge_efficiency_pct: 100.0,
        self_discharge_pct_per_month: 0.0,
        temperature_derate: None,
    });
    scenario.inverter = InverterSpec {
        battery_power_kw: 1_000.0,
        site_export_limit_kw: None,
    };
    scenario.dispatch = DispatchPolicy::ManualSchedule { entries };
    scenario.initial_soc_pct = 50.0;

    let zero = [0.0; 4];
    let periods = [0; 4];
    let labels = ["all".to_string()];
    let outage = [false; 4];
    let result = engine::run(
        &scenario,
        &EngineInput {
            grid: &grid,
            load_kw: &requested_kw,
            critical_load_kw: &zero,
            generation_kw: &zero,
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

    let expected_stored = [498.5, 495.5, 150.0, 150.0];
    let expected_soc = [49.85, 49.55, 15.0, 15.0];
    let expected_discharge = [1.5, 3.0, 345.5, 0.0];
    for (index, row) in result.timeline.iter().enumerate() {
        close(row.stored_energy_kwh, expected_stored[index]);
        close(row.soc_pct, expected_soc[index]);
        close(row.battery_discharge_kwh, expected_discharge[index]);
    }

    close(result.energy.initial_stored_energy_kwh, 500.0);
    close(result.energy.final_stored_energy_kwh, 150.0);
    close(result.energy.battery_discharge_kwh, 350.0);
    close(result.energy.grid_import_kwh, 634.5);
    close(result.energy.equivalent_full_cycles, 0.4375);
    close(result.energy.conversion_loss_kwh, 0.0);
    close(result.energy.self_discharge_loss_kwh, 0.0);
    close(result.energy.energy_balance_residual_kwh, 0.0);

    let clamp_intervals = result
        .timeline
        .iter()
        .zip(requested_kw)
        .filter(|(row, requested)| row.battery_discharge_kwh + TOLERANCE < *requested)
        .count();
    assert_eq!(clamp_intervals, 2);
}
