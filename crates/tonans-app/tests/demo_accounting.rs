//! What the bundled demo must keep saying, from outside the model.

use chrono::{DateTime, Utc};
use tonans_app::demo;
use tonans_core::run;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_767_225_600, 0).unwrap()
}

#[test]
fn year_fifteen_capacity_is_the_compounding_figure_not_the_straight_line() {
    let project = demo::build(now()).unwrap();
    let result = run::run_project(&project, now()).unwrap();
    let economics = &result.scenario("peak-limit").unwrap().economics;
    let year_15 = economics.cash_flow[14].capacity_retention_pct;
    assert!((year_15 - 73.25).abs() < 0.01, "{year_15}");

    // The same figure from the formula in docs/MODEL.md, with the rate worked back from year 2.
    let initial = economics.cash_flow[0].capacity_retention_pct;
    let factor = economics.cash_flow[1].capacity_retention_pct / initial;
    assert!((year_15 - initial * factor.powi(14)).abs() < 1e-6);
    let straight_line = initial * (1.0 - 14.0 * (1.0 - factor));
    assert!((straight_line - 69.21).abs() < 0.01, "{straight_line}");
}

#[test]
fn the_demo_year_touches_twelve_months_and_raises_nothing_about_it() {
    let result = run::run_project(&demo::build(now()).unwrap(), now()).unwrap();
    for scenario in &result.scenarios {
        assert_eq!(scenario.cost.billed_month_count, 12);
    }
    assert!(!result
        .all_warnings()
        .iter()
        .any(|warning| warning.code == "cost.billed_months_not_twelve"));
}

/// Floating point results are only promised per build target, so this is pinned on one.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn the_demo_hashes_have_not_moved() {
    let result = run::run_project(&demo::build(now()).unwrap(), now()).unwrap();
    assert_eq!(
        result.model_input_hash.as_str(),
        "sha256:97029af07baaf3143a8afe551d56028d485d5e7bd8bc9a35e8487a0fc571a79e"
    );
    assert_eq!(
        result.results_hash().unwrap().as_str(),
        "sha256:b9512228beb1b6a11e46bdc5285516fb67fd89edc2441575fc29044dbf90abba"
    );
}
