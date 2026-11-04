//! Economic accounting: what the system costs, what it saves, and what that is
//! worth once discounted.
//!
//! # Every step is arithmetic you can check
//!
//! There is no solver here and no curve fit. The whole model is a year of
//! simulated grid cost, a difference against a baseline, a fade approximation,
//! and a discounted cash flow. The report prints the year-by-year table so a reader
//! can follow it, and `summary.json` holds the exact figures to add up.
//!
//! # The three approximations, stated plainly
//!
//! **Annualization.** Tonans simulates the data set it was given. If that is not
//! a full year, the period's cost is scaled by `8760 / duration_hours` to reach an
//! annual figure. The factor is *always* reported, and a factor other than 1.0
//! raises a caution — because a demand charge does not scale with time, and three
//! months of summer data annualized by four is not a year.
//!
//! **Fade.** Capacity is lost at a fixed percentage a year, compounding: usable capacity in
//! year `n` is `initial x (1 - r)^(n - 1)`, where `r` adds a calendar term per year and a
//! cycling term per equivalent full cycle. It is not an electrochemical model.
//! It knows nothing about depth of discharge, C-rate, or thermal history. A
//! vendor's warranted capacity curve beats it every time, and where one exists it
//! should be used instead.
//!
//! **Benefit carried forward.** Tonans simulates *one* representative year. Later
//! years reuse that year's benefit, scaled by remaining usable capacity if
//! [`BenefitScaling::ProportionalToUsableCapacity`] is set. Re-simulating every
//! year against a progressively faded battery would be more faithful; it is a
//! documented limitation, not an oversight. See `docs/MODEL.md`.
//!
//! # What escalates and what does not
//!
//! Energy savings escalate at the stated rate. Fixed operating cost and the
//! throughput wear cost are held constant in nominal terms. Capital is spent at
//! year zero. There is no tax, no incentive, no depreciation, and no residual
//! value — Tonans would have to guess at all four, and a guess in a financial
//! model is worse than an absence.

use serde::{Deserialize, Serialize};

use crate::engine::{CostSummary, EnergySummary};
use crate::error::{finite, Result, Warning, WarningLedger};
use crate::schema::{BenefitScaling, Scenario};

/// Hours in the year Tonans annualizes to. A leap year is 8,784; using a fixed
/// 8,760 keeps the factor a property of the tariff year, not of the calendar.
pub const HOURS_PER_YEAR: f64 = 8_760.0;

/// One year of the cash flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CashFlowYear {
    /// Year number, starting at 1. Capital is spent in year 0.
    pub year: u32,
    /// Usable capacity at the start of this year, against nameplate.
    pub capacity_retention_pct: f64,
    /// Grid saving against the baseline, escalated and scaled for fade.
    pub gross_savings_usd: f64,
    /// Fixed operating cost.
    pub opex_usd: f64,
    /// Battery wear, booked against throughput.
    pub throughput_cost_usd: f64,
    /// Replacement capital, in the year the trigger is crossed.
    pub replacement_usd: f64,
    /// The four above, combined.
    pub net_cash_flow_usd: f64,
    /// `net_cash_flow_usd`, discounted to year zero.
    pub discounted_cash_flow_usd: f64,
    /// Running total of the discounted flows.
    pub cumulative_discounted_usd: f64,
}

