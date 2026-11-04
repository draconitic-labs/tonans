//! The self-contained HTML report.
//!
//! One file. No script, no network, no fonts to fetch, nothing to execute — it
//! carries its own `Content-Security-Policy` of `default-src 'none'` and it opens
//! from a filesystem, from an email attachment, or from a USB stick in ten years'
//! time. Every chart in it was rendered to SVG by [`crate::svg`] before it was
//! written.
//!
//! The report is written to be *read against*, not merely admired. It leads with
//! what the analysis is not, it prints the energy-balance residual so a reader can
//! check that the model closed, it shows the assumptions next to the numbers they
//! produced, and it ends with the hashes needed to reproduce it. Every warning the
//! run raised is on the page — none is filtered out for looking bad.

use std::fmt::Write as _;

use crate::error::{Result, Severity, Warning};
use crate::provenance::Provenance;
use crate::run::{RunResult, ScenarioResult};
use crate::schema::{DispatchPolicy, Project, RateSource, SeriesKind};
use crate::sensitivity::SensitivityResult;
use crate::svg::{self, escape, format_number, FlowSegment, Panel, Role, Scale, Series};
use crate::units::round_money;
use crate::version::{PLANNING_DISCLAIMER, PRODUCT_NAME, SUPPORT_EMAIL, VENDOR_NAME};

/// Everything the report renders from.
#[derive(Debug, Clone)]
pub struct ReportInput<'a> {
    /// The project.
    pub project: &'a Project,
    /// The run.
    pub run: &'a RunResult,
    /// Sensitivity studies to include. May be empty.
    pub sensitivities: &'a [SensitivityResult],
    /// The provenance manifest, minus artifact hashes (they do not exist yet).
    pub provenance: &'a Provenance,
}

const CHART_WIDTH: f64 = 880.0;

/// Render the report.
pub fn render(input: &ReportInput<'_>) -> Result<String> {
    let run = input.run;
    let currency = &input.project.site.currency;
    let mut html = String::with_capacity(256 * 1024);

    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    // The report must never fetch anything, and must never execute anything. This
    // is enforced by the document itself, not merely by its content.
    html.push_str(
        r#"<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'">"#,
    );
    html.push_str("\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str("<meta name=\"robots\" content=\"noindex, nofollow\">\n");
    let _ = writeln!(
        html,
        "<title>{product} report — {project}</title>",
        product = PRODUCT_NAME,
        project = escape(&run.project_name)
    );
    let _ = writeln!(html, "<style>{}</style>", stylesheet());
    html.push_str("</head>\n<body>\n<main class=\"page\">\n");

    header(&mut html, input);
    disclaimer_banner(&mut html);
    warning_ledger(&mut html, &run.all_warnings());
    comparison_table(&mut html, run, currency);

    for scenario in &run.scenarios {
        scenario_section(&mut html, input, scenario, currency);
    }

    for study in input.sensitivities {
        sensitivity_section(&mut html, study, currency);
    }

    provenance_section(&mut html, input);
    model_limits(&mut html, run);
    footer(&mut html, run);

    html.push_str("</main>\n</body>\n</html>\n");
    Ok(html)
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn header(html: &mut String, input: &ReportInput<'_>) {
    let run = input.run;
    let _ = write!(
        html,
        r#"<header class="head">
<div class="brand">{product} <span class="muted">by {vendor}</span></div>
<h1>{project}</h1>
<dl class="stamp">
<div><dt>Generated</dt><dd>{generated} UTC</dd></div>
<div><dt>Application</dt><dd>{app}</dd></div>
<div><dt>Model</dt><dd>{model}</dd></div>
<div><dt>Schema</dt><dd>v{schema}</dd></div>
</dl>
</header>
"#,
        product = PRODUCT_NAME,
        vendor = VENDOR_NAME,
        project = escape(&run.project_name),
        generated = escape(&run.generated_utc.format("%Y-%m-%d %H:%M:%S").to_string()),
        app = escape(&run.app_version),
        model = escape(&run.model_version),
        schema = run.schema_version,
    );
}

fn disclaimer_banner(html: &mut String) {
    let _ = writeln!(
        html,
        r#"<section class="banner" role="note"><strong>Planning estimate.</strong> {disclaimer}</section>"#,
        disclaimer =
            escape(PLANNING_DISCLAIMER.trim_start_matches("Planning estimate for human review. "))
    );
}

fn warning_ledger(html: &mut String, warnings: &[Warning]) {
    html.push_str("<section id=\"warnings\">\n<h2>Warning ledger</h2>\n");
    if warnings.is_empty() {
        html.push_str("<p class=\"muted\">No warnings were raised. Every assumption is still an assumption; see the model limits below.</p>\n</section>\n");
        return;
    }
    let _ = write!(
        html,
        "<p class=\"muted\">{count} warning(s), most serious first. Nothing here is filtered out for being inconvenient.</p>\n<ul class=\"ledger\">\n",
        count = warnings.len()
    );
    ledger_items(html, warnings);
    html.push_str("</ul>\n</section>\n");
}

fn ledger_items(html: &mut String, warnings: &[Warning]) {
    for warning in warnings {
        // Status colour never carries meaning alone: the severity is spelled out.
        let _ = writeln!(
            html,
            r#"<li class="sev-{severity}"><span class="pill">{icon} {severity}</span> <code>{code}</code> {message}</li>"#,
            severity = warning.severity,
            icon = severity_icon(warning.severity),
            code = escape(&warning.code),
            message = escape(&warning.message)
        );
    }
}

fn severity_icon(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "i",
        Severity::Caution => "!",
        Severity::Critical => "x",
    }
}

