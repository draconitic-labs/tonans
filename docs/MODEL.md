# The model

Tonans performs **deterministic single-bus energy accounting**. This document states
the calculation order, attribution conventions, and validation boundary so every
planning result can be reviewed rather than accepted as a black box.

## What the model is not

- **Not an AC power-flow study.** Voltage, reactive power, phase angle, imbalance,
  impedance, and short-circuit duty are not modeled.
- **Not protection, conductor, interconnection, or compliance analysis.**
- **Not real-time dispatch or control.** Tonans accepts and emits no equipment
  commands.
- **Not an electrochemical lifetime model.** Degradation is a transparent linear
  fade and throughput-cost approximation.
- **Not an optimizer.** Named dispatch rules use no future load, generation, or
  price information. Tonans does not claim an optimum it did not compute.
- **Not a guarantee of savings.** Results are planning estimates for human review.

## Time and units

Time is an exact fixed grid: explicit timezone, interval duration, start, and count.
Every boundary must remain aligned to the declared local-clock interval across the
full span. A fractional-hour timezone transition that would move a later boundary
off that grid is refused rather than allowing an interval to straddle a tariff hour
or local calendar month.
The duration must divide an hour. Every imported timestamp must land exactly on that
grid; gaps are refused unless the user selects a bounded synthesis policy, and any
synthesis remains visible as a critical warning.

Within an interval, kW values are treated as constant and converted to kWh using the
declared interval duration. Battery charge and discharge quantities below are AC-bus
energy unless explicitly described as energy stored inside the battery.

## Exact interval order

The following order is part of the numeric model:

1. **Standing loss.** Self-discharge is removed from the stored-energy buckets first,
   pro-rated over a 730-hour nominal month. It cannot pull storage below the hard
   minimum SOC.
2. **Temperature factors.** The temperature table is linearly interpolated, holding
   the nearest endpoint outside its range and emitting a warning. Its **power factor**
   caps both charge and discharge power. Its **usable-capacity factor** creates a
   temporary inaccessible floor across the battery's warm usable window: the
   unavailable fraction is added above the hard minimum SOC.
3. **Envelope.** Charge headroom and discharge availability are calculated after the
   standing loss. Charge and discharge are bounded by battery power, inverter power,
   interval duration, one-way efficiency, the hard SOC bounds, and the temperature
   constraints. Grid-connected discharge also honors reserve and policy floors.
   During an outage the reserve and policy floors are released, but the hard minimum
   and temporary temperature floor remain.
4. **Policy proposal.** Grid-connected operation asks the named policy for one signed
   battery setpoint. A positive value discharges; a negative value charges. An outage
   bypasses the grid-connected policy and uses the islanding rule below.
5. **Clamping.** The single signed setpoint is clipped to the interval envelope.
   Charge and discharge are alternate branches of that one decision, so simultaneous
   charge and discharge is impossible by construction.
6. **Bus settlement.** The remaining net flow becomes either grid import or grid
   export, never both. If export would exceed the site limit, controllable battery
   discharge is reduced first and onsite generation is curtailed second.
7. **Storage and source buckets.** Charge efficiency is applied before energy enters
   storage; discharge efficiency determines the internal withdrawal required for AC
   delivery. Stored energy and its source buckets are updated, conversion loss is
   recorded, and SOC extrema are observed.
8. **Tariff accounting.** Interval import cost and export credit are accumulated at
   that interval's rates. Local calendar month and demand-period peaks are updated
   for later demand-charge calculation.

Temperature derating does **not** destroy stored energy. When the usable-capacity
factor raises the temporary floor above the current deliverable range, that energy
is inaccessible for discharge during the cold interval but remains in the source
buckets. If the battery warms and the floor falls, the stored energy becomes
available again. The power factor still caps both directions independently.

## Outage order

Grid import and export are zero. Non-critical load is shed and reported separately.
Onsite generation serves critical load first. A deficit is met from storage down to
the hard/temperature floor, with any remainder reported as unmet critical load. A
surplus charges the battery within the envelope and is otherwise curtailed. The
reserve floor is intentionally released because outage use is what the reserve is
for.

## Flow and source attribution

A single bus does not reveal the physical path taken by indistinguishable electrons,
so Tonans uses one explicit accounting convention:

- onsite generation is attributed to served load first, battery charging second,
  and direct grid export third;
- battery discharge is attributed to remaining served load first and grid export
  second;
- any remaining charge is attributed to the grid;
- stored energy is tracked in three buckets: onsite-generation origin, grid origin,
  and energy present at model start whose source is unknown;
- withdrawals remove those stored buckets proportionally, then allocate the
  efficiency-adjusted output between load and export using the same load/export
  fraction.

