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
- **Months billed.** Data touching other than 12 calendar months at the site's time zone now
  raises `cost.billed_months_not_twelve`, saying how many were billed and that each gets a full
  demand and fixed charge. A year that starts mid month, or is stamped in UTC for a site elsewhere,
  touches 13, and a test year's annual grid cost read 72,700 USD where a year starting on the first
  reads 71,600. No number changes. The warning is outside `results_hash`, and inside the identity
  hash of a study over such data. A year touching exactly 12 raises nothing, and its results and
  hashes do not move.
- **Fade wording.** The texts called fade linear, and it has always compounded: usable capacity is
  `initial x (1 - r)^(n - 1)` in year `n`. No number changes.

Other results that could have been wrong before this version, none of which changes the model:

- **A study could disagree with Results by the whole cost of a cost edit.** The study form kept
  the first cost rate it saw and rounded it. If you changed a scenario's installed cost or size
  with that form already open, or switched it to another scenario, the study's net present values
  used the old rate. In the bundled demo, doubling the cost left the study 220,000 USD away from
  Results. If you made a study after editing a cost, redo it.
- **Results could be marked Current when they were not.** A run, a study, or an export that
  finished after you opened another project, or saved a change, was shown as if it described what
  was on screen. That includes one project's results under another's title. If you opened another
  project or saved during a run on a large file, run it again.
- **A second window could delete imported data.** Saving from a window opened before another
  window imported a file put the empty project back. Now the save is refused, and nothing is
  overwritten.
- **A suggested mapping could be 1000 times wrong, or plainly the wrong column.** `kWhLoad` and
  similar headers were read as Wh, `price_eur_per_kwh` as dollars, a backup generator and
  `Non_Critical_Load_kW` as the critical load, `Solar_Irradiance_W/m2` as generation, and a running
  meter total as energy per interval. If you accepted a suggestion for a header like these, check
  the figures on the data screen against a bill. A column now gets a role only when every word of
  its header is recognized and states both the meaning and the unit. **A suggestion is still only a
  suggestion:** it reads a header and not your data, and it always needs your review.

Also changed in what the screen says:

- Year 0 of the cash flow table shows installed cost in its own column, not under Replacement.
- The best cell of a study states the axis value exactly, so it no longer reads 1 for 0.80.
- The Flat energy price preset no longer keeps an earlier demand charge.
- The annual grid cost tile says when its figure is scaled from a short file.