fn comparison_table(html: &mut String, run: &RunResult, currency: &str) {
    html.push_str("<section id=\"comparison\">\n<h2>Scenario comparison</h2>\n");
    let _ = writeln!(
        html,
        "<p class=\"muted\">Every figure is compared against the baseline scenario <strong>{baseline}</strong>. A negative cost change is a saving.</p>",
        baseline = escape(
            run.baseline()
                .map_or(run.baseline_scenario_id.as_str(), |s| s.scenario_name.as_str())
        )
    );

    html.push_str("<div class=\"scroll\"><table>\n<thead><tr>");
    for heading in [
        "Scenario",
        "Dispatch policy",
        "Grid cost (annual)",
        "Change vs baseline",
        "Peak import (kW)",
        "Grid import (kWh)",
        "Self-sufficiency (%)",
        "Unmet critical (kWh)",
        "NPV",
        "Simple payback (years)",
    ] {
        let _ = write!(html, "<th>{}</th>", escape(heading));
    }
    html.push_str("</tr></thead>\n<tbody>\n");

    for scenario in &run.scenarios {
        let economics = &scenario.economics;
        let comparison = &scenario.comparison;
        let _ = write!(
            html,
            r#"<tr{highlight}>
<td><span class="swatch" style="background:{colour}"></span>{name}{badge}</td>
<td>{policy}</td>
<td class="num">{cost}</td>
<td class="num {delta_class}">{delta}</td>
<td class="num">{peak}</td>
<td class="num">{import}</td>
<td class="num">{sufficiency}</td>
<td class="num {unmet_class}">{unmet}</td>
<td class="num">{npv}</td>
<td class="num">{payback}</td>
</tr>
"#,
            highlight = if comparison.is_baseline {
                r#" class="baseline""#
            } else {
                ""
            },
            colour = Role::Load.css_var(),
            name = escape(&scenario.scenario_name),
            badge = if comparison.is_baseline {
                " <span class=\"tag\">baseline</span>"
            } else {
                ""
            },
            policy = escape(&scenario.dispatch_policy),
            cost = money(economics.scenario_annual_grid_cost_usd, currency),
            delta_class = if comparison.is_baseline {
                ""
            } else if comparison.grid_cost_delta_usd < 0.0 {
                "good"
            } else {
                "bad"
            },
            delta = if comparison.is_baseline {
                "—".to_string()
            } else {
                money(
                    comparison.grid_cost_delta_usd * economics.annualization_multiplier,
                    currency,
                )
            },
            peak = escape(&format_number(scenario.energy.peak_import_kw)),
            import = escape(&format_number(scenario.energy.grid_import_kwh)),
            sufficiency = escape(&format!("{:.1}", scenario.energy.self_sufficiency_pct)),
            unmet_class = if scenario.energy.unmet_critical_kwh > 1e-9 {
                "bad"
            } else {
                ""
            },
            unmet = escape(&format_number(scenario.energy.unmet_critical_kwh)),
            npv = if scenario.scenario.battery.is_some() || economics.capex_usd > 0.0 {
                money(economics.npv_usd, currency)
            } else {
                "—".to_string()
            },
            payback = economics
                .simple_payback_years
                .map_or_else(|| "never".to_string(), |y| format!("{y:.1}")),
        );
    }
    html.push_str("</tbody>\n</table></div>\n</section>\n");
}

fn scenario_section(
    html: &mut String,
    input: &ReportInput<'_>,
    scenario: &ScenarioResult,
    currency: &str,
) {
    let _ = write!(
        html,
        "<section class=\"scenario\">\n<h2>{name}</h2>\n<p class=\"muted\">Dispatch policy: <strong>{policy}</strong>. {baseline}</p>\n",
        name = escape(&scenario.scenario_name),
        policy = escape(&scenario.dispatch_policy),
        baseline = if scenario.comparison.is_baseline {
            "This is the baseline every other scenario is measured against."
        } else {
            ""
        }
    );

    assumptions_table(html, scenario, currency);
    energy_flow_figure(html, scenario);
    timeline_figure(html, input, scenario);
    energy_table(html, scenario);
    cost_table(html, scenario, currency);
    outage_figure(html, scenario);
    economics_table(html, scenario, currency);

    html.push_str("</section>\n");
}

