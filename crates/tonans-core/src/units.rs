//! Units.
//!
//! Every material quantity in Tonans carries an explicit unit. The mechanism is
//! deliberately low-tech: each numeric field in the schema, the results, the API,
//! and the report is *named* with its unit suffix (`_kwh`, `_kw`, `_pct`, `_usd`,
//! …), and [`unit_for_field`] maps that suffix back to a [`Unit`] for display.
//!
//! The contract is enforced by `tests/units_contract.rs`, which serializes a
//! populated result tree and fails if any numeric leaf carries a name it cannot
//! resolve to a unit. A newly added quantity therefore cannot reach a report
//! without a unit.

use std::fmt;

/// A physical or monetary unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Power, kilowatts. In this model always the mean power over an interval.
    Kilowatt,
    /// Energy, kilowatt-hours.
    KilowattHour,
    /// A percentage, 0-100 unless a field says otherwise.
    Percent,
    /// Temperature, degrees Celsius.
    Celsius,
    /// Money, in the project currency.
    Money,
    /// A price per unit of energy, money per kilowatt-hour.
    MoneyPerKilowattHour,
    /// A demand price, money per kilowatt of monthly peak.
    MoneyPerKilowattMonth,
    /// A recurring charge, money per month. A fixed standing charge is *not* an
    /// amount of money, and rendering it as one turns "45 USD/month" into "45 USD"
    /// on the scenario card — off by a factor of the billing period.
    MoneyPerMonth,
    /// Money per kilowatt-hour of battery throughput.
    MoneyPerKilowattHourThroughput,
    /// Duration, hours.
    Hour,
    /// Duration, years.
    Year,
    /// Duration, minutes.
    Minute,
    /// A dimensionless multiplier, 1.0 meaning unchanged.
    Multiplier,
    /// A dimensionless count of whole things.
    Count,
    /// Battery cycles, where one cycle is one usable-capacity discharge.
    Cycle,
    /// A size in bytes.
    Byte,
}

impl Unit {
    /// The symbol shown next to a value in the UI and in reports.
    ///
    /// [`Unit::Money`] resolves to the project's currency code at render time, so
    /// its symbol here is the neutral placeholder the caller is expected to
    /// replace via [`Unit::symbol_with_currency`].
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Unit::Kilowatt => "kW",
            Unit::KilowattHour => "kWh",
            Unit::Percent => "%",
            Unit::Celsius => "°C",
            Unit::Money => "USD",
            Unit::MoneyPerKilowattHour => "USD/kWh",
            Unit::MoneyPerKilowattMonth => "USD/kW-month",
            Unit::MoneyPerMonth => "USD/month",
            Unit::MoneyPerKilowattHourThroughput => "USD/kWh throughput",
            Unit::Hour => "h",
            Unit::Year => "years",
            Unit::Minute => "min",
            Unit::Multiplier => "x",
            Unit::Count => "",
            Unit::Cycle => "cycles",
            Unit::Byte => "bytes",
        }
    }

    /// The symbol for a unit, with the money placeholder replaced by `currency`.
    #[must_use]
    pub fn symbol_with_currency(self, currency: &str) -> String {
        match self {
            Unit::Money => currency.to_string(),
            Unit::MoneyPerKilowattHour => format!("{currency}/kWh"),
            Unit::MoneyPerKilowattMonth => format!("{currency}/kW-month"),
            Unit::MoneyPerMonth => format!("{currency}/month"),
            Unit::MoneyPerKilowattHourThroughput => format!("{currency}/kWh throughput"),
            other => other.symbol().to_string(),
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.symbol())
    }
}

/// Resolve the unit of a serialized field from its name.
///
/// Returns `None` for a name that carries no unit suffix. Callers that must not
/// let an unlabelled quantity through (the report, the units-contract test) treat
/// `None` as a failure; callers rendering free-form JSON treat it as "not a
/// measured quantity".
///
/// Order matters: the longest suffixes are tested first so that, for example,
/// `import_price_per_kwh_usd` resolves to a price and not to money.
#[must_use]
pub fn unit_for_field(name: &str) -> Option<Unit> {
    const SUFFIXES: &[(&str, Unit)] = &[
        (
            "_per_kwh_throughput_usd",
            Unit::MoneyPerKilowattHourThroughput,
        ),
        ("_per_kw_month_usd", Unit::MoneyPerKilowattMonth),
        ("_per_kwh_usd", Unit::MoneyPerKilowattHour),
        ("_pct_per_year", Unit::Percent),
        ("_pct_per_month", Unit::Percent),
        ("_pct_per_cycle", Unit::Percent),
        ("_kwh_per_year", Unit::KilowattHour),
        ("_cycles_per_year", Unit::Cycle),
        ("_usd_per_year", Unit::Money),
        // Before the bare `_usd`, or a standing charge resolves to plain money and
        // the report drops the "per month" that makes it mean anything.
        ("_usd_per_month", Unit::MoneyPerMonth),
        ("_kwh", Unit::KilowattHour),
        ("_kw", Unit::Kilowatt),
        ("_pct", Unit::Percent),
        ("_usd", Unit::Money),
        ("_celsius", Unit::Celsius),
        ("_c", Unit::Celsius),
        ("_hours", Unit::Hour),
        ("_years", Unit::Year),
        ("_minutes", Unit::Minute),
        ("_multiplier", Unit::Multiplier),
        ("_cycles", Unit::Cycle),
        ("_bytes", Unit::Byte),
        ("_count", Unit::Count),
    ];
    SUFFIXES
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix))
        .map(|(_, unit)| *unit)
}

