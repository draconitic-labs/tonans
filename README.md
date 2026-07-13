# Tonans

Tonans is Flintglade's local-first microgrid scenario workbench. It compares
transparent battery, tariff, renewable-generation, and outage-planning scenarios
without controlling equipment or presenting planning estimates as certified designs.

## Status: release candidate for operator testing. Not published.

The code gates have passed during the current hardening cycle: formatting, strict
Clippy, the workspace test suite, dependency audit, production build, and package
checks. The final integrated package and fresh-context review gates will be rerun
after the current release changes are combined; `docs/RELEASE-CHECKLIST.md` records
the reproducible commands and does not treat an earlier run as evidence for a later
artifact.

The required public-reference comparison is closed by `docs/BENCHMARK.md` and the
executable SSC regression. Its scope is deliberately narrow: a normalized SSC
battery-state reference replay pinned to SSC commit
`86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832`. It is not certification, endorsement,
or a claim of SAM/SSC parity.

The remaining user-visible release gate is the operator's browser walkthrough of
the real DOM: import -> configure -> run -> compare -> sensitivity -> export ->
reopen. The API and packaged-binary path underneath that workflow are tested, but
automated API coverage is not a substitute for clicking the screens.

Sales remain disabled. There is no purchase flow, licence key, activation server,
Stripe catalog entry, or payment integration in the executable. Every current build
runs in the Evaluation tier.

## What it is

Tonans performs deterministic single-bus energy accounting and produces **planning
estimates for human review**. It is not an AC power-flow study, a protection or
interconnection analysis, a battery lifetime model, an optimiser, or a guarantee of
savings. `docs/MODEL.md` states the boundary in full, and every screen and export
repeats it.

It is local-first in the literal sense: it binds only to `127.0.0.1`, serves assets
compiled into the executable, makes no outbound connection, and stores project data
under the workspace the user selects.

## Documentation

| | |
|---|---|
| `PRODUCT-SPEC.md` | The authoritative requirements and release finish line. |
| `AGENTS.md` | The engineering contract. |
| `docs/MODEL.md` | Model order, assumptions, metrics, limitations, and validation. |
| `docs/BENCHMARK.md` | The pinned, normalized SSC battery-state reference replay. |
| `docs/ARCHITECTURE.md` | Crate boundaries, HTTP design, toolchain, and packaging goal. |
| `docs/SECURITY.md` | Threat model and defenses in request and filesystem order. |
| `docs/PRIVACY.md` | What is collected, where data lives, and how to remove it. |
| `docs/RELEASE-CHECKLIST.md` | Reproducible gates and the remaining operator step. |
| `TERMS.md` | Proposed licence tiers and pricing; sales are disabled. |
| `LICENSE.md` | Proprietary software licence; third-party components remain under their own licences. |

## Building

Windows x64, with the llvm-mingw toolchain:

```powershell
$env:PATH="$env:USERPROFILE\.cargo\bin;<llvm-mingw>\bin;$env:PATH"
cargo +stable-x86_64-pc-windows-gnullvm test --workspace --all-targets --locked
cargo +stable-x86_64-pc-windows-gnullvm clippy --workspace --all-targets --locked -- -D warnings
```

## Packaging

```powershell
.\packaging\package.ps1
.\packaging\verify-package.ps1
.\packaging\smoke.ps1
```

These scripts build and verify a local release candidate. They do not publish,
upload, install, enable sales, or create a shortcut. Publication remains a deliberate
human decision after the integrated gates and operator walkthrough pass.

Support: `support@flintglade.com`.