fn assumptions_table(html: &mut String, scenario: &ScenarioResult, currency: &str) {
    let spec = &scenario.scenario;
    let mut rows: Vec<(String, String)> = Vec::new();

    match &spec.battery {
        None => rows.push(("Battery".into(), "none — grid only".into())),
        Some(battery) => {
            rows.push((
                "Battery energy".into(),
                format!(
                    "{} kWh nameplate",
                    format_number(battery.nameplate_energy_kwh)
                ),
            ));
            rows.push((
                "Battery power".into(),
                format!(
                    "{} kW charge / {} kW discharge, at the AC bus",
                    format_number(battery.max_charge_kw),
                    format_number(battery.max_discharge_kw)
                ),
            ));
            rows.push((
                "Inverter rating".into(),
                format!("{} kW", format_number(spec.inverter.battery_power_kw)),
            ));
            rows.push((
                "Usable envelope".into(),
                format!(
                    "{}% to {}% state of charge, reserve floor {}%",
                    format_number(battery.soc_min_pct),
                    format_number(battery.soc_max_pct),
                    format_number(battery.reserve_soc_pct)
                ),
            ));
            rows.push((
                "Efficiency".into(),
                format!(
                    "{}% charge, {}% discharge ({}% round trip). All conversion loss is in these two figures; the inverter adds a power ceiling and no loss.",
                    format_number(battery.charge_efficiency_pct),
                    format_number(battery.discharge_efficiency_pct),
                    format_number(battery.round_trip_efficiency_pct())
                ),
            ));
            rows.push((
                "Standing loss".into(),
                format!(
                    "{}% of stored energy per 730-hour month",
                    format_number(battery.self_discharge_pct_per_month)
                ),
            ));
            rows.push((
                "Initial state".into(),
                format!(
                    "{}% state of charge, {}% of nameplate capacity",
                    format_number(spec.initial_soc_pct),
                    format_number(spec.initial_capacity_pct)
                ),
            ));
            rows.push((
                "Temperature derating".into(),
                match &battery.temperature_derate {
                    None => "none".to_string(),
                    Some(derate) => format!(
                        "{} point table, interpolated linearly, held constant outside it",
                        derate.points.len()
                    ),
                },
            ));
        }
    }

    rows.push((
        "Export limit".into(),
        spec.inverter.site_export_limit_kw.map_or_else(
            || "unlimited".to_string(),
            |kw| format!("{} kW", format_number(kw)),
        ),
    ));
    rows.push(("Dispatch".into(), describe_policy(&spec.dispatch)));
    rows.push((
        "Import price".into(),
        describe_rate(
            &spec.tariff.import,
            spec.tariff.import_price_multiplier,
            currency,
        ),
    ));
    rows.push((
        "Export credit".into(),
        describe_rate(
            &spec.tariff.export,
            spec.tariff.export_price_multiplier,
            currency,
        ),
    ));
    if spec.tariff.demand_prices_per_kw_month_usd.is_empty() {
        rows.push(("Demand charge".into(), "none".into()));
    } else {
        rows.push((
            "Demand charge".into(),
            spec.tariff
                .demand_prices_per_kw_month_usd
                .iter()
                .map(|(period, price)| format!("{period}: {price:.2} {currency}/kW-month"))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    if spec.tariff.fixed_charge_usd_per_month > 0.0 {
        rows.push((
            "Fixed charge".into(),
            format!(
                "{:.2} {currency} per month",
                spec.tariff.fixed_charge_usd_per_month
            ),
        ));
    }
    if spec.economics.capex_usd > 0.0 || spec.battery.is_some() {
        rows.push((
            "Capital".into(),
            format!("{} installed", money(spec.economics.capex_usd, currency)),
        ));
        rows.push((
            "Operating cost".into(),
            format!(
                "{} per year",
                money(spec.economics.annual_opex_usd, currency)
            ),
        ));
        rows.push((
            "Horizon".into(),
            format!(
                "{} years at {}% discount, {}% energy escalation",
                spec.economics.horizon_years,
                format_number(spec.economics.discount_rate_pct_per_year),
                format_number(spec.economics.energy_price_escalation_pct_per_year)
            ),
        ));
        if let Some(replacement) = &spec.economics.replacement {
            rows.push((
                "Replacement".into(),
                format!(
                    "{} when usable capacity falls below {}% of nameplate",
                    money(replacement.cost_usd, currency),
                    format_number(replacement.trigger_capacity_pct)
                ),
            ));
        }
        rows.push((
            "Degradation".into(),
            scenario.economics.degradation_method.clone(),
        ));
    }
    if !spec.outage.windows.is_empty() {
        rows.push((
            "Outages".into(),
            format!(
                "{} window(s) replayed; non-critical load is assumed to be shed",
                spec.outage.windows.len()
            ),
        ));
    }

    html.push_str("<h3>Assumptions</h3>\n<div class=\"scroll\"><table class=\"kv\">\n<tbody>\n");
    for (key, value) in rows {
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\">{key}</th><td>{value}</td></tr>",
            key = escape(&key),
            value = escape(&value)
        );
    }
    html.push_str("</tbody>\n</table></div>\n");
}

fn describe_policy(policy: &DispatchPolicy) -> String {
    match policy {
        DispatchPolicy::SelfConsumption =>
            "Self-consumption. Stores surplus generation, discharges to cover net load. Never charges from the grid; never exports from the battery."
                .to_string(),
        DispatchPolicy::PeakLimit {
            import_limit_kw,
            allow_grid_charge,
            grid_charge_target_soc_pct,
        } => format!(
            "Peak limit. Discharges only to hold grid import at or below {limit} kW, and charges from surplus generation. {grid}",
            limit = format_number(*import_limit_kw),
            grid = if *allow_grid_charge {
                format!(
                    "It also charges from the grid inside the remaining import headroom, up to {target}% state of charge.",
                    target = format_number(*grid_charge_target_soc_pct)
                )
            } else {
                "It does not charge from the grid.".to_string()
            }
        ),
        DispatchPolicy::ReserveFirst {
            reserve_target_soc_pct,
            allow_grid_charge,
        } => format!(
            "Reserve first. Below {target}% state of charge the battery only charges — refilling the outage reserve outranks every saving. At or above it, it behaves as self-consumption and will not fall back below the target while the grid is up. {grid}",
            target = format_number(*reserve_target_soc_pct),
            grid = if *allow_grid_charge {
                "It may charge from the grid to reach the target."
            } else {
                "It refills from surplus generation only."
            }
        ),
        DispatchPolicy::ManualSchedule { entries } => format!(
            "Manual schedule. {count} explicit setpoint window(s); the battery is idle outside them. Setpoints are clipped to what the hardware can do, and any clipping is reported as a warning.",
            count = entries.len()
        ),
    }
}

fn describe_rate(source: &RateSource, multiplier: f64, currency: &str) -> String {
    let base = match source {
        RateSource::Flat { price_per_kwh_usd } => {
            format!("{price_per_kwh_usd:.4} {currency}/kWh, flat")
        }
        RateSource::Series => "per-interval price from the imported data".to_string(),
    };
    if (multiplier - 1.0).abs() < 1e-9 {
        base
    } else {
        format!("{base}, scaled by {multiplier}x")
    }
}

fn energy_flow_figure(html: &mut String, scenario: &ScenarioResult) {
    let energy = &scenario.energy;
    let inputs = vec![
        FlowSegment {
            role: Role::Generation,
            label: "Generation".into(),
            value_kwh: energy.generation_used_kwh,
        },
        FlowSegment {
            role: Role::GridImport,
            label: "Grid import".into(),
            value_kwh: energy.grid_import_kwh,
        },
        FlowSegment {
            role: Role::BatteryDischarge,
            label: "Battery out".into(),
            value_kwh: energy.battery_discharge_kwh,
        },
    ];
    let outputs = vec![
        FlowSegment {
            role: Role::Load,
            label: "Load served".into(),
            value_kwh: energy.served_load_kwh,
        },
        FlowSegment {
            role: Role::GridExport,
            label: "Grid export".into(),
            value_kwh: energy.grid_export_kwh,
        },
        FlowSegment {
            role: Role::BatteryCharge,
            label: "Battery in".into(),
            value_kwh: energy.battery_charge_kwh,
        },
    ];

    let _ = write!(
        html,
        "<h3>Energy flow</h3>\n<figure>{svg}<figcaption>Energy in and energy out over the whole simulated period. Stored energy changed by {delta} kWh, and {losses} kWh was lost in conversion and standing loss — which is why the two bars are not identical. The bus balance closed to {residual} kWh.</figcaption></figure>\n",
        svg = svg::energy_flow(&inputs, &outputs, CHART_WIDTH),
        delta = escape(&format_number(
            energy.final_stored_energy_kwh - energy.initial_stored_energy_kwh
        )),
        losses = escape(&format_number(energy.conversion_loss_kwh + energy.self_discharge_loss_kwh)),
        residual = escape(&format!("{:.9}", energy.energy_balance_residual_kwh))
    );
}

fn timeline_figure(html: &mut String, input: &ReportInput<'_>, scenario: &ScenarioResult) {
    if scenario.timeline.is_empty() {
        return;
    }
    let rows = &scenario.timeline;
    let hours = input
        .project
        .dataset
        .as_ref()
        .map_or(1.0, |d| f64::from(d.interval_minutes) / 60.0);

    // The engine works in kWh per interval; a reader thinks in kW. Convert once,
    // here, at the boundary where it becomes a picture.
    let to_kw = |f: fn(&crate::engine::IntervalRow) -> f64| -> Vec<f64> {
        rows.iter().map(|r| f(r) / hours).collect()
    };
    let load = to_kw(|r| r.load_kwh);
    let generation = to_kw(|r| r.generation_available_kwh);
    let import = to_kw(|r| r.grid_import_kwh);
    let export = to_kw(|r| -r.grid_export_kwh);
    let charge = to_kw(|r| -r.battery_charge_kwh);
    let discharge = to_kw(|r| r.battery_discharge_kwh);
    let soc: Vec<f64> = rows.iter().map(|r| r.soc_pct).collect();

    let mut panels = vec![
        Panel {
            title: "Load and on-site generation".into(),
            axis_unit: "kW".into(),
            series: vec![
                Series {
                    role: Role::Load,
                    label: "Load".into(),
                    values: &load,
                },
                Series {
                    role: Role::Generation,
                    label: "Generation".into(),
                    values: &generation,
                },
            ],
            signed: false,
        },
        Panel {
            title: "Grid exchange — import above zero, export below".into(),
            axis_unit: "kW".into(),
            series: vec![
                Series {
                    role: Role::GridImport,
                    label: "Import".into(),
                    values: &import,
                },
                Series {
                    role: Role::GridExport,
                    label: "Export".into(),
                    values: &export,
                },
            ],
            signed: true,
        },
    ];

    if scenario.scenario.battery.is_some() {
        panels.push(Panel {
            title: "Battery power — discharge above zero, charge below".into(),
            axis_unit: "kW".into(),
            series: vec![
                Series {
                    role: Role::BatteryDischarge,
                    label: "Discharge".into(),
                    values: &discharge,
                },
                Series {
                    role: Role::BatteryCharge,
                    label: "Charge".into(),
                    values: &charge,
                },
            ],
            signed: true,
        });
        // Its own panel, on its own axis. State of charge is a percentage; power is
        // a rate. Putting them on one axis would let the picture imply a relationship
        // that is an artefact of the scaling.
        panels.push(Panel {
            title: "State of charge".into(),
            axis_unit: "%".into(),
            series: vec![Series {
                role: Role::StateOfCharge,
                label: "State of charge".into(),
                values: &soc,
            }],
            signed: false,
        });
    }

    let labels = time_axis_labels(input, rows.len());
    let _ = write!(
        html,
        "<h3>Timelines</h3>\n<figure>{svg}<figcaption>{caption}</figcaption></figure>\n",
        svg = svg::timeline(&panels, &labels, rows.len(), CHART_WIDTH),
        caption = escape(
            "Each panel has its own axis. State of charge is never plotted against power on a shared \
             scale: a chart with two y-axes invites a comparison that is an artefact of the scaling."
        )
    );
}

fn time_axis_labels(input: &ReportInput<'_>, count: usize) -> Vec<String> {
    let Ok(Some(grid)) = input.project.grid() else {
        return Vec::new();
    };
    let timezone = grid.timezone();
    let ticks = 6.min(count.max(1));
    (0..ticks)
        .map(|i| {
            let index = if ticks > 1 {
                i * (count - 1) / (ticks - 1)
            } else {
                0
            };
            grid.start_of(index)
                .with_timezone(&timezone)
                .format("%Y-%m-%d")
                .to_string()
        })
        .collect()
}

fn energy_table(html: &mut String, scenario: &ScenarioResult) {
    let e = &scenario.energy;
    let rows: &[(&str, f64, &str)] = &[
        ("Site load", e.load_kwh, "kWh"),
        ("Load served", e.served_load_kwh, "kWh"),
        ("Critical load", e.critical_load_kwh, "kWh"),
        ("Unmet critical load", e.unmet_critical_kwh, "kWh"),
        (
            "Non-critical load shed during outages",
            e.shed_noncritical_kwh,
            "kWh",
        ),
        ("Generation available", e.generation_available_kwh, "kWh"),
        ("Generation used", e.generation_used_kwh, "kWh"),
        ("Generation to load", e.generation_to_load_kwh, "kWh"),
        ("Generation to storage", e.generation_to_battery_kwh, "kWh"),
        ("Generation to grid", e.generation_to_grid_kwh, "kWh"),
        ("Generation curtailed", e.curtailed_kwh, "kWh"),
        ("Grid import", e.grid_import_kwh, "kWh"),
        ("Grid import to load", e.grid_to_load_kwh, "kWh"),
        ("Grid import to storage", e.grid_to_battery_kwh, "kWh"),
        ("Grid export", e.grid_export_kwh, "kWh"),
        ("Battery charged (at the bus)", e.battery_charge_kwh, "kWh"),
        (
            "Battery discharged (at the bus)",
            e.battery_discharge_kwh,
            "kWh",
        ),
        ("Battery output to load", e.battery_to_load_kwh, "kWh"),
        ("Battery output to grid", e.battery_to_grid_kwh, "kWh"),
        (
            "Generation-origin battery output to load",
            e.generation_origin_battery_to_load_kwh,
            "kWh",
        ),
        (
            "Grid-origin battery output to load",
            e.grid_origin_battery_to_load_kwh,
            "kWh",
        ),
        (
            "Unknown initial battery output to load",
            e.initial_origin_battery_to_load_kwh,
            "kWh",
        ),
        ("Conversion loss", e.conversion_loss_kwh, "kWh"),
        ("Standing loss", e.self_discharge_loss_kwh, "kWh"),
        ("Peak grid import", e.peak_import_kw, "kW"),
        ("Peak grid export", e.peak_export_kw, "kW"),
        ("Lowest state of charge", e.min_soc_pct, "%"),
        ("Highest state of charge", e.max_soc_pct, "%"),
        (
            "Self-consumption of generation",
            e.self_consumption_pct,
            "%",
        ),
        (
            "Self-sufficiency from onsite generation",
            e.self_sufficiency_pct,
            "%",
        ),
        ("Equivalent full cycles", e.equivalent_full_cycles, "cycles"),
    ];

    // This table is not decoration. Three of the light-mode chart hues fall below a
    // 3:1 contrast ratio against the page, which is only permissible if the values
    // are readable another way. This is that way.
    html.push_str("<h3>Energy</h3>\n<div class=\"scroll\"><table class=\"num-table\">\n<thead><tr><th>Quantity</th><th class=\"num\">Value</th><th>Unit</th></tr></thead>\n<tbody>\n");
    for (label, value, unit) in rows {
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\">{label}</th><td class=\"num\">{value}</td><td class=\"unit\">{unit}</td></tr>",
            label = escape(label),
            value = escape(&format_number(*value)),
            unit = escape(unit)
        );
    }
    let _ = writeln!(
        html,
        "<tr class=\"check\"><th scope=\"row\">Energy balance residual</th><td class=\"num\">{residual}</td><td class=\"unit\">kWh</td></tr>",
        residual = escape(&format!("{:.9}", e.energy_balance_residual_kwh))
    );
    html.push_str("</tbody>\n</table></div>\n<p class=\"muted\">The residual is the model checking itself: generation used, plus import, plus discharge, minus load served, export, and charge, summed over every interval. It should be zero to within floating-point rounding. If it is not, nothing above is trustworthy.</p>\n");
}

fn cost_table(html: &mut String, scenario: &ScenarioResult, currency: &str) {
    let c = &scenario.cost;
    html.push_str("<h3>Grid cost over the simulated period</h3>\n<div class=\"scroll\"><table class=\"num-table\">\n<tbody>\n");
    for (label, value) in [
        ("Imported energy", c.import_energy_cost_usd),
        ("Export credit", -c.export_credit_usd),
        ("Demand charge", c.demand_charge_usd),
        ("Fixed charge", c.fixed_charge_usd),
    ] {
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\">{label}</th><td class=\"num\">{value}</td></tr>",
            label = escape(label),
            value = money(value, currency)
        );
    }
    let _ = write!(
        html,
        "<tr class=\"total\"><th scope=\"row\">Total</th><td class=\"num\">{total}</td></tr>\n</tbody>\n</table></div>\n",
        total = money(c.total_grid_cost_usd, currency)
    );

    if !c.demand_charge_lines.is_empty() {
        html.push_str("<h4>Demand charge by month</h4>\n<div class=\"scroll\"><table class=\"num-table\">\n<thead><tr><th>Month</th><th>Period</th><th class=\"num\">Peak import (kW)</th><th class=\"num\">Price</th><th class=\"num\">Charge</th></tr></thead>\n<tbody>\n");
        for line in &c.demand_charge_lines {
            let _ = writeln!(
                html,
                "<tr><td>{year}-{month:02}</td><td>{period}</td><td class=\"num\">{peak}</td><td class=\"num\">{price} {currency}/kW-month</td><td class=\"num\">{charge}</td></tr>",
                year = line.year,
                month = line.month,
                period = escape(&line.period),
                peak = escape(&format!("{:.2}", line.peak_import_kw)),
                price = escape(&format!("{:.2}", line.price_per_kw_month_usd)),
                charge = money(line.charge_usd, currency)
            );
        }
        html.push_str("</tbody>\n</table></div>\n<p class=\"muted\">The demand charge is the highest mean import power in each local calendar month and tariff period, priced at that period's rate. Peak power is measured over the interval, so a shorter interval will find a higher peak in the same data.</p>\n");
    }
}