/// What the system is worth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EconomicsSummary {
    /// Capital spent at year zero.
    pub capex_usd: f64,
    /// Years the cash flow runs for.
    pub horizon_years: u32,
    /// Discount rate applied.
    pub discount_rate_pct_per_year: f64,
    /// Escalation applied to energy savings.
    pub energy_price_escalation_pct_per_year: f64,

    /// What the simulated period was scaled by to reach an annual figure.
    ///
    /// Exactly 1.0 when the data set is a full year. Anything else is reported and
    /// warned about, because a demand charge does not scale with elapsed time.
    pub annualization_multiplier: f64,

    /// Grid cost of the baseline scenario over one year.
    pub baseline_annual_grid_cost_usd: f64,
    /// Grid cost of this scenario over one year.
    pub scenario_annual_grid_cost_usd: f64,
    /// The difference, in year one, before escalation and fade.
    pub year_one_gross_savings_usd: f64,
    /// Year-one import-energy cost less export credit savings; this portion escalates.
    pub year_one_energy_savings_usd: f64,
    /// Year-one demand and fixed-charge savings; this portion does not escalate.
    pub year_one_non_energy_savings_usd: f64,

    /// Battery energy delivered to the bus over one year.
    pub annual_throughput_kwh: f64,
    /// That throughput, in full usable cycles.
    pub equivalent_full_cycles_per_year: f64,
    /// Capacity lost per year: the calendar term plus the cycling term.
    pub annual_fade_pct_per_year: f64,
    /// Usable capacity at the end of the horizon.
    pub final_capacity_retention_pct: f64,
    /// The years a replacement is triggered in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replacement_years: Vec<u32>,

    /// Net present value: `-capex + Σ discounted net cash flow`.
    pub npv_usd: f64,
    /// Years until undiscounted cumulative net cash flow repays the capital.
    /// Absent when it never does within the horizon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub simple_payback_years: Option<f64>,
    /// The same, against discounted cash flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discounted_payback_years: Option<f64>,

    /// The year-by-year table the numbers above are summed from.
    pub cash_flow: Vec<CashFlowYear>,

    /// How the fade model is to be described. Carried in the struct so that no
    /// report can render a fade figure without it.
    pub degradation_method: String,
}