/// Round a money amount to whole minor units (cents), half away from zero.
///
/// Money is accumulated in `f64` at full precision and rounded exactly once, at
/// the boundary where it becomes an output. Rounding each interval instead would
/// accumulate up to half a cent of bias across 35,040 intervals. See
/// `docs/MODEL.md` for the argument in full.
///
/// # Why not `(value * 100.0).round() / 100.0`
///
/// Because multiplying by 100 is itself inexact, and the error decides the
/// rounding. The nearest `f64` to `0.145` is a hair *below* it, so `0.145 * 100.0`
/// is `14.499999999999998` and rounds *down* to `0.14` — but a user with a
/// 0.145 USD/kWh tariff and 1 kWh of import checks the arithmetic by hand, gets
/// 0.15, and files a bug. Meanwhile `2.675 * 100.0` lands on exactly `267.5` and
/// rounds *up*. The naive form does not even fail consistently: it gives whichever
/// answer the representation error happens to produce.
///
/// Tonans promises hand-checkable numbers, and hands work in decimal. So the
/// rounding is applied to the value's shortest round-trip decimal form — the exact
/// digits the report prints — and never to its binary expansion.
#[must_use]
pub fn round_money(value: f64) -> f64 {
    // `{}` on an f64 renders the shortest decimal string that round-trips, and
    // never uses exponent notation. It is what the reader of a report sees, so it
    // is what gets rounded.
    let text = format!("{value}");
    let Some(point) = text.find('.') else {
        // No fractional part: an integer, an infinity, or a NaN. Nothing to round.
        return value;
    };
    let (whole_text, fraction) = (&text[..point], &text[point + 1..]);
    if fraction.len() <= 2 {
        return value; // already at or coarser than cent precision
    }

    // A shortest representation with three or more fraction digits belongs to a
    // value well inside 2^52, so the cent count cannot overflow an i64.
    let negative = whole_text.starts_with('-');
    let digits = whole_text.trim_start_matches('-');
    let (Ok(whole), Ok(cents)) = (digits.parse::<i64>(), fraction[..2].parse::<i64>()) else {
        return value;
    };

    // The sign is stripped, so "round the magnitude up when the next digit is 5 or
    // more" *is* half-away-from-zero.
    let mut minor = whole * 100 + cents;
    if fraction.as_bytes()[2] >= b'5' {
        minor += 1;
    }
    let magnitude = minor as f64 / 100.0;
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

/// Round a physical quantity to a stable display precision.
///
/// Applied only to serialized outputs, never to a value that is fed back into the
/// engine, so it cannot compound.
#[must_use]
pub fn round_physical(value: f64) -> f64 {
    (value * 1.0e6).round() / 1.0e6
}

/// Normalize a float for canonical serialization.
///
/// Collapses negative zero, which is arithmetically equal to positive zero but
/// serializes differently and would otherwise perturb a project or result hash.
#[must_use]
pub fn normalize_zero(value: f64) -> f64 {
    if value == 0.0 {
        0.0
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_suffix_wins() {
        assert_eq!(
            unit_for_field("import_price_per_kwh_usd"),
            Some(Unit::MoneyPerKilowattHour)
        );
        assert_eq!(
            unit_for_field("demand_price_per_kw_month_usd"),
            Some(Unit::MoneyPerKilowattMonth)
        );
        assert_eq!(unit_for_field("grid_import_kwh"), Some(Unit::KilowattHour));
        assert_eq!(unit_for_field("peak_import_kw"), Some(Unit::Kilowatt));
        assert_eq!(unit_for_field("npv_usd"), Some(Unit::Money));
        assert_eq!(
            unit_for_field("calendar_fade_pct_per_year"),
            Some(Unit::Percent)
        );
    }

    #[test]
    fn unlabelled_names_resolve_to_nothing() {
        assert_eq!(unit_for_field("name"), None);
        assert_eq!(unit_for_field("scenario_id"), None);
    }

    #[test]
    fn money_rounds_half_away_from_zero_at_the_cent() {
        assert!((round_money(1.005) - 1.01).abs() < 1e-12);
        assert!((round_money(-1.005) + 1.01).abs() < 1e-12);
        assert!((round_money(0.004) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn the_half_cent_is_decided_by_the_decimal_and_not_by_binary_error() {
        // The regression. `(v * 100.0).round() / 100.0` scales through an inexact
        // product, and the error — not the rule — picks the winner. Each of these
        // is a decimal half-cent; every one must round away from zero, and the
        // naive form gets the starred ones wrong in opposite directions.
        for (value, expected) in [
            (0.145_f64, 0.15_f64), // * naive: 14.499999999999998 -> 0.14
            (1.005, 1.01),         // * naive: 100.49999999999999 -> 1.00
            (2.675, 2.68),         //   naive: exactly 267.5      -> 2.68, right by luck
            (8.835, 8.84),
            (1234.565, 1234.57),
            (-0.145, -0.15),
            (-1.005, -1.01),
        ] {
            let rounded = round_money(value);
            assert!(
                (rounded - expected).abs() < 1e-12,
                "round_money({value}) = {rounded}, expected {expected}"
            );
        }

        // A half-cent that *is* exactly representable (an odd eighth: 0.125 * 100
        // is exactly 12.5) must round away from zero too — the rule is one rule.
        assert!((round_money(0.125) - 0.13).abs() < 1e-12);
        assert!((round_money(-0.125) + 0.13).abs() < 1e-12);

        // Below the half-cent it still rounds down, however many nines follow.
        assert!((round_money(0.1449999) - 0.14).abs() < 1e-12);
    }

    #[test]
    fn rounding_money_is_total() {
        // It runs on every money field of every export, so it may not panic on any
        // f64 the engine can hand it.
        assert!(round_money(f64::NAN).is_nan());
        assert_eq!(round_money(f64::INFINITY), f64::INFINITY);
        assert_eq!(round_money(f64::NEG_INFINITY), f64::NEG_INFINITY);
        assert!((round_money(0.0) - 0.0).abs() < 1e-12);
        assert!((round_money(1e15) - 1e15).abs() < 1.0);
        assert!((round_money(f64::MIN_POSITIVE) - 0.0).abs() < 1e-12);
        // Already whole cents: returned untouched, not re-derived.
        assert!((round_money(12.34) - 12.34).abs() < 1e-12);
    }

    #[test]
    fn negative_zero_is_collapsed() {
        assert!(normalize_zero(-0.0).is_sign_positive());
        assert!((normalize_zero(-1.5) + 1.5).abs() < 1e-12);
    }

    #[test]
    fn money_symbols_carry_the_project_currency() {
        assert_eq!(Unit::Money.symbol_with_currency("EUR"), "EUR");
        assert_eq!(
            Unit::MoneyPerKilowattHour.symbol_with_currency("EUR"),
            "EUR/kWh"
        );
        assert_eq!(Unit::MoneyPerMonth.symbol_with_currency("EUR"), "EUR/month");
        assert_eq!(Unit::KilowattHour.symbol_with_currency("EUR"), "kWh");
    }

    #[test]
    fn a_recurring_charge_is_not_an_amount_of_money() {
        // The regression, found by the units contract in `tests/units_contract.rs`:
        // `fixed_charge_usd_per_month` matched no suffix at all, so a 45 USD/month
        // standing charge reached the scenario card, the report, and the JSON export
        // carrying *no unit* — and the first suffix it would have hit, had the table
        // been reordered carelessly, is the bare `_usd`, which renders it as a
        // one-off 45 USD. Both readings are wrong, and neither is visibly wrong.
        assert_eq!(
            unit_for_field("fixed_charge_usd_per_month"),
            Some(Unit::MoneyPerMonth)
        );
        assert_eq!(Unit::MoneyPerMonth.symbol(), "USD/month");

        // The neighbours it must not be confused with.
        assert_eq!(
            unit_for_field("demand_price_per_kw_month_usd"),
            Some(Unit::MoneyPerKilowattMonth)
        );
        assert_eq!(unit_for_field("capex_usd"), Some(Unit::Money));
        assert_eq!(
            unit_for_field("degradation_pct_per_month"),
            Some(Unit::Percent)
        );
    }
}