fn outage_figure(html: &mut String, scenario: &ScenarioResult) {
    if scenario.outages.is_empty() {
        return;
    }
    html.push_str("<h3>Outages</h3>\n<div class=\"scroll\"><table class=\"num-table\">\n<thead><tr><th>Window</th><th class=\"num\">Duration (h)</th><th class=\"num\">Critical load (kWh)</th><th class=\"num\">Served (kWh)</th><th class=\"num\">Unmet (kWh)</th><th class=\"num\">SOC at start (%)</th><th class=\"num\">Lowest SOC (%)</th><th>Outcome</th></tr></thead>\n<tbody>\n");
    for outage in &scenario.outages {
        let _ = writeln!(
            html,
            r#"<tr><td>{label}{start} to {end}</td><td class="num">{duration}</td><td class="num">{critical}</td><td class="num">{served}</td><td class="num {unmet_class}">{unmet}</td><td class="num">{soc_start}</td><td class="num">{soc_min}</td><td>{outcome}</td></tr>"#,
            label = if outage.label.is_empty() {
                String::new()
            } else {
                format!("<strong>{}</strong><br>", escape(&outage.label))
            },
            start = escape(&outage.start_utc.format("%Y-%m-%d %H:%M").to_string()),
            end = escape(&outage.end_utc.format("%Y-%m-%d %H:%M").to_string()),
            duration = escape(&format!("{:.1}", outage.duration_hours)),
            critical = escape(&format_number(outage.critical_load_kwh)),
            served = escape(&format_number(outage.critical_served_kwh)),
            unmet_class = if outage.survived { "" } else { "bad" },
            unmet = escape(&format_number(outage.unmet_critical_kwh)),
            soc_start = escape(&format!("{:.1}", outage.soc_at_start_pct)),
            soc_min = escape(&format!("{:.1}", outage.min_soc_pct)),
            outcome = if outage.survived {
                "<span class=\"pill good\">+ survived the full window</span>".to_string()
            } else {
                format!(
                    "<span class=\"pill critical\">x critical load lost after {:.1} h</span>",
                    outage.survival_hours
                )
            }
        );
    }
    html.push_str("</tbody>\n</table></div>\n<p class=\"muted\">During an outage Tonans assumes non-critical load is shed by a transfer switch and that the battery may discharge below its reserve floor down to its hard minimum. \"Survived\" means every kWh of critical load was served for the whole window; it is a lower bound on survivability, because the window may simply have ended before the battery ran out.</p>\n");
}

