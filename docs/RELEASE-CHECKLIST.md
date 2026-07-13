# Release checklist — Tonans 0.1.0

**Status: release candidate for operator testing. NOT PUBLISHED, and not ready to be.**

Three of the gates in `PRODUCT-SPEC.md` are **not closed**. They are listed first,
because a checklist that buries its failures under its passes is a marketing
document.

Reproduce every command below with this toolchain on the `PATH`:

```powershell
$env:PATH='C:\Users\zachb\.cargo\bin;<llvm-mingw>\bin;'+$env:PATH
```

---

## Blocking — must close before publication

### 1. No independent comparison against an established tool or reference case

`PRODUCT-SPEC.md` requires *"one documented comparison against an established public
tool or published reference case using matching assumptions; differences must be
explained, not hidden."*

**This has not been done.** Tonans' internal consistency is well covered — golden
hand calculations, energy conservation, deterministic reruns, money totals — but
internal consistency is not correctness. A model can be perfectly self-consistent
and still disagree with the rest of the world, and right now we do not know whether
it does.

Until this exists, Tonans' agreement with reality is **asserted, not demonstrated**.
This is the single biggest reason the build is a release candidate rather than a
product. It is also recorded in `docs/MODEL.md`, which ships to the customer, so
nobody can buy it without being told.

### 2. No fresh-context review

`AGENTS.md` requires a fresh-context review as release evidence. The review performed
in this slice was a **self-review by the context that wrote the code**, which is worth
something but is not the thing being asked for — it shares every blind spot of the
work it is reviewing.

An independent reviewer, and preferably an independent **energy analyst**, should read
`crates/tonans-core/src/engine.rs` and `economics.rs` against `docs/MODEL.md` before
anyone relies on a number.

### 3. No browser DOM smoke test

`PRODUCT-SPEC.md` requires *"UI smoke: import → configure → run → compare →
sensitivity → export → reopen."*

What **is** covered (`packaging/smoke.ps1`, on the packaged binary):

- the loopback bind, the token gate, the wrong-token refusal, the DNS-rebinding
  `Host` guard, the cross-origin refusal, the CSP and frame headers;
- the demonstration project installed on first launch **through the real importer**;
- `import → run → export → verify` end to end via `tonans run`, including the export
  bundle's own checksums, the report's boundary text and absence of script, and an
  energy-balance residual under 1e-6 kWh.

What is **not** covered: driving the actual DOM — clicking through configure,
compare, and sensitivity in a real browser. There is no headless-browser harness in
this repository. The API path underneath each screen is exercised; the screens
themselves have been exercised only by hand.

**The operator must click through the workflow manually before this ships.**

---

## Closed — with the command that closes it

### Formatting, lint, tests

```powershell
cargo +stable-x86_64-pc-windows-gnullvm fmt --all -- --check
cargo +stable-x86_64-pc-windows-gnullvm clippy --workspace --all-targets --locked -- -D warnings
cargo +stable-x86_64-pc-windows-gnullvm test --workspace --all-targets --locked
```

All three pass. Clippy is `-D warnings` on the default lint set, plus the crate's own
`#![warn(missing_docs)]` and `#![warn(clippy::doc_markdown)]`, plus the workspace
denials of `todo!`, `unimplemented!`, and `dbg!`. `unsafe_code` is **forbidden**
workspace-wide, and `clippy.toml` bans wall-clock reads outside the one audited call
site.

Not run at `clippy::pedantic`: it objects to the hex colour literals in the palette
and the single-character bindings in the colour-vision maths, neither of which is a
defect, and silencing forty pedantic lints to claim a stricter gate would be theatre.

### Dependency audit

```powershell
cargo audit
```

**Clean.** 37 crates in the locked tree, 0 advisories. No upgrades were made: there
was nothing actionable, and a speculative bump is a change without a reason.

`getrandom` is pinned to 0.2 deliberately — 0.3 reaches Windows' CSPRNG through a
`raw-dylib` import the `windows-gnu` toolchain cannot build. The pin is a
compatibility constraint, is documented in `docs/ARCHITECTURE.md`, and is clean.

### Production build, SBOM, notices, checksums

```powershell
.\packaging\package.ps1
```

Produces `packaging/out/tonans-0.1.0-windows-x64.zip`, an external `.sha256` sidecar,
and inside the ZIP: `SHA256SUMS.txt`, `SBOM.cdx.json` (CycloneDX 1.5, 25 shipped
crates), and `NOTICES.md` (22 distinct third-party licence texts).

The release build passes `--remap-path-prefix`. See **Findings**, below.

### Package scan

```powershell
.\packaging\verify-package.ps1
```

13 checks, all passing: exact file allowlist (no missing, no extra); Windows version
metadata naming Flintglade only; no mail domain other than `flintglade.com` anywhere,
binary included; no build-machine path; no payment integration or test secret; no
remote origin in any document we wrote; no `0.0.0.0` in the binary; internal and
external checksums; a well-formed SBOM; and the package stating that it is unsigned.

### Packaged-binary smoke

```powershell
.\packaging\smoke.ps1
```

13 checks, all passing. Runs the *packaged* binary against a temporary workspace,
never the operator's real one, and stops it through its own shutdown route rather
than killing it — so a failure to exit cleanly is itself a finding.

### Signing

**Explicitly unsigned.** `PRODUCT-SPEC.md` permits "signed-or-explicitly-unsigned
status, recorded" — this is the recording.

