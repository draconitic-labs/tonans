//! A year of data can touch 13 calendar months, and each is billed in full.
//!
//! These are the three starts an independent review tried, on the same 8,760 hourly rows. The
//! figures are from that review and stay as they were: what changes is that a count other
//! than 12 says so.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Timelike, Utc};
use serde_json::json;
use tonans_core::canonical::canonical_hash;
use tonans_core::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
use tonans_core::run::{self, RunResult};
use tonans_core::schema::{GapPolicy, Project, SeriesKind, Site};
use tonans_core::sensitivity::{self, SensitivityAxis, SensitivityParam, SensitivityRequest};

const CODE: &str = "cost.billed_months_not_twelve";

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_767_225_600, 0).unwrap()
}

fn project(start: DateTime<Utc>, timezone: &str) -> Project {
    project_of_hours(start, timezone, 8760)
}

fn project_of_hours(start: DateTime<Utc>, timezone: &str, hours: i64) -> Project {
    let mut csv = String::from("t,load\n");
    for hour in 0..hours {
        let t = start + Duration::hours(hour);
        let load = 50 + if (9..17).contains(&t.hour()) { 50 } else { 0 };
        csv.push_str(&format!("{},{load}\n", t.format("%Y-%m-%dT%H:%M:%SZ")));
    }
    let mut series = BTreeMap::new();
    series.insert(
        SeriesKind::Load,
        MappedColumn {
            header: "load".to_string(),
            unit: ImportUnit::Kilowatt,
        },
    );
    let report = csvimport::import(
        csv.as_bytes(),
        &ImportRequest {
            filename: "year.csv".to_string(),
            timezone: timezone.to_string(),
            interval_minutes: 60,
            gap_policy: GapPolicy::Reject,
            mapping: ColumnMapping {
                timestamp: "t".to_string(),
                series,
                demand_period: None,
                ignore_columns: Vec::new(),
            },
        },
        now(),
    )
    .unwrap();

    let site = Site {
        timezone: timezone.to_string(),
        currency: "USD".to_string(),
    };
    let mut project = Project::new("year", "Year", site, now()).unwrap();
    project.dataset = Some(report.dataset);

    // The same two scenarios at each start, as plain data so they read like the review's.
    let mut value = serde_json::to_value(&project).unwrap();
    let baseline = &mut value["scenarios"][0];
    baseline["tariff"] = json!({
        "import": { "source": "flat", "price_per_kwh_usd": 0.1 },
        "export": { "source": "flat", "price_per_kwh_usd": 0.0 },
        "import_price_multiplier": 1.0,
        "export_price_multiplier": 1.0,
        "demand_prices_per_kw_month_usd": { "all": 10.0 },
        "fixed_charge_usd_per_month": 100.0,
    });
    let mut candidate = baseline.clone();
    candidate["id"] = json!("cand");
    candidate["name"] = json!("cand");
    candidate["initial_soc_pct"] = json!(50.0);
    candidate["dispatch"] = json!({
        "policy": "peak_limit",
        "import_limit_kw": 80.0,
        "allow_grid_charge": true,
        "grid_charge_target_soc_pct": 90.0,
    });
    candidate["inverter"] = json!({ "battery_power_kw": 50.0, "site_export_limit_kw": null });
    candidate["battery"] = json!({
        "nameplate_energy_kwh": 400.0,
        "max_charge_kw": 50.0,
        "max_discharge_kw": 50.0,
        "soc_min_pct": 10.0,
        "soc_max_pct": 100.0,
        "reserve_soc_pct": 10.0,
        "charge_efficiency_pct": 95.0,
        "discharge_efficiency_pct": 95.0,
        "self_discharge_pct_per_month": 0.0,
    });
    candidate["economics"]["capex_usd"] = json!(100_000.0);
    value["scenarios"].as_array_mut().unwrap().push(candidate);
    let project: Project = serde_json::from_value(value).unwrap();
    project.validate().unwrap();
    project
}