fn economics_table(html: &mut String, scenario: &ScenarioResult, currency: &str) {
    let economics = &scenario.economics;
    if economics.capex_usd <= 0.0 && scenario.scenario.battery.is_none() {
        return;
    }

    html.push_str("<h3>Economics</h3>\n");
    let _ = write!(
        html,
        r#"<div class="tiles">
<div class="tile"><div class="tile-label">Net present value</div><div class="tile-value {npv_class}">{npv}</div><div class="tile-note">over {horizon} years at {discount}% discount</div></div>
<div class="tile"><div class="tile-label">Simple payback</div><div class="tile-value">{payback}</div><div class="tile-note">undiscounted</div></div>
<div class="tile"><div class="tile-label">Year-one saving</div><div class="tile-value">{saving}</div><div class="tile-note">against the baseline</div></div>
<div class="tile"><div class="tile-label">Throughput</div><div class="tile-value">{cycles}</div><div class="tile-note">equivalent full cycles per year</div></div>
</div>
"#,
        npv_class = if economics.npv_usd >= 0.0 {
            "good"
        } else {
            "bad"
        },
        npv = money(economics.npv_usd, currency),
        horizon = economics.horizon_years,
        discount = format_number(economics.discount_rate_pct_per_year),
        payback = economics
            .simple_payback_years
            .map_or_else(|| "never".to_string(), |y| format!("{y:.1} years")),
        saving = money(economics.year_one_gross_savings_usd, currency),
        cycles = escape(&format!("{:.1}", economics.equivalent_full_cycles_per_year)),
    );

    if (economics.annualization_multiplier - 1.0).abs() > 0.01 {
        let _ = writeln!(
            html,
            "<p class=\"banner caution\">Annual figures are the simulated period multiplied by {factor:.3}, because the data set is not a full year. Demand charges and seasonal load do not scale with elapsed time.</p>",
            factor = economics.annualization_multiplier
        );
    }

    html.push_str("<div class=\"scroll\"><table class=\"num-table\">\n<thead><tr><th class=\"num\">Year</th><th class=\"num\">Usable capacity (%)</th><th class=\"num\">Gross saving</th><th class=\"num\">Operating cost</th><th class=\"num\">Throughput cost</th><th class=\"num\">Replacement</th><th class=\"num\">Net cash flow</th><th class=\"num\">Discounted</th><th class=\"num\">Cumulative discounted</th></tr></thead>\n<tbody>\n");
    let _ = writeln!(
        html,
        "<tr><td class=\"num\">0</td><td class=\"num\">—</td><td class=\"num\">—</td><td class=\"num\">—</td><td class=\"num\">—</td><td class=\"num\">{capex}</td><td class=\"num\">{negcapex}</td><td class=\"num\">{negcapex}</td><td class=\"num\">{negcapex}</td></tr>",
        capex = money(economics.capex_usd, currency),
        negcapex = money(-economics.capex_usd, currency)
    );
    for year in &economics.cash_flow {
        let _ = writeln!(
            html,
            "<tr><td class=\"num\">{year}</td><td class=\"num\">{capacity}</td><td class=\"num\">{gross}</td><td class=\"num\">{opex}</td><td class=\"num\">{throughput}</td><td class=\"num\">{replacement}</td><td class=\"num\">{net}</td><td class=\"num\">{discounted}</td><td class=\"num\">{cumulative}</td></tr>",
            year = year.year,
            capacity = escape(&format!("{:.1}", year.capacity_retention_pct)),
            gross = money(year.gross_savings_usd, currency),
            opex = money(-year.opex_usd, currency),
            throughput = money(-year.throughput_cost_usd, currency),
            replacement = if year.replacement_usd > 0.0 {
                money(-year.replacement_usd, currency)
            } else {
                "—".to_string()
            },
            net = money(year.net_cash_flow_usd, currency),
            discounted = money(year.discounted_cash_flow_usd, currency),
            cumulative = money(year.cumulative_discounted_usd - economics.capex_usd, currency)
        );
    }
    html.push_str("</tbody>\n</table></div>\n");
    let _ = writeln!(
        html,
        "<p class=\"muted\">{method} Energy savings escalate at the stated rate; operating and throughput costs are held constant in nominal terms. There is no tax, incentive, depreciation, or residual value in this model — Tonans would have to guess at all four, and a guess in a financial model is worse than an absence.</p>",
        method = escape(&economics.degradation_method)
    );
}

