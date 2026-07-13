# Public battery-state benchmark

Tonans includes one documented comparison against the open-source SAM Simulation Core (SSC). It is deliberately narrow: a **normalized SSC battery-state reference replay** covering state-of-charge floor enforcement, requested discharge clipping, throughput, grid import, and loss accounting. It is not SAM certification, an NREL endorsement, or evidence of full SSC/SAM parity.

## Pinned reference

- SSC commit: `86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832`
- Upstream project and role: <https://github.com/NatLabRockies/ssc/tree/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832>
- Public capacity-test inputs: <https://github.com/NatLabRockies/ssc/blob/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832/test/shared_test/lib_battery_capacity_test.h#L69-L90>
- Public expected sequence: <https://github.com/NatLabRockies/ssc/blob/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832/test/shared_test/lib_battery_capacity_test.cpp#L47-L71>
- Upstream tolerance convention: <https://github.com/NatLabRockies/ssc/blob/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832/test/shared_test/lib_battery_capacity_test.h#L43-L52>

The reference was pinned so an upstream change cannot silently alter Tonans' release evidence.

## Normalization

The Tonans replay uses four one-hour intervals, no generation, and load equal to the upstream requested discharge sequence: `1.5`, `3`, `490`, and `490` kW. The battery is 1,000 kWh, starts at 50% state of charge, has a 15% hard minimum and 95% maximum, 100% one-way efficiencies, no self-discharge, no temperature derating, and 1,000 kW power ceilings. A manual schedule requests the same discharge sequence.

Expected values, with absolute tolerance `0.01`:

| Interval | Stored energy (kWh) | SOC (%) | Delivered discharge (kWh) |
|---:|---:|---:|---:|
| 1 | 498.5 | 49.85 | 1.5 |
| 2 | 495.5 | 49.55 | 3.0 |
| 3 | 150.0 | 15.0 | 345.5 |
| 4 | 150.0 | 15.0 | 0.0 |

The aggregate expectations are 350 kWh battery discharge, 634.5 kWh grid import, 500/150 kWh initial/final stored energy, 0.4375 equivalent full cycles against the 800 kWh usable window, zero modeled losses, zero energy-balance residual, and exactly two intervals clipped by the hard energy floor.

## Reproduction

From the repository root:

```powershell
cargo test -p tonans-core --test ssc_capacity_reference --locked
```

The executable regression is `crates/tonans-core/tests/ssc_capacity_reference.rs`. Any change in the numeric model must keep this case passing or document why the pinned reference is no longer comparable.