Windows SmartScreen will warn that the publisher cannot be confirmed, and it is right
to. The package says so in `README.md`, `LICENSE.md`, and `SECURITY.md`, and publishes
checksums instead. An honestly unsigned binary with a published digest is better than
an implied assurance nobody bought.

### Golden cases, invariants, import abuse, unit contract

181 tests pass (`cargo test --workspace --all-targets --locked`).

Golden hand calculations live in `engine.rs` and `economics.rs`; import-abuse cases
in `csvimport.rs` cover malformed headers, unit mismatch, NaN/infinity, duplicate and
non-monotonic timestamps, gaps, DST ambiguity, oversized files, and hostile filenames.

Two integration suites were **missing** — the source referenced
`tests/units_contract.rs` and `tests/invariants.rs`, and neither file existed. Both
now do:

- `tests/invariants.rs` — energy conservation per interval, bounded SOC inside the
  usable envelope, no simultaneous charge/discharge, non-negative unmet and shed
  load, identical `model_input_hash` and `results_hash` across reruns, and money
  components summing to the total. Asserted over a two-week fixture with a battery,
  a time-of-use tariff, a demand charge over two billed months, a constrained
  interconnection, and an unsurvivable outage — and then over thousands of
  pseudo-random scenarios.
- `tests/units_contract.rs` — serializes a fully-populated result tree, walks every
  numeric leaf, and fails if the name it sits under does not resolve to a unit.

Both carry **non-vacuity assertions**, and those earned their place immediately: the
first fixture had a PV array that peaked *below* the site's midday load, so no
surplus was ever produced and the export, export-credit, and curtailment branches
were never taken. The energy balance "conserved" because the terms in question were
all zero. The unit contract then found finding 6 above. A test that cannot fail is
not evidence, and these two nearly weren't.

---

## Findings from this slice

Six defects were found and fixed. Each has a regression test.

| | Defect | Fix |
|---|---|---|
| 1 | **Money rounding was decided by binary error, not by a rule.** `(v * 100.0).round() / 100.0` rounds `0.145` *down* to 0.14 (the nearest double is below the half-cent) and `2.675` *up* to 2.68 (the scaled product lands exactly on 267.5). An analyst checking a 0.145 USD/kWh tariff by hand gets 0.15 and files a bug. | Round the shortest round-trip **decimal** form — the digits the report prints — half away from zero. `units.rs` |
| 2 | **CSV formula injection: `-` was not defused.** A leading `-` was excused as "a negative number", but `field()` only ever renders *names* — numbers go through `number`/`money` — and Excel evaluates `-1+1+cmd\|'/c calc'!A0`. A defused field was also left unquoted, which an importer that trims whitespace can undo. | Defuse `= + - @ TAB CR`, and always quote a defused field. `export.rs` |
| 3 | **Unbounded allocation in the importer.** `max_consecutive_intervals` arrives unvalidated in a JSON body; the synthesized-fraction ceiling and `Dataset::validate` both run *at the end*, on memory already committed. Two rows far enough apart, with a permissive gap policy, synthesized the whole span first. | Bound the policy up front, and check the running interval count **before** each gap is filled. `csvimport.rs` |
| 4 | **The audit trail was sanitized, not validated.** `source_file_name` silently rewrote `../../evil.csv` to `evil.csv` and recorded that in the provenance manifest — a repair, in the one place a repair is least defensible. The browser sends `file.name`, which is never a path, so nothing legitimate needed the trimming. | Refuse it. `paths.rs` |
| 5 | **The release binary embedded the packager's Windows username.** Rust puts dependency source paths in panic metadata; they sit under the build machine's home directory, and `strip = "debuginfo"` does **not** remove them. | `--remap-path-prefix` in `package.ps1`; `verify-package.ps1` greps the artifact and fails the release if any survives. |
| 6 | **A standing charge reached the reader with no unit.** `fixed_charge_usd_per_month` matched no suffix in the unit table, so a 45 USD/month fixed charge rendered on the scenario card, in the report, and in the JSON export as a bare number — and the first suffix a careless reordering would have hit is `_usd`, which renders it as a one-off 45 USD. Both readings are wrong and neither is visibly wrong. Found by the new units contract. | New `Unit::MoneyPerMonth`, and a `_usd_per_month` suffix ahead of `_usd`. `units.rs` |

Reviewed and found sound, no change needed: the launch token (256-bit CSPRNG,
never persisted, constant-time compare), the `Host` pin and `Origin` check, the CSP
and security headers, the body/header/rate bounds and the absence of chunked framing,
path confinement in `store.rs` and `export.rs`, and entitlement enforcement — which
is checked at the point of *use* (`run_project`, `sensitivity::run`), not only at
save, so hand-editing a project file on disk does not buy a scenario.

---

## Not done, deliberately

- **No desktop shortcut.** The task forbade creating one, and `PRODUCT-SPEC.md`'s
  "launchable from a desktop shortcut" is the operator's step, not the build's.
- **No Stripe product, no payment integration, no live licensing origin.** Sales are
  disabled. `verify-package.ps1` fails the release if any of it reaches the artifact.
- **Not published.** No tag, no release, no upload. `packaging/` cannot publish; none
  of the three scripts has a network call in it.

## The operator's next steps

1. Verify the ZIP's SHA-256 against the sidecar.
2. Extract it and click through: import → configure → run → compare → sensitivity →
   export → reopen. **This is gate 3 above, and only you can close it.**
3. Read `docs/MODEL.md` and decide whether the boundary it draws is one you are
   willing to sell against.
4. Commission gates 1 and 2 — the independent comparison and the fresh-context
   review — before publishing anything.