fn sensitivity_section(html: &mut String, study: &SensitivityResult, currency: &str) {
    let _ = write!(
        html,
        "<section class=\"scenario\">\n<h2>Sensitivity — {name}</h2>\n",
        name = escape(&study.scenario_name)
    );

    // A study's own warnings, such as whether installed cost moved with battery size,
    // change how its numbers read, so they sit above the matrices.
    if !study.warnings.is_empty() {
        html.push_str("<ul class=\"ledger\">\n");
        ledger_items(html, &study.warnings);
        html.push_str("</ul>\n");
    }

    let rows: Vec<String> = study.axes[0]
        .values
        .iter()
        .map(|v| format_number(*v))
        .collect();
    let (column_label, columns): (String, Vec<String>) = match study.axes.get(1) {
        Some(axis) => (
            axis.param.label().to_string(),
            axis.values.iter().map(|v| format_number(*v)).collect(),
        ),
        None => ("—".to_string(), vec!["value".to_string()]),
    };

    for (title, unit, scale, extract) in [
        (
            "Net present value",
            currency.to_string(),
            Scale::Diverging,
            (|m: &crate::sensitivity::SensitivityMetrics| m.npv_usd) as fn(&_) -> f64,
        ),
        (
            "Peak grid import",
            "kW".to_string(),
            Scale::Sequential,
            (|m: &crate::sensitivity::SensitivityMetrics| m.peak_import_kw) as fn(&_) -> f64,
        ),
        (
            "Unmet critical load",
            "kWh".to_string(),
            Scale::Sequential,
            (|m: &crate::sensitivity::SensitivityMetrics| m.unmet_critical_kwh) as fn(&_) -> f64,
        ),
    ] {
        let cells: Vec<f64> = study.cells.iter().map(|c| extract(&c.metrics)).collect();
        // Net present value has a sign, and the sign is the whole question, so it gets
        // a diverging ramp with a neutral zero. Peak import and unmet load are pure
        // magnitudes and get a single-hue ramp. A rainbow would invent boundaries.
        let _ = write!(
            html,
            "<h3>{title}</h3>\n<figure>{svg}</figure>\n",
            title = escape(title),
            svg = svg::heatmap(&svg::Heatmap {
                row_label: study.axes[0].param.label(),
                row_values: &rows,
                column_label: &column_label,
                column_values: &columns,
                cells: &cells,
                unit: &unit,
                scale,
                width: CHART_WIDTH,
            })
        );
    }

    sensitivity_table(html, study, currency);
    html.push_str("</section>\n");
}

