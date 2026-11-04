# Tonans product specification

## Product boundary

**Tonans - Microgrid Scenario Workbench** is a local Windows application for
comparing battery-storage and microgrid planning scenarios. It performs deterministic
single-bus energy accounting. It does not calculate AC power flow, short-circuit
duty, protection coordination, conductor sizing, interconnection compliance,
real-time dispatch, or device commands.

Every screen and report must identify results as planning estimates for human
review, not stamped designs or guaranteed savings.

## Required workflow

1. Create or reopen a versioned local project.
2. Import validated interval CSV data for load, critical load, renewable generation,
   energy/import/export rates, and optional demand-charge periods.
3. Define explicit battery, inverter, reserve, degradation, outage, tariff, and
   economic assumptions with units.
4. Run baseline and candidate scenarios using named dispatch policies.
5. Compare energy balance, grid exchange, peak demand, state of charge, curtailment,
   losses, survivability, unmet critical load, and economics.
6. Run bounded sensitivities over capacity, power, reserve, tariff, and degradation
   assumptions.
7. Export a self-contained HTML report plus machine-readable CSV/JSON and a
   provenance manifest containing input hashes, project hash, application version,
   warnings, units, and assumptions.

## Deterministic model requirements

- Fixed interval ordering with explicit timezone and duration.
- Charge and discharge limits, usable SOC envelope, reserve floor, asymmetric
  efficiencies, inverter limits, self-discharge, temperature derating, and
  prevention of simultaneous charge/discharge.
- Dispatch policies: self-consumption, peak-limit, reserve-first, and an explicit
  manual schedule.
- Grid-connected accounting for import, export, curtailment, energy charges, export
  credits, and monthly demand charges.
- Outage replay for critical-load served, unserved energy, reserve depletion, and
  survivability duration.
- Transparent annual fade and throughput-cost degradation approximation; never
  describe it as an electrochemical lifetime model.
- CAPEX, OPEX, replacement assumptions, simple NPV, payback, and comparison against
  a no-storage baseline.
- Warnings whenever input or model limits make a result unreliable.

## Safety and privacy boundaries

- Bind only to random loopback ports and serve only embedded assets.
- Require an unguessable launch token for control APIs.
- Use a restrictive CSP and no remote scripts, fonts, analytics, ads, or telemetry.
- Enforce request, CSV, project, scenario, and sensitivity size limits.
- Store projects only under the workspace the user explicitly selects; crash-safe
  writes, path confinement, and reparse-point defenses are mandatory.
- Never accept or emit equipment-control commands.

## Limits

A project holds up to 32 scenarios, one run holds up to 420,480 rows (scenarios multiplied
by intervals), and a sensitivity study runs up to 256 cells. These limits are a safety
envelope that keeps a run bounded. Exports have no usage restrictions
beyond the Apache License 2.0.

## Observable release-ready finish line

- Golden cases: no battery, ideal shifting, round-trip loss, charge/discharge and
  inverter caps, SOC reserve, outage depletion, export curtailment, and monthly
  demand charge.
- Invariants: interval energy conservation, bounded SOC, no simultaneous
  charge/discharge, nonnegative unmet load, deterministic repeat output, and
  consistent money totals.
- Import tests: malformed headers, unit mismatch, NaN/infinity,
  duplicate/non-monotonic timestamps, gaps, DST/timezone ambiguity, oversized files,
  and hostile filenames/paths.
- UI smoke: import -> configure -> run -> compare -> sensitivity -> export -> reopen.
- One documented comparison against an established public tool or published
  reference case using matching assumptions; differences and scope must be explicit.
- Formatting, lint, tests, dependency audit, production build,
  signed-or-explicitly-unsigned status, SBOM, notices, checksums, binary/package scan,
  and fresh-context review all recorded against the final integrated artifact.
- The Windows release candidate is locally launchable from a desktop shortcut but
  remains unpublished until the final integrated gates pass. Independent review by a
  qualified energy analyst is recommended.