/// Compute the economics of `scenario` against `baseline`.
///
/// `baseline_cost` is the grid cost of the project's baseline scenario over the
/// same data set. When `scenario` *is* the baseline, savings are zero by
/// construction and the NPV is simply the negative of any capital it declares.
pub fn evaluate(
    scenario: &Scenario,
    energy: &EnergySummary,
    cost: &CostSummary,
    baseline_cost: &CostSummary,
    ledger: &mut WarningLedger,
) -> Result<EconomicsSummary> {
    let spec = &scenario.economics;
    let degradation = &scenario.degradation;

    // ---- Annualization ---------------------------------------------------

    let annualization_multiplier = if energy.duration_hours > 0.0 {
        finite(
            "annualization_multiplier",
            HOURS_PER_YEAR / energy.duration_hours,
        )?
    } else {
        1.0
    };
    if (annualization_multiplier - 1.0).abs() > 0.01 {
        ledger.push(Warning::caution(
            "economics.annualized_from_partial_year",
            format!(
                "the data set covers {hours:.0} hours, so every annual figure below is the \
                 simulated period multiplied by {factor:.3}. Demand charges and seasonal load do \
                 not scale with elapsed time, so treat these figures as indicative only. Import a \
                 full year for economics you can defend.",
                hours = energy.duration_hours,
                factor = annualization_multiplier
            ),
        ));
    }

    // Charged per calendar month touched, however little of the month the data covers, and
    // not scaled to a twelfth of anything. The numbers stand; this says what is in them.
    if cost.billed_month_count != 12 {
        let months = cost.billed_month_count;
        ledger.push(Warning::caution(
            "cost.billed_months_not_twelve",
            format!(
                "the data touches {months} calendar month{plural} at the site's time zone, where a \
                 whole year touches 12. {months} month{plural} {were} billed, each with a full \
                 demand charge and a full fixed charge however little of the month the data \
                 covers, and the grid cost, savings, and net present value include them. Data \
                 that starts on the first of a month, in site time, does not do this.",
                plural = if months == 1 { "" } else { "s" },
                were = if months == 1 { "was" } else { "were" },
            ),
        ));
    }

    let baseline_annual_grid_cost_usd =
        baseline_cost.total_grid_cost_usd * annualization_multiplier;
    let scenario_annual_grid_cost_usd = cost.total_grid_cost_usd * annualization_multiplier;
    let baseline_annual_energy_cost_usd = (baseline_cost.import_energy_cost_usd
        - baseline_cost.export_credit_usd)
        * annualization_multiplier;
    let scenario_annual_energy_cost_usd =
        (cost.import_energy_cost_usd - cost.export_credit_usd) * annualization_multiplier;
    let year_one_energy_savings_usd =
        baseline_annual_energy_cost_usd - scenario_annual_energy_cost_usd;
    let year_one_non_energy_savings_usd = ((baseline_cost.demand_charge_usd
        + baseline_cost.fixed_charge_usd)
        - (cost.demand_charge_usd + cost.fixed_charge_usd))
        * annualization_multiplier;
    let year_one_gross_savings_usd = year_one_energy_savings_usd + year_one_non_energy_savings_usd;

    let annual_throughput_kwh = energy.battery_discharge_kwh * annualization_multiplier;
    let equivalent_full_cycles_per_year = energy.equivalent_full_cycles * annualization_multiplier;

    // ---- Fade ------------------------------------------------------------

    let annual_fade_pct_per_year = degradation.calendar_fade_pct_per_year
        + degradation.cycle_fade_pct_per_cycle * equivalent_full_cycles_per_year;
    let fade_factor = (1.0 - annual_fade_pct_per_year / 100.0).clamp(0.0, 1.0);

    if annual_fade_pct_per_year > 20.0 {
        ledger.push(Warning::caution(
            "economics.implausible_fade",
            format!(
                "the fade assumptions imply {annual_fade_pct_per_year:.1}% capacity lost per year, \
                 which would retire the battery in a handful of years. Check the cycle-fade figure \
                 against the vendor's warranty curve."
            ),
        ));
    }

    // ---- Cash flow -------------------------------------------------------

    let escalation = 1.0 + spec.energy_price_escalation_pct_per_year / 100.0;
    let discount = 1.0 + spec.discount_rate_pct_per_year / 100.0;

    let reference_capacity_pct = scenario.initial_capacity_pct;
    let mut capacity_retention_pct = reference_capacity_pct;
    // Iterative multiplication rather than `powf`. `powf` is a libm call, and libm
    // is not required to be bit-identical across platforms; `*=` is IEEE-754 and
    // is. Determinism is worth two extra lines.
    let mut escalation_factor = 1.0f64;
    let mut discount_factor = 1.0f64;

    let mut cash_flow = Vec::with_capacity(spec.horizon_years as usize);
    let mut replacement_years = Vec::new();
    let mut cumulative_discounted_usd = 0.0;
    let mut cumulative_undiscounted_usd = 0.0;
    let mut npv_usd = -spec.capex_usd;

    let mut simple_payback_years = None;
    let mut discounted_payback_years = None;

    for year in 1..=spec.horizon_years {
        discount_factor /= discount;

        // A replacement is triggered at the *start* of the year in which usable
        // capacity would otherwise begin below the trigger.
        let mut replacement_usd = 0.0;
        if let Some(replacement) = &spec.replacement {
            if scenario.battery.is_some()
                && capacity_retention_pct < replacement.trigger_capacity_pct
            {
                replacement_usd = replacement.cost_usd;
                capacity_retention_pct = 100.0;
                replacement_years.push(year);
            }
        }

        let benefit_scale = match degradation.benefit_scaling {
            BenefitScaling::ProportionalToUsableCapacity if reference_capacity_pct > 0.0 => {
                capacity_retention_pct / reference_capacity_pct
            }
            _ => 1.0,
        };

        let gross_savings_usd = (year_one_energy_savings_usd * escalation_factor
            + year_one_non_energy_savings_usd)
            * benefit_scale;
        let throughput_cost_usd = annual_throughput_kwh
            * degradation.throughput_cost_per_kwh_throughput_usd
            * benefit_scale;
        let opex_usd = spec.annual_opex_usd;

        let net_cash_flow_usd =
            gross_savings_usd - opex_usd - throughput_cost_usd - replacement_usd;
        let discounted_cash_flow_usd = net_cash_flow_usd * discount_factor;

        let previous_undiscounted = cumulative_undiscounted_usd;
        let previous_discounted = cumulative_discounted_usd;
        cumulative_undiscounted_usd += net_cash_flow_usd;
        cumulative_discounted_usd += discounted_cash_flow_usd;
        npv_usd += discounted_cash_flow_usd;

        // Payback is interpolated inside the year that crosses the capital, which
        // is the convention a reader expects from "3.4 years".
        if simple_payback_years.is_none()
            && previous_undiscounted < spec.capex_usd
            && cumulative_undiscounted_usd >= spec.capex_usd
            && net_cash_flow_usd > 0.0
        {
            let shortfall = spec.capex_usd - previous_undiscounted;
            simple_payback_years = Some(f64::from(year - 1) + shortfall / net_cash_flow_usd);
        }
        if discounted_payback_years.is_none()
            && previous_discounted < spec.capex_usd
            && cumulative_discounted_usd >= spec.capex_usd
            && discounted_cash_flow_usd > 0.0
        {
            let shortfall = spec.capex_usd - previous_discounted;
            discounted_payback_years =
                Some(f64::from(year - 1) + shortfall / discounted_cash_flow_usd);
        }

        cash_flow.push(CashFlowYear {
            year,
            capacity_retention_pct,
            gross_savings_usd,
            opex_usd,
            throughput_cost_usd,
            replacement_usd,
            net_cash_flow_usd,
            discounted_cash_flow_usd,
            cumulative_discounted_usd,
        });

        // Age the battery into the next year.
        if scenario.battery.is_some() {
            capacity_retention_pct *= fade_factor;
        }
        escalation_factor *= escalation;
    }

    // ---- Advisories ------------------------------------------------------

    if spec.capex_usd > 0.0 && simple_payback_years.is_none() {
        ledger.push(Warning::caution(
            "economics.no_payback_within_horizon",
            format!(
                "the capital is not repaid within the {} year horizon at these assumptions.",
                spec.horizon_years
            ),
        ));
    }
    if scenario.battery.is_some() && spec.capex_usd <= 0.0 {
        ledger.push(Warning::caution(
            "economics.no_capex",
            "this scenario has a battery but no capital cost, so its NPV and payback are \
             meaningless. Enter the installed cost.",
        ));
    }
    if spec.replacement.is_none() && annual_fade_pct_per_year > 0.0 && scenario.battery.is_some() {
        ledger.push(Warning::info(
            "economics.no_replacement_modelled",
            "the battery fades but no replacement is modelled, so the later years of this cash \
             flow assume a battery that is never renewed.",
        ));
    }

    let degradation_method = format!(
        "Compounding fade approximation: {calendar:.2}% per year (calendar) + {cycle:.4}% per \
         equivalent full cycle x {cycles:.1} cycles per year = {total:.2}% per year, each year's \
         loss taken from the capacity left after the year before. Benefit in later years is \
         {scaling}. This is a transparent approximation, not an electrochemical lifetime model.",
        calendar = degradation.calendar_fade_pct_per_year,
        cycle = degradation.cycle_fade_pct_per_cycle,
        cycles = equivalent_full_cycles_per_year,
        total = annual_fade_pct_per_year,
        scaling = match degradation.benefit_scaling {
            BenefitScaling::ProportionalToUsableCapacity => "scaled by remaining usable capacity",
            BenefitScaling::Constant =>
                "held constant, i.e. fade is assumed to be handled outside the model",
        }
    );

    Ok(EconomicsSummary {
        capex_usd: spec.capex_usd,
        horizon_years: spec.horizon_years,
        discount_rate_pct_per_year: spec.discount_rate_pct_per_year,
        energy_price_escalation_pct_per_year: spec.energy_price_escalation_pct_per_year,
        annualization_multiplier,
        baseline_annual_grid_cost_usd,
        scenario_annual_grid_cost_usd,
        year_one_gross_savings_usd,
        year_one_energy_savings_usd,
        year_one_non_energy_savings_usd,
        annual_throughput_kwh,
        equivalent_full_cycles_per_year,
        annual_fade_pct_per_year,
        final_capacity_retention_pct: capacity_retention_pct,
        replacement_years,
        npv_usd,
        simple_payback_years,
        discounted_payback_years,
        cash_flow,
        degradation_method,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{CostSummary, EnergySummary};
    use crate::schema::{BatterySpec, DegradationSpec, EconomicsSpec, ReplacementSpec};

    fn energy(duration_hours: f64, discharge_kwh: f64) -> EnergySummary {
        EnergySummary {
            interval_count: (duration_hours as usize).max(1),
            duration_hours,
            load_kwh: 0.0,
            served_load_kwh: 0.0,
            critical_load_kwh: 0.0,
            unmet_critical_kwh: 0.0,
            shed_noncritical_kwh: 0.0,
            generation_available_kwh: 0.0,
            generation_used_kwh: 0.0,
            generation_to_load_kwh: 0.0,
            generation_to_battery_kwh: 0.0,
            generation_to_grid_kwh: 0.0,
            curtailed_kwh: 0.0,
            battery_charge_kwh: 0.0,
            battery_discharge_kwh: discharge_kwh,
            conversion_loss_kwh: 0.0,
            self_discharge_loss_kwh: 0.0,
            grid_import_kwh: 0.0,
            grid_to_load_kwh: 0.0,
            grid_to_battery_kwh: 0.0,
            grid_export_kwh: 0.0,
            battery_to_load_kwh: 0.0,
            battery_to_grid_kwh: 0.0,
            generation_origin_battery_to_load_kwh: 0.0,
            grid_origin_battery_to_load_kwh: 0.0,
            initial_origin_battery_to_load_kwh: 0.0,
            generation_origin_battery_to_grid_kwh: 0.0,
            grid_origin_battery_to_grid_kwh: 0.0,
            initial_origin_battery_to_grid_kwh: 0.0,
            peak_import_kw: 0.0,
            peak_export_kw: 0.0,
            initial_stored_energy_kwh: 0.0,
            final_stored_energy_kwh: 0.0,
            min_soc_pct: 0.0,
            max_soc_pct: 0.0,
            self_consumption_pct: 0.0,
            self_sufficiency_pct: 0.0,
            equivalent_full_cycles: 0.0,
            energy_balance_residual_kwh: 0.0,
        }
    }

    fn cost(total: f64) -> CostSummary {
        CostSummary {
            import_energy_cost_usd: total,
            export_credit_usd: 0.0,
            demand_charge_usd: 0.0,
            fixed_charge_usd: 0.0,
            total_grid_cost_usd: total,
            billed_month_count: 12,
            demand_charge_lines: Vec::new(),
        }
    }

    fn scenario(economics: EconomicsSpec, degradation: DegradationSpec) -> Scenario {
        let mut scenario = Scenario::no_storage_baseline();
        scenario.id = "candidate".into();
        scenario.battery = Some(BatterySpec {
            nameplate_energy_kwh: 100.0,
            max_charge_kw: 50.0,
            max_discharge_kw: 50.0,
            soc_min_pct: 10.0,
            soc_max_pct: 100.0,
            reserve_soc_pct: 20.0,
            charge_efficiency_pct: 95.0,
            discharge_efficiency_pct: 95.0,
            self_discharge_pct_per_month: 0.0,
            temperature_derate: None,
        });
        scenario.inverter.battery_power_kw = 50.0;
        scenario.initial_soc_pct = 50.0;
        scenario.economics = economics;
        scenario.degradation = degradation;
        scenario
    }

    /// Hand calculation, no fade, no escalation, no discount.
    ///
    /// Baseline costs 10,000/yr, the scenario costs 8,000/yr, so it saves 2,000/yr.
    /// OPEX is 200/yr, so net is 1,800/yr. CAPEX is 9,000.
    /// Payback = 9,000 / 1,800 = exactly 5 years.
    /// NPV over 10 years at a 0% discount = -9,000 + 10 x 1,800 = +9,000.
    #[test]
    fn a_flat_cash_flow_pays_back_where_arithmetic_says_it_does() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 9_000.0,
                annual_opex_usd: 200.0,
                horizon_years: 10,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 0.0,
                replacement: None,
            },
            DegradationSpec {
                calendar_fade_pct_per_year: 0.0,
                cycle_fade_pct_per_cycle: 0.0,
                throughput_cost_per_kwh_throughput_usd: 0.0,
                benefit_scaling: BenefitScaling::Constant,
            },
        );
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(8_000.0),
            &cost(10_000.0),
            &mut ledger,
        )
        .unwrap();

        assert!((result.annualization_multiplier - 1.0).abs() < 1e-12);
        assert!((result.year_one_gross_savings_usd - 2_000.0).abs() < 1e-9);
        assert!((result.simple_payback_years.unwrap() - 5.0).abs() < 1e-9);
        assert!((result.npv_usd - 9_000.0).abs() < 1e-9);
        assert_eq!(result.cash_flow.len(), 10);
        assert!((result.cash_flow[0].net_cash_flow_usd - 1_800.0).abs() < 1e-9);
    }

    /// Discounting, by hand. 1,000/yr for 3 years at 10%, no capital.
    /// 1000/1.1 = 909.0909...
    /// 1000/1.21 = 826.4462...
    /// 1000/1.331 = 751.3148...
    /// Sum = 2,486.851991...
    #[test]
    fn discounting_matches_the_closed_form() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 0.0,
                annual_opex_usd: 0.0,
                horizon_years: 3,
                discount_rate_pct_per_year: 10.0,
                energy_price_escalation_pct_per_year: 0.0,
                replacement: None,
            },
            DegradationSpec {
                calendar_fade_pct_per_year: 0.0,
                cycle_fade_pct_per_cycle: 0.0,
                throughput_cost_per_kwh_throughput_usd: 0.0,
                benefit_scaling: BenefitScaling::Constant,
            },
        );
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &cost(1_000.0),
            &mut ledger,
        )
        .unwrap();

        let expected = 1_000.0 / 1.1 + 1_000.0 / 1.21 + 1_000.0 / 1.331;
        assert!(
            (result.npv_usd - expected).abs() < 1e-9,
            "{}",
            result.npv_usd
        );
        assert!(
            (expected - 2_486.851991).abs() < 1e-5,
            "sanity check on the hand figure"
        );
    }

    /// Fade reduces later benefit when scaling is proportional.
    /// 2% per year: year 1 = 100%, year 2 = 98%, year 3 = 96.04%.
    #[test]
    fn proportional_scaling_shrinks_benefit_with_capacity() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 0.0,
                annual_opex_usd: 0.0,
                horizon_years: 3,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 0.0,
                replacement: None,
            },
            DegradationSpec {
                calendar_fade_pct_per_year: 2.0,
                cycle_fade_pct_per_cycle: 0.0,
                throughput_cost_per_kwh_throughput_usd: 0.0,
                benefit_scaling: BenefitScaling::ProportionalToUsableCapacity,
            },
        );
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &cost(1_000.0),
            &mut ledger,
        )
        .unwrap();

        assert!((result.cash_flow[0].gross_savings_usd - 1_000.0).abs() < 1e-9);
        assert!((result.cash_flow[1].gross_savings_usd - 980.0).abs() < 1e-9);
        assert!((result.cash_flow[2].gross_savings_usd - 960.4).abs() < 1e-9);
        assert!((result.final_capacity_retention_pct - 96.04 * 0.98).abs() < 1e-9);
        assert!(result
            .degradation_method
            .contains("not an electrochemical lifetime model"));

        // Constant scaling holds the benefit flat, whatever the fade.
        let mut constant = scenario.clone();
        constant.degradation.benefit_scaling = BenefitScaling::Constant;
        let result = evaluate(
            &constant,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &cost(1_000.0),
            &mut WarningLedger::new(),
        )
        .unwrap();
        assert!((result.cash_flow[2].gross_savings_usd - 1_000.0).abs() < 1e-9);
    }

    /// A replacement fires in the first year capacity would start below the trigger.
    /// 10% fade: 100, 90, 81, 72.9, 65.61 — so with a trigger at 70%, year 5 is the
    /// first year that starts below it.
    #[test]
    fn fade_compounds_and_year_fifteen_is_the_stated_formula() {
        let economics = EconomicsSpec {
            capex_usd: 100_000.0,
            replacement: None,
            horizon_years: 15,
            ..EconomicsSpec::default()
        };
        let degradation = DegradationSpec {
            calendar_fade_pct_per_year: 2.0,
            cycle_fade_pct_per_cycle: 0.0,
            ..DegradationSpec::default()
        };
        let mut ledger = WarningLedger::new();
        let result = evaluate(
            &scenario(economics, degradation),
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(90_000.0),
            &cost(100_000.0),
            &mut ledger,
        )
        .unwrap();
        // 100 x 0.98^14. A straight line would read 72.
        let expected = 100.0 * 0.98_f64.powi(14);
        assert!((expected - 75.36).abs() < 0.01);
        let year_15 = result.cash_flow[14].capacity_retention_pct;
        assert!((year_15 - expected).abs() < 1e-9, "{year_15}");
        // And every text that describes it says so.
        assert!(result.degradation_method.contains("Compounding"));
        assert!(!result.degradation_method.to_lowercase().contains("linear"));
    }

    #[test]
    fn replacement_fires_when_capacity_crosses_the_trigger() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 0.0,
                annual_opex_usd: 0.0,
                horizon_years: 6,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 0.0,
                replacement: Some(ReplacementSpec {
                    cost_usd: 5_000.0,
                    trigger_capacity_pct: 70.0,
                }),
            },
            DegradationSpec {
                calendar_fade_pct_per_year: 10.0,
                cycle_fade_pct_per_cycle: 0.0,
                throughput_cost_per_kwh_throughput_usd: 0.0,
                benefit_scaling: BenefitScaling::ProportionalToUsableCapacity,
            },
        );
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &cost(1_000.0),
            &mut ledger,
        )
        .unwrap();

        assert_eq!(result.replacement_years, vec![5]);
        assert!((result.cash_flow[4].replacement_usd - 5_000.0).abs() < 1e-9);
        // Replacement restores capacity to nameplate for that year.
        assert!((result.cash_flow[4].capacity_retention_pct - 100.0).abs() < 1e-9);
        assert!((result.cash_flow[3].capacity_retention_pct - 72.9).abs() < 1e-9);
    }

    #[test]
    fn escalation_compounds_on_the_savings() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 0.0,
                annual_opex_usd: 0.0,
                horizon_years: 3,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 5.0,
                replacement: None,
            },
            DegradationSpec::default(),
        );
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &cost(1_000.0),
            &mut ledger,
        )
        .unwrap();
        assert!((result.cash_flow[0].gross_savings_usd - 1_000.0).abs() < 1e-9);
        assert!((result.cash_flow[1].gross_savings_usd - 1_050.0).abs() < 1e-9);
        assert!((result.cash_flow[2].gross_savings_usd - 1_102.5).abs() < 1e-9);
    }

    #[test]
    fn energy_escalation_does_not_escalate_demand_or_fixed_charge_savings() {
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 0.0,
                annual_opex_usd: 0.0,
                horizon_years: 3,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 10.0,
                replacement: None,
            },
            DegradationSpec::default(),
        );
        let baseline = CostSummary {
            import_energy_cost_usd: 1_000.0,
            export_credit_usd: 0.0,
            demand_charge_usd: 200.0,
            fixed_charge_usd: 100.0,
            total_grid_cost_usd: 1_300.0,
            billed_month_count: 12,
            demand_charge_lines: Vec::new(),
        };
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &baseline,
            &mut WarningLedger::new(),
        )
        .unwrap();

        assert!((result.year_one_energy_savings_usd - 1_000.0).abs() < 1e-9);
        assert!((result.year_one_non_energy_savings_usd - 300.0).abs() < 1e-9);
        assert!((result.cash_flow[0].gross_savings_usd - 1_300.0).abs() < 1e-9);
        assert!((result.cash_flow[1].gross_savings_usd - 1_400.0).abs() < 1e-9);
        assert!((result.cash_flow[2].gross_savings_usd - 1_510.0).abs() < 1e-9);

        let mut export_case = cost(1_000.0);
        export_case.export_credit_usd = 200.0;
        export_case.total_grid_cost_usd = 800.0;
        let export_result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &export_case,
            &cost(1_000.0),
            &mut WarningLedger::new(),
        )
        .unwrap();
        assert!((export_result.year_one_energy_savings_usd - 200.0).abs() < 1e-9);
        assert!((export_result.cash_flow[1].gross_savings_usd - 220.0).abs() < 1e-9);
    }

    #[test]
    fn a_partial_year_is_annualized_loudly_and_never_silently() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(EconomicsSpec::default(), DegradationSpec::default());
        // One month of data: 730 hours. 8760/730 = 12 exactly.
        let result = evaluate(
            &scenario,
            &energy(730.0, 0.0),
            &cost(100.0),
            &cost(200.0),
            &mut ledger,
        )
        .unwrap();

        assert!((result.annualization_multiplier - 12.0).abs() < 1e-9);
        assert!((result.baseline_annual_grid_cost_usd - 2_400.0).abs() < 1e-9);
        assert!((result.scenario_annual_grid_cost_usd - 1_200.0).abs() < 1e-9);

        let codes: Vec<String> = ledger.into_sorted().into_iter().map(|w| w.code).collect();
        assert!(
            codes.contains(&"economics.annualized_from_partial_year".to_string()),
            "{codes:?}"
        );
    }

    #[test]
    fn capital_that_is_never_repaid_says_so() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 1_000_000.0,
                annual_opex_usd: 0.0,
                horizon_years: 5,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 0.0,
                replacement: None,
            },
            DegradationSpec::default(),
        );
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 0.0),
            &cost(0.0),
            &cost(100.0),
            &mut ledger,
        )
        .unwrap();

        assert!(result.simple_payback_years.is_none());
        assert!(result.npv_usd < 0.0);
        let codes: Vec<String> = ledger.into_sorted().into_iter().map(|w| w.code).collect();
        assert!(
            codes.contains(&"economics.no_payback_within_horizon".to_string()),
            "{codes:?}"
        );
    }

    #[test]
    fn throughput_wear_is_booked_against_discharged_energy() {
        let mut ledger = WarningLedger::new();
        let scenario = scenario(
            EconomicsSpec {
                capex_usd: 0.0,
                annual_opex_usd: 0.0,
                horizon_years: 1,
                discount_rate_pct_per_year: 0.0,
                energy_price_escalation_pct_per_year: 0.0,
                replacement: None,
            },
            DegradationSpec {
                calendar_fade_pct_per_year: 0.0,
                cycle_fade_pct_per_cycle: 0.0,
                throughput_cost_per_kwh_throughput_usd: 0.05,
                benefit_scaling: BenefitScaling::Constant,
            },
        );
        // 20,000 kWh discharged over a full year at 5 cents of wear = 1,000.
        let result = evaluate(
            &scenario,
            &energy(HOURS_PER_YEAR, 20_000.0),
            &cost(0.0),
            &cost(3_000.0),
            &mut ledger,
        )
        .unwrap();

        assert!((result.annual_throughput_kwh - 20_000.0).abs() < 1e-9);
        assert!((result.cash_flow[0].throughput_cost_usd - 1_000.0).abs() < 1e-9);
        assert!((result.cash_flow[0].net_cash_flow_usd - 2_000.0).abs() < 1e-9);
    }
}
