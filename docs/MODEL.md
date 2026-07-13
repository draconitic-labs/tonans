# The model

Tonans performs **deterministic single-bus energy accounting**. This document says
exactly what that means, what it deliberately does not do, and how it is validated —
because a planning number you cannot argue with is a planning number you should not
trust.

---

## What the model is not

These are not gaps to be filled in a later version. They are the product boundary.

- **Not an AC power-flow study.** Voltage, reactive power, phase angle, and
  short-circuit duty are not modelled. There is one bus and one number for power on
  it.
- **Not a protection or interconnection analysis.** No coordination, no conductor
  sizing, no compliance determination.
- **Not real-time dispatch or control.** Tonans accepts no equipment command and
  emits none.
- **Not a battery lifetime model.** Degradation is a transparent linear fade plus a
  throughput-cost approximation. It is not electrochemical, and Tonans says so on
  every artifact it touches.
- **Not an optimiser.** Dispatch policies are named rules evaluated interval by
  interval with **no view of the future**. A real optimiser with perfect foresight
  would beat every policy here, and would also be lying to you about what a
  controller can actually achieve. Tonans will not claim an optimum it did not
  compute.
- **Not a guarantee of savings.** It is a planning estimate for human review.

## The interval loop

Time is a fixed grid: an explicit timezone, an explicit interval duration that
divides an hour, and a count. Every consecutive timestamp in the source file must be
exactly one interval apart. Nothing is inferred from the data.

Within one interval, in this order:

1. Generation meets load directly.
2. Surplus generation charges the battery, subject to the charge power limit, the
   inverter limit, the usable-capacity ceiling, and the charge efficiency.
3. Remaining surplus exports, subject to any site export limit; whatever cannot
   export is **curtailed**, and counted.
4. A load deficit is met by discharging the battery, subject to the discharge power
   limit, the inverter limit, the reserve floor, and the discharge efficiency.
5. Remaining deficit imports from the grid — or, during an outage, goes unserved:
   non-critical load is **shed**, critical load that cannot be served is **unmet**,
   and both are counted.
6. Self-discharge and temperature derating apply to the stored energy.

**Charge and discharge never happen in the same interval.** The engine cannot
express it, an integration test asserts it, and any dispatch rule that seemed to
want it is a rule with a bug.

Efficiencies are **asymmetric one-way** figures. Round-trip efficiency is derived
from them, never entered directly, because a single round-trip number hides which
side of the conversion the loss happened on — and the two sides do not see the same
tariff.

### Energy conservation

Every interval closes:

```
generation + import + discharge  =  served load + export + curtailed + charge
                                    + conversion loss + self-discharge loss
                                    + change in stored energy
```

The residual is computed, carried into every result, and **printed in the report**,
so a reader can check that the model closed rather than take our word for it. It is
zero to within floating-point noise, and an integration test fails if it is not.

## Money

Money is accumulated in `f64` at full precision and rounded **exactly once**, at the
boundary where it becomes an output. Rounding each interval instead would accumulate
up to half a cent of bias across 35,040 intervals.

The rounding rule is **half away from zero, at the cent** — and it is applied to the
value's shortest round-trip **decimal** form, not to its binary expansion.

That distinction is not pedantry, and it is worth the paragraph. The obvious
implementation is `(value * 100.0).round() / 100.0`, and it is wrong in a way a user
can catch by hand. The nearest `f64` to `0.145` is a hair *below* it, so
`0.145 * 100.0` is `14.499999999999998` and rounds **down** to `0.14` — while an
analyst with a 0.145 USD/kWh tariff and 1 kWh of import does the arithmetic on paper,
gets 0.15, and files a bug. Meanwhile `2.675 * 100.0` lands on exactly `267.5` and
rounds **up**. The naive form does not even fail consistently: it returns whichever
answer the representation error happens to produce.

Tonans promises hand-checkable numbers, and hands work in decimal. So the rounding
happens on the digits the report actually prints.

Cost components sum to the total within a cent, and an integration test asserts it.