The convention closes every reported source flow without pretending the model can
identify a physical electron. Initial stored energy remains `unknown`; Tonans does
not silently relabel it as renewable.

### Self-consumption and self-sufficiency

Self-consumption is the share of available onsite generation used directly or sent
to storage, less generation-origin battery energy later exported.

Self-sufficiency is the share of **served load met by onsite generation directly or
after storage**. Grid-origin battery energy and unknown initial battery energy are
excluded. Sensitivity cells are complete reruns and use this same definition; a
metric does not change meaning merely because an input is being swept.

## Energy identities

Each interval checks the AC-bus identity:

```text
generation used + grid import + battery discharge
  = served load + grid export + battery charge
```

Generation also closes as:

```text
generation available = generation used + curtailed generation
```

Storage closes separately:

```text
ending stored energy
  = starting stored energy - standing loss
    + charge * charge efficiency
    - discharge / discharge efficiency
```

Conversion loss is the difference between AC-bus charge/discharge and the associated
internal storage change. Residuals and losses are carried into results and reports;
the invariant suite fails if the balance exceeds tolerance.

## Money and tariffs

Interval money is accumulated in `f64` without cent-rounding each row. Customer-facing
totals are rounded once at the output boundary using decimal half-away-from-zero at
the cent. Machine-readable interval values retain sufficient decimal precision for
their sum to reconcile with the unrounded accumulator before the final cent rounding.

Tariff support includes:

- flat or per-interval import energy rates;
- flat or per-interval export credits;
- negative import prices, with a warning;
- local-calendar-month demand charges by labeled demand period; and
- fixed monthly charges.

## Economics

Tonans reports CAPEX, annual OPEX, replacement assumptions, simple and discounted
payback, and a simple NPV against the no-storage baseline. It includes no tax,
incentive, depreciation, or residual value.

If a data set is shorter than a year, period results are multiplied by the stated
annualization factor. A warning explains that demand charges and seasonal behavior
do not necessarily scale with elapsed time.

The configured energy-price escalation rate applies **only to energy savings**:
import-energy charges and export-credit differences. Demand-charge and fixed-charge
savings do not escalate. Annual OPEX and throughput wear cost are held constant in
nominal terms; replacement follows the explicit replacement assumption. If the user
selects capacity-proportional benefit scaling, the documented fade factor is applied
to modeled benefits, but Tonans does not rerun dispatch for every future year.

## Determinism and hashes

`tonans-core` has no ambient clock or randomness. Observable ordering is explicit,
negative zero is normalized before hashing, and the long-horizon calculation avoids
platform-dependent transcendental functions.

The supported reproducibility claim is: **the same canonical model inputs, the same
`MODEL_VERSION`, and the same supported build target produce the same numeric
`results_hash`.** Presentation files may differ because generation timestamps,
metadata, and nonnumeric prose are excluded from the numeric result identity.

`model_input_hash` covers the data set, scenarios, selected baseline, and model
version. `results_hash` covers the numeric results after nonnumeric generation
metadata is removed. `MODEL_VERSION` changes whenever an unchanged canonical input
could produce a different number.

## Validation

| Evidence | Scope |
|---|---|
| Golden cases | Hand-calculable battery, loss, cap, reserve, outage, curtailment, tariff, and demand-charge cases. |
| Invariants | Energy closure, bounded SOC, one-direction battery flow, nonnegative unmet load, repeat hashes, and money reconciliation. |
| Import abuse | Malformed headers, unit mismatch, non-finite values, duplicate or off-grid timestamps, DST ambiguity, gaps, oversized inputs, and hostile names/paths. |
| Unit contract | Every numeric field in a populated serialized result resolves to an explicit unit. |
| Public reference | A pinned, normalized SSC battery-state replay covering floor enforcement, clipping, throughput, import, and loss accounting. |

### Public-reference comparison

The public-reference gate is closed by `docs/BENCHMARK.md` and
`crates/tonans-core/tests/ssc_capacity_reference.rs`. The reference is pinned to SSC
commit `86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832` and reproduces the published
battery-state request sequence under normalized matching assumptions.

This is deliberately a **narrow normalized SSC battery-state reference replay**. It
is not SAM certification, an endorsement by SSC or its maintainers, or evidence of
full SSC/SAM parity. The benchmark document lists the normalization, expected values,
tolerance, and upstream lines so its limits are auditable.

An independent review by a qualified energy analyst remains desirable before anyone
relies on Tonans for consequential planning.

## Warnings

Warnings are never filtered for convenience. Every relevant warning travels through
the scenario result, report, and provenance manifest, sorted by severity. A
synthesized interval always creates a critical warning that remains attached to the
data set and every downstream artifact.

Support: `support@flintglade.com`
