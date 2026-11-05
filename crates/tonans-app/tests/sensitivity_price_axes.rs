//! A study over a price compares the candidate and the baseline at that price.
//!
//! Each cell here is checked two ways: against setting both scenarios to the price and
//! running them, and against the figures an independent review worked out from the
//! bundled demo. Before model 1.2.0 the demand-price row of this table read -53,815,
//! -152,593, -251,372, -350,150, which pointed the opposite way.

use chrono::{DateTime, Utc};
use tonans_app::demo;
use tonans_core::run;
use tonans_core::schema::Project;
use tonans_core::sensitivity::{self, SensitivityAxis, SensitivityParam, SensitivityRequest};

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_767_225_600, 0).unwrap()
}

/// The net present value of the demo's peak-limit scenario, with every scenario set up by
/// `set`, through an ordinary run.
fn npv_with_every_scenario(set: impl Fn(&mut tonans_core::schema::Scenario)) -> f64 {
    let mut project: Project = demo::build(now()).unwrap();
    for scenario in &mut project.scenarios {
        set(scenario);
    }
    let result = run::run_project(&project, now()).unwrap();
    result.scenario("peak-limit").unwrap().economics.npv_usd
}

fn study(param: SensitivityParam, values: &[f64]) -> Vec<f64> {
    let project = demo::build(now()).unwrap();
    let result = sensitivity::run(
        &project,
        &SensitivityRequest {
            scenario_id: "peak-limit".to_string(),
            axes: vec![SensitivityAxis {
                param,
                values: values.to_vec(),
            }],
            capex_per_kwh_usd: None,
        },
    )
    .unwrap();
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.code == "sensitivity.prices_move_baseline"));
    result
        .cells
        .iter()
        .map(|cell| cell.metrics.npv_usd)
        .collect()
}

fn assert_table(cells: &[f64], by_hand: &[f64], reviewed: &[f64]) {
    for ((cell, hand), expected) in cells.iter().zip(by_hand).zip(reviewed) {
        assert_eq!(
            cell, hand,
            "the cell differs from running both at that price"
        );
        assert!(
            (cell - expected).abs() < 0.5,
            "the cell reads {cell}, and the reviewed figure is {expected}"
        );
    }
}

#[test]
fn a_demand_price_sweep_equals_running_both_scenarios_at_that_price() {
    let values = [5.0, 10.0, 15.0, 20.0];
    let by_hand: Vec<f64> = values
        .iter()
        .map(|price| {
            npv_with_every_scenario(|scenario| {
                for period in scenario.tariff.demand_prices_per_kw_month_usd.values_mut() {
                    *period = *price;
                }
            })
        })
        .collect();
    assert_table(
        &study(SensitivityParam::DemandPricePerKwMonthUsd, &values),
        &by_hand,
        &[-204_854.0, -165_903.0, -126_951.0, -88_000.0],
    );
}

#[test]
fn an_import_price_sweep_equals_running_both_scenarios_at_that_price() {
    let values = [0.8, 1.0, 1.2, 1.5];
    let by_hand: Vec<f64> = values
        .iter()
        .map(|multiplier| {
            npv_with_every_scenario(|scenario| {
                scenario.tariff.import_price_multiplier = *multiplier;
            })
        })
        .collect();
    assert_table(
        &study(SensitivityParam::ImportPriceMultiplier, &values),
        &by_hand,
        &[-181_118.0, -181_557.0, -181_996.0, -182_654.0],
    );
}

#[test]
fn a_higher_demand_price_makes_peak_shaving_worth_more_not_less() {
    // The direction is the finding. A table of numbers can drift; this cannot.
    let cells = study(SensitivityParam::DemandPricePerKwMonthUsd, &[5.0, 20.0]);
    assert!(cells[1] > cells[0], "{cells:?}");
}

#[test]
fn a_size_axis_leaves_the_baseline_alone_so_the_cell_matches_a_run() {
    // At the scenario's own size and cost, a study cell is the scenario's own result.
    let project = demo::build(now()).unwrap();
    let result = sensitivity::run(
        &project,
        &SensitivityRequest {
            scenario_id: "peak-limit".to_string(),
            axes: vec![SensitivityAxis {
                param: SensitivityParam::BatteryEnergyKwh,
                values: vec![400.0],
            }],
            capex_per_kwh_usd: Some(220_000.0 / 400.0),
        },
    )
    .unwrap();
    let npv = run::run_project(&project, now())
        .unwrap()
        .scenario("peak-limit")
        .unwrap()
        .economics
        .npv_usd;
    assert!((result.cells[0].metrics.npv_usd - npv).abs() < 0.005);
    assert!((result.cells[0].metrics.installed_cost_usd - 220_000.0).abs() < 0.005);
}

/// With the rate the page sends, a study's cell at the scenario's own size is that
/// scenario's results to the cent, including after its cost has been edited, and with a
/// cost and size that do not divide into a whole number. The page used to round the rate
/// and to keep the first one it saw.
#[test]
fn a_study_at_the_exact_rate_matches_results_to_the_cent() {
    for (capex, nameplate) in [(220_000.0, 400.0), (440_000.0, 400.0), (123_457.0, 333.3)] {
        let mut project = demo::build(now()).unwrap();
        let scenario = project
            .scenarios
            .iter_mut()
            .find(|s| s.id == "peak-limit")
            .unwrap();
        scenario.economics.capex_usd = capex;
        scenario.battery.as_mut().unwrap().nameplate_energy_kwh = nameplate;

        let results = run::run_project(&project, now()).unwrap();
        let expected = results.scenario("peak-limit").unwrap().economics.npv_usd;

        let study = sensitivity::run(
            &project,
            &SensitivityRequest {
                scenario_id: "peak-limit".to_string(),
                axes: vec![SensitivityAxis {
                    param: SensitivityParam::BatteryEnergyKwh,
                    values: vec![nameplate],
                }],
                capex_per_kwh_usd: Some(capex / nameplate),
            },
        )
        .unwrap();
        let cell = &study.cells[0].metrics;
        assert!(
            (cell.npv_usd - expected).abs() < 0.005,
            "at {capex} USD and {nameplate} kWh the cell reads {} and Results reads {expected}",
            cell.npv_usd
        );
        assert!((cell.installed_cost_usd - capex).abs() < 0.005);
    }
}