fn sensitivity_table(html: &mut String, study: &SensitivityResult, currency: &str) {
    html.push_str(
        "<h3>Every cell</h3>\n<div class=\"scroll\"><table class=\"num-table\">\n<thead><tr>",
    );
    for axis in &study.axes {
        let _ = write!(
            html,
            "<th class=\"num\">{}</th>",
            escape(axis.param.label())
        );
    }
    html.push_str("<th class=\"num\">Installed cost</th><th class=\"num\">Annual grid cost</th><th class=\"num\">Annual saving</th><th class=\"num\">NPV</th><th class=\"num\">Payback (years)</th><th class=\"num\">Peak import (kW)</th><th class=\"num\">Unmet critical (kWh)</th><th class=\"num\">Self-sufficiency (%)</th><th class=\"num\">Lowest SOC (%)</th></tr></thead>\n<tbody>\n");

    for cell in &study.cells {
        html.push_str("<tr>");
        for value in &cell.axis_values {
            let _ = write!(
                html,
                "<td class=\"num\">{}</td>",
                escape(&format_number(*value))
            );
        }
        let m = &cell.metrics;
        let _ = writeln!(
            html,
            "<td class=\"num\">{installed}</td><td class=\"num\">{cost}</td><td class=\"num\">{saving}</td><td class=\"num {npv_class}\">{npv}</td><td class=\"num\">{payback}</td><td class=\"num\">{peak}</td><td class=\"num {unmet_class}\">{unmet}</td><td class=\"num\">{sufficiency}</td><td class=\"num\">{soc}</td></tr>",
            installed = money(m.installed_cost_usd, currency),
            cost = money(m.annual_grid_cost_usd, currency),
            saving = money(m.annual_savings_usd, currency),
            npv_class = if m.npv_usd >= 0.0 { "good" } else { "bad" },
            npv = money(m.npv_usd, currency),
            payback = m
                .simple_payback_years
                .map_or_else(|| "never".to_string(), |y| format!("{y:.1}")),
            peak = escape(&format_number(m.peak_import_kw)),
            unmet_class = if m.unmet_critical_kwh > 1e-9 { "bad" } else { "" },
            unmet = escape(&format_number(m.unmet_critical_kwh)),
            sufficiency = escape(&format!("{:.1}", m.self_sufficiency_pct)),
            soc = escape(&format!("{:.1}", m.min_soc_pct)),
        );
    }
    html.push_str("</tbody>\n</table></div>\n");
}

fn provenance_section(html: &mut String, input: &ReportInput<'_>) {
    let provenance = input.provenance;
    html.push_str("<section id=\"provenance\">\n<h2>Provenance</h2>\n<p class=\"muted\">Everything needed to reproduce this report, or to prove it came from the data it claims to.</p>\n<div class=\"scroll\"><table class=\"kv\">\n<tbody>\n");

    let mut rows: Vec<(String, String)> = vec![
        (
            "Project hash".into(),
            provenance.project.project_hash.to_string(),
        ),
        (
            "Model input hash".into(),
            provenance.project.model_input_hash.to_string(),
        ),
        ("Results hash".into(), provenance.results_hash.to_string()),
        ("Site timezone".into(), provenance.project.timezone.clone()),
        ("Currency".into(), provenance.project.currency.clone()),
        ("Platform".into(), provenance.tool.platform.clone()),
    ];

    for file in &provenance.inputs {
        rows.push(("Source file".into(), file.filename.clone()));
        rows.push(("  digest".into(), file.sha256.to_string()));
        rows.push((
            "  size".into(),
            format!("{} bytes, {} data rows", file.size_bytes, file.row_count),
        ));
        rows.push((
            "  interval".into(),
            format!(
                "{} minutes, {} intervals, {} to {}",
                file.interval_minutes,
                file.interval_count,
                file.start_utc.format("%Y-%m-%d %H:%M"),
                file.end_utc.format("%Y-%m-%d %H:%M")
            ),
        ));
        rows.push(("  gap policy".into(), file.gap_policy.clone()));
        if file.synthesized_interval_count > 0 {
            rows.push((
                "  synthesized intervals".into(),
                format!(
                    "{} of {} were NOT measured — they were created by the gap policy",
                    file.synthesized_interval_count, file.interval_count
                ),
            ));
        }
        for (role, column) in &file.column_mapping {
            rows.push((format!("  column: {role}"), column.clone()));
        }
        if !file.ignored_columns.is_empty() {
            rows.push(("  ignored columns".into(), file.ignored_columns.join(", ")));
        }
    }
    for scenario in &provenance.scenarios {
        rows.push((
            format!("Assumptions: {}", scenario.name),
            scenario.assumptions_hash.to_string(),
        ));
    }

    for (key, value) in rows {
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\">{key}</th><td class=\"mono\">{value}</td></tr>",
            key = escape(&key),
            value = escape(&value)
        );
    }
    html.push_str("</tbody>\n</table></div>\n");

    if let Some(dataset) = &input.project.dataset {
        html.push_str("<h3>Series imported</h3>\n<ul>\n");
        for kind in SeriesKind::ALL {
            let present = dataset.series.contains_key(kind);
            let _ = writeln!(
                html,
                "<li>{name}: {state}</li>",
                name = escape(kind.as_str()),
                state = if present {
                    format!("imported, stored in {}", kind.unit())
                } else if kind.is_required() {
                    "missing".to_string()
                } else {
                    "not imported — taken as zero".to_string()
                }
            );
        }
        html.push_str("</ul>\n");
    }

    html.push_str("<h3>Units</h3>\n<div class=\"scroll\"><table class=\"kv\">\n<tbody>\n");
    for (suffix, unit) in &provenance.units {
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\"><code>{suffix}</code></th><td>{unit}</td></tr>",
            suffix = escape(suffix),
            unit = escape(unit)
        );
    }
    html.push_str("</tbody>\n</table></div>\n</section>\n");
}

fn model_limits(html: &mut String, run: &RunResult) {
    html.push_str("<section id=\"limits\">\n<h2>What this model does not do</h2>\n<ul class=\"exclusions\">\n");
    for exclusion in &run.model_exclusions {
        let _ = writeln!(html, "<li>{}</li>", escape(exclusion));
    }
    html.push_str("</ul>\n");

    let _ = write!(
        html,
        "<h3>Enforced limits</h3>\n<p class=\"muted\">This run happened inside these bounds: at most {rows} data rows, {scenarios} scenarios, {cells} sensitivity cells, {outages} outage windows, a {horizon}-year horizon, and at most {synth:.0}% of intervals synthesized by a gap policy.</p>\n</section>\n",
        rows = run.limits.max_csv_rows,
        scenarios = run.limits.max_scenarios,
        cells = run.limits.max_sensitivity_cells,
        outages = run.limits.max_outage_windows,
        horizon = run.limits.max_horizon_years,
        synth = run.limits.max_synthesized_fraction * 100.0
    );
}

