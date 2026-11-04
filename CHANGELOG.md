# Changelog

Changes that affect what Tonans tells you. A change to a number moves `MODEL_VERSION`, which
is part of the model input hash, so a result names the model that produced it.

## Model 1.2.0

**Earlier sensitivity studies with a price axis were wrong.** If you made a decision from an
import-price or demand-price study produced by model 1.1.1 or earlier, redo the study.

Those axes changed the price for the varied scenario and left the baseline at its own price.
Each cell's saving, net present value, and payback therefore mostly measured the gap between
two prices rather than what storage was worth at either:

- Raising the price made storage look worse, and lowering it made storage look better. For a
  scenario that saves money by cutting import or peak demand, the true direction is usually
  the opposite.
- The size of the effect was far too large. In the bundled demo, peak-limit scenario:

| Demand price (USD/kW-month) | 5 | 10 | 15 | 20 |
|---|---|---|---|---|
| Net present value before (USD) | -53,815 | -152,593 | -251,372 | -350,150 |
| Net present value now (USD) | -204,854 | -165,903 | -126,951 | -88,000 |

| Import price (x) | 0.8 | 1 | 1.2 | 1.5 |
|---|---|---|---|---|
| Net present value before (USD) | -53,854 | -181,557 | -309,260 | -500,814 |
| Net present value now (USD) | -181,118 | -181,557 | -181,996 | -182,654 |

A cell's annual grid cost, peak import, unmet critical load, self-sufficiency, and state of
charge were correct, because those do not depend on the baseline. Studies over size, reserve,
capacity, fade, installed cost, or discount rate are unaffected in their numbers.

Price axes now apply to both scenarios, so each cell equals setting both to that price and
running them. A test holds this to the table above.

What else moves with this version:

- **Hashes.** `model_input_hash` includes the model version, and `results_hash` includes the
  model input hash, so both change for every project, including ones with no study. The
  numbers in a run are unchanged. A study's own hash changes as well, because it now names
  its model version, reports each cell's installed cost, and can carry new warnings.
- **Exports.** The sensitivity CSV gains `installed_cost_usd` and `capex_per_kwh_usd`, and
  the provenance manifest gains `capex_per_kwh_usd`.