fn start(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
}

fn warned(result: &RunResult) -> Vec<String> {
    result
        .all_warnings()
        .into_iter()
        .filter(|warning| warning.code == CODE)
        .map(|warning| warning.message)
        .collect()
}

/// Months billed, then the baseline's annual grid cost, the year-one saving, and the NPV.
fn figures(result: &RunResult) -> (usize, f64, f64, f64) {
    let cand = result.scenario("cand").unwrap();
    (
        cand.cost.billed_month_count,
        cand.economics.baseline_annual_grid_cost_usd,
        cand.economics.year_one_gross_savings_usd,
        cand.economics.npv_usd,
    )
}

fn close(actual: f64, expected: f64, within: f64, what: &str) {
    assert!(
        (actual - expected).abs() < within,
        "{what}: {actual} against {expected}"
    );
}

#[test]
fn a_year_from_the_first_of_a_month_bills_twelve_and_says_nothing() {
    let result = run::run_project(&project(start(2023, 1, 1), "UTC"), now()).unwrap();
    let (months, cost, saving, npv) = figures(&result);
    assert_eq!(months, 12);
    close(cost, 71_600.0, 1.0, "annual grid cost");
    close(saving, 1_752.0, 1.0, "year-one saving");
    close(npv, -83_790.0, 1.0, "net present value");
    assert!(warned(&result).is_empty());
}

#[test]
fn a_year_from_the_middle_of_a_month_bills_thirteen_and_says_so() {
    let result = run::run_project(&project(start(2023, 7, 15), "UTC"), now()).unwrap();
    let (months, cost, saving, npv) = figures(&result);
    assert_eq!(months, 13);
    close(cost, 72_700.0, 1.0, "annual grid cost");
    close(saving, 1_952.0, 1.0, "year-one saving");
    close(npv, -81_848.0, 1.0, "net present value");
    let messages = warned(&result);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("13 calendar months"), "{messages:?}");
    assert!(messages[0].contains("were billed"), "{messages:?}");
    assert!(messages[0].contains("full demand charge and a full fixed charge"));
}

#[test]
fn a_year_stamped_in_utc_for_a_site_elsewhere_bills_thirteen_and_says_so() {
    let result = run::run_project(&project(start(2023, 1, 1), "America/New_York"), now()).unwrap();
    let (months, cost, saving, npv) = figures(&result);
    assert_eq!(months, 13);
    close(cost, 72_200.0, 1.0, "annual grid cost");
    close(saving, 1_452.0, 1.0, "year-one saving");
    close(npv, -86_704.0, 1.0, "net present value");
    assert!(warned(&result)[0].contains("at the site's time zone"));
}

#[test]
fn the_warning_reaches_a_study_but_not_the_numeric_identity() {
    let thirteen = project(start(2023, 7, 15), "UTC");
    let result = run::run_project(&thirteen, now()).unwrap();

    // Warnings are outside the results hash, so this one cannot move it.
    let hash = result.results_hash().unwrap();
    let mut without = result.clone();
    for scenario in &mut without.scenarios {
        scenario.warnings.retain(|warning| warning.code != CODE);
    }
    assert_eq!(without.results_hash().unwrap(), hash);

    // A study's own identity covers its warnings, so it carries this one, and a year
    // touching exactly twelve months carries nothing new.
    let request = |_: &Project| SensitivityRequest {
        scenario_id: "cand".to_string(),
        axes: vec![SensitivityAxis {
            param: SensitivityParam::ReserveSocPct,
            values: vec![10.0, 20.0],
        }],
        capex_per_kwh_usd: None,
    };
    let study = sensitivity::run(&thirteen, &request(&thirteen)).unwrap();
    assert!(study.warnings.iter().any(|warning| warning.code == CODE));
    let mut quieter = study.clone();
    quieter.warnings.retain(|warning| warning.code != CODE);
    assert_ne!(
        canonical_hash(&study).unwrap(),
        canonical_hash(&quieter).unwrap()
    );

    let twelve = project(start(2023, 1, 1), "UTC");
    let study = sensitivity::run(&twelve, &request(&twelve)).unwrap();
    assert!(!study.warnings.iter().any(|warning| warning.code == CODE));
}