fn footer(html: &mut String, run: &RunResult) {
    let _ = write!(
        html,
        r#"<footer>
<p>{product} {version}, model {model}. {vendor} — {support}.</p>
<p class="muted">{disclaimer}</p>
</footer>
"#,
        product = PRODUCT_NAME,
        version = escape(&run.app_version),
        model = escape(&run.model_version),
        vendor = VENDOR_NAME,
        support = escape(SUPPORT_EMAIL),
        disclaimer = escape(PLANNING_DISCLAIMER)
    );
}

// ---------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------

fn money(value: f64, currency: &str) -> String {
    let rounded = round_money(value);
    let magnitude = rounded.abs();
    let body = if magnitude >= 1_000_000.0 {
        format!("{:.2}M", rounded / 1_000_000.0)
    } else if magnitude >= 10_000.0 {
        format!("{:.1}k", rounded / 1_000.0)
    } else {
        format!("{rounded:.2}")
    };
    escape(&format!("{body} {currency}"))
}

fn stylesheet() -> String {
    format!(
        "{palette}{rules}",
        palette = svg::palette_css(),
        rules = r#"
*{box-sizing:border-box}
body{margin:0;background:var(--page);color:var(--ink);font:15px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif}
.page{max-width:1000px;margin:0 auto;padding:32px 24px 80px}
h1{font-size:30px;margin:.2em 0 .4em;letter-spacing:-.015em}
h2{font-size:21px;margin:2.2em 0 .6em;padding-bottom:.35em;border-bottom:1px solid var(--hairline)}
h3{font-size:15px;margin:1.8em 0 .5em;text-transform:uppercase;letter-spacing:.06em;color:var(--ink-secondary)}
h4{font-size:14px;margin:1.4em 0 .4em;color:var(--ink-secondary)}
p{margin:.6em 0}
.muted{color:var(--ink-secondary);font-size:13.5px}
.mono,code{font-family:ui-monospace,"Cascadia Mono",Consolas,monospace;font-size:12.5px;word-break:break-all}
.brand{font-weight:600;letter-spacing:.02em}
.head{border-bottom:2px solid var(--ink);padding-bottom:18px}
.stamp{display:flex;flex-wrap:wrap;gap:22px;margin:.6em 0 0}
.stamp div{margin:0}
.stamp dt{font-size:11px;text-transform:uppercase;letter-spacing:.07em;color:var(--ink-muted)}
.stamp dd{margin:2px 0 0;font-weight:600;font-size:14px}
.banner{margin:20px 0;padding:14px 16px;border-radius:8px;background:var(--surface);border:1px solid var(--hairline);border-left:4px solid var(--warning);font-size:14px}
.banner.caution{border-left-color:var(--c-import)}
section{margin-top:8px}
.scenario{margin-top:34px}
figure{margin:12px 0 6px;padding:14px;background:var(--surface);border:1px solid var(--hairline);border-radius:10px}
figcaption{margin-top:10px;font-size:12.5px;color:var(--ink-secondary)}
.chart{display:block;overflow:visible}
.scroll{overflow-x:auto;margin:10px 0}
table{border-collapse:collapse;width:100%;font-size:13.5px;background:var(--surface);border:1px solid var(--hairline);border-radius:8px}
th,td{padding:7px 11px;text-align:left;border-bottom:1px solid var(--hairline);vertical-align:top}
thead th{font-size:11px;text-transform:uppercase;letter-spacing:.05em;color:var(--ink-muted);font-weight:600;white-space:nowrap}
tbody tr:last-child td,tbody tr:last-child th{border-bottom:none}
.num{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.unit{color:var(--ink-muted);font-size:12px}
.kv th{width:34%;font-weight:600;color:var(--ink-secondary)}
tr.total th,tr.total td{font-weight:700;border-top:2px solid var(--axis)}
tr.check td{color:var(--ink-muted);font-size:12px}
tr.baseline{background:color-mix(in srgb,var(--c-load) 7%,transparent)}
.good{color:var(--good)}
.bad{color:var(--critical)}
.swatch{display:inline-block;width:10px;height:10px;border-radius:3px;margin-right:7px;vertical-align:baseline}
.tag{display:inline-block;margin-left:6px;padding:1px 6px;border-radius:4px;background:var(--gridline);color:var(--ink-secondary);font-size:10.5px;text-transform:uppercase;letter-spacing:.05em}
.pill{display:inline-block;padding:1px 7px;border-radius:4px;font-size:11px;font-weight:700;text-transform:uppercase;letter-spacing:.04em;border:1px solid var(--hairline)}
.pill.good{color:var(--good)}
.pill.critical{color:var(--critical)}
.ledger{list-style:none;padding:0;margin:10px 0}
.ledger li{padding:9px 12px;margin-bottom:6px;background:var(--surface);border:1px solid var(--hairline);border-left:4px solid var(--ink-muted);border-radius:6px;font-size:13.5px}
.ledger li.sev-critical{border-left-color:var(--critical)}
.ledger li.sev-caution{border-left-color:var(--warning)}
.ledger li.sev-info{border-left-color:var(--c-load)}
.tiles{display:flex;flex-wrap:wrap;gap:12px;margin:12px 0}
.tile{flex:1 1 170px;padding:14px 16px;background:var(--surface);border:1px solid var(--hairline);border-radius:10px}
.tile-label{font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:var(--ink-muted)}
.tile-value{font-size:24px;font-weight:650;margin:5px 0 3px;letter-spacing:-.01em}
.tile-note{font-size:12px;color:var(--ink-secondary)}
.exclusions{margin:8px 0;padding-left:20px}
.exclusions li{margin:5px 0;font-size:14px}
footer{margin-top:52px;padding-top:18px;border-top:1px solid var(--hairline);font-size:12.5px}
@media print{body{background:#fff}figure,table,.tile,.ledger li{break-inside:avoid}}
"#
    )
}