## Tariffs

- Energy charges on import, at a flat rate or a per-interval price series.
- Export credit at a flat rate or a per-interval price series.
- **Negative import prices are permitted**, because wholesale markets have them.
  Tonans accounts for them and warns you, because it means importing energy can
  *earn* money — so a dispatch policy that avoids import is not necessarily the
  cheaper one, and a reader who has not noticed will draw the wrong conclusion.
- Monthly demand charges on the peak import in each labelled period, bucketed by the
  **site's local calendar month**, not by elapsed time.
- Fixed charges.

## Economics

CAPEX, OPEX, replacement assumptions, simple and discounted payback, and a simple
NPV against a no-storage baseline.

There is **no tax, incentive, depreciation, or residual value** in this model.
Tonans would have to guess at all four, and a guess in a financial model is worse
than an absence — an absence is visible.

When the data set is shorter than a year, annual figures are the simulated period
multiplied by a stated annualization factor, and the report says so in a banner,
because **demand charges and seasonal load do not scale with elapsed time.** A
three-week data set annualized to a year is an estimate built on an estimate, and
you should be told.

## Determinism

The same project produces **byte-identical** results, on any machine, forever.

- `tonans-core` contains no clock and no randomness. Wall-clock reads are banned by
  a lint across the whole workspace; every timestamp is *injected* by the caller.
- Transcendental functions are avoided in favour of iterated multiplication, because
  `powf` is a libm call and libm is not required to be bit-identical across
  platforms.
- Nothing iterates a hash map. Ordering is explicit everywhere it is observable.
- Negative zero is normalised before hashing, because it is arithmetically equal to
  positive zero but serializes differently.

`model_input_hash` covers exactly the things that determine the numbers — the data
set, the scenarios, the baseline, and the model version — and nothing else.
`results_hash` covers the results with the generation timestamp removed. **Two runs
with the same `model_input_hash` must produce the same `results_hash`**, an
integration test asserts it, and a reader can use the pair to tell whether a
difference between two reports is real or cosmetic.

`MODEL_VERSION` is independent of the application version and is incremented
whenever a change could alter a number for an unchanged input. A documentation or UI
release leaves it alone, so a reader of an old report can tell whether today's build
would still reproduce it.

## How the model is validated

| | |
|---|---|
| **Golden cases** | Hand-calculable scenarios with known answers: no battery, ideal shifting, round-trip loss, charge/discharge and inverter caps, SOC reserve, outage depletion, export curtailment, monthly demand charge. |
| **Invariants** | Asserted over full runs, per interval and in aggregate: energy conservation, bounded SOC, no simultaneous charge/discharge, non-negative unmet load, deterministic repeat output, consistent money totals. |
| **Import abuse** | Malformed headers, unit mismatch, `NaN`/infinity, duplicate and non-monotonic timestamps, gaps, DST ambiguity, oversized files, hostile filenames and paths. |
| **Unit contract** | The result tree is serialized and every numeric leaf must resolve to a unit from its field name. A new quantity cannot reach a report without one. |

### The independent comparison — outstanding

**PRODUCT-SPEC.md requires one documented comparison against an established public
tool or published reference case, with matching assumptions and every difference
explained rather than hidden. That comparison has not been done.**

It is a **release-blocking gap**, it is recorded as such in
`docs/RELEASE-CHECKLIST.md`, and it is the reason this build is a release candidate
for operator testing rather than a published product. Until it exists, Tonans'
agreement with the wider world is asserted and not demonstrated — and this document
would be dishonest if it implied otherwise.

An independent review by a qualified energy analyst is wanted for the same reason.

## Warnings

Whenever an input or a model limit makes a result unreliable, Tonans raises a
warning, carries it through every artifact, and prints **all** of them in the report,
most serious first. None is filtered out for being inconvenient.

A synthesized interval always raises a `critical` warning, and that warning rides on
the data set for the rest of its life — into every scenario, every report, and every
provenance manifest built from it. A filled interval is never silent.

---

Flintglade — `support@flintglade.com`