#[test]
fn the_report_and_the_export_carry_the_warning() {
    let project = project(start(2023, 7, 15), "UTC");
    let result = run::run_project(&project, now()).unwrap();
    let directory = std::env::temp_dir().join(format!("tonans-months-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    tonans_core::export::write_bundle(&directory, &project, &result, &[]).unwrap();
    for file in ["report.html", "provenance.json", "summary.json"] {
        let text = std::fs::read_to_string(directory.join(file)).unwrap();
        assert!(text.contains(CODE), "{file}");
        assert!(
            text.contains("13 calendar months at the site&#39;s time zone were billed")
                || text.contains("13 calendar months at the site's time zone were billed"),
            "{file}"
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_short_file_says_how_many_months_it_billed_in_the_singular() {
    let mut project = project(start(2023, 1, 1), "UTC");
    let dataset = project.dataset.as_mut().unwrap();
    for series in dataset.series.values_mut() {
        series.values.truncate(3);
    }
    dataset.interval_count = 3;
    project.validate().unwrap();
    let result = run::run_project(&project, now()).unwrap();
    assert_eq!(
        warned(&result)[0],
        "1 calendar month at the site's time zone was billed, with a full demand charge and a \
         full fixed charge however little of the month the data covers."
    );
}

/// The whole message, so that nothing beyond the count and the charges creeps back in.
fn says(project: &Project, months: usize, expected: &str) {
    let result = run::run_project(project, now()).unwrap();
    assert_eq!(figures(&result).0, months);
    let messages = warned(&result);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0], expected);
}

const BOTH: &str = "each with a full demand charge and a full fixed charge however little of the \
                    month the data covers.";

#[test]
fn the_warning_is_true_of_a_year_stamped_at_the_end_of_each_hour() {
    let hour_ending = start(2023, 1, 1) + Duration::hours(1);
    says(
        &project(hour_ending, "UTC"),
        13,
        &format!("13 calendar months at the site's time zone were billed, {BOTH}"),
    );
}

#[test]
fn the_warning_is_true_of_two_whole_years() {
    says(
        &project_of_hours(start(2022, 1, 1), "UTC", 2 * 8760),
        24,
        &format!("24 calendar months at the site's time zone were billed, {BOTH}"),
    );
}

#[test]
fn the_warning_is_true_of_a_short_file_from_the_first() {
    says(
        &project_of_hours(start(2023, 1, 1), "UTC", 90 * 24),
        3,
        &format!("3 calendar months at the site's time zone were billed, {BOTH}"),
    );
}

#[test]
fn the_warning_names_only_the_charges_that_apply() {
    let with_charges = |demand: f64, fixed: f64| {
        let mut project = project(start(2023, 7, 15), "UTC");
        for scenario in &mut project.scenarios {
            for price in scenario.tariff.demand_prices_per_kw_month_usd.values_mut() {
                *price = demand;
            }
            scenario.tariff.fixed_charge_usd_per_month = fixed;
        }
        project.validate().unwrap();
        project
    };
    says(
        &with_charges(0.0, 0.0),
        13,
        "13 calendar months at the site's time zone were billed. No demand charge or fixed \
         charge applies.",
    );
    says(
        &with_charges(10.0, 0.0),
        13,
        "13 calendar months at the site's time zone were billed, each with a full demand charge \
         however little of the month the data covers.",
    );
    says(
        &with_charges(0.0, 100.0),
        13,
        "13 calendar months at the site's time zone were billed, each with a full fixed charge \
         however little of the month the data covers.",
    );
}
