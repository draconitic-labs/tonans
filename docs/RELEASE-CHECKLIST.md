# Release checklist - Tonans release candidate

**Status: not published, not for sale, and reserved for operator testing.**

The numeric public-reference gate and final integrated automated gates are closed for
the exact operator-test artifact below. The operator's real-browser DOM walkthrough
remains pending. Evidence belongs to the exact artifact being considered for release;
an earlier green run does not bless a later binary.

Verified 2026-07-13:

- archive SHA-256: `da7809d611a517fa0081ea1ac42f50d5736d11fddc58a81d7b872bb3fdf8a3d8`;
- runtime: `Tonans 0.1.0 (model 1.1.1, schema v1)`;
- formatting, strict Clippy, 212 tests, and locked RustSec audit passed;
- closed-package verifier passed 15/15 checks;
- sanitized packaged-binary smoke passed 12/12 checks;
- CycloneDX 1.5 JSON Schema validation passed with the official SPDX and JSF schemas;
- Grok 4.5 maximum reasoning and an independent fresh-context Codex review both
  returned GO for the exact reviewed source and archive digest; and
- Authenticode status is intentionally `NotSigned` for this private operator test.

Use the stable GNU/LLVM Rust target and place Cargo before llvm-mingw:

```powershell
$env:PATH="$env:USERPROFILE\.cargo\bin;<llvm-mingw>\bin;$env:PATH"
```

## Pending before publication

### Operator browser walkthrough

The operator must launch the final staged executable and click through the actual
DOM workflow:

```text
import -> configure -> run -> compare -> sensitivity -> export -> reopen
```

Automated tests and the packaged-binary smoke exercise the HTTP boundary and model
path, but they do not drive the browser DOM. The operator should confirm visible
units, warnings, source-flow metrics, assumption text, sensitivity results, export
links, and project reopening. Record the tested package digest when this passes.

### Final integrated gate rerun

After the release branches are combined, rerun every command below against the exact
source revision used to make the staged package. This includes a fresh-context review
of the final diff and package, not only a review of an earlier implementation.

The self-contained-runtime check must run in a sanitized environment that does not
inherit llvm-mingw or another compiler runtime on `PATH`. Executable imports must be
limited to declared Windows system libraries; otherwise every runtime file must be
included, licensed, inventoried in the SBOM, and hashed.

## Closed public-reference gate

`docs/BENCHMARK.md` documents a normalized battery-state replay pinned to SSC commit
`86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832`. It compares requested discharge,
stored energy, SOC-floor clipping, throughput, grid import, losses, and balance under
matching normalized assumptions.

```powershell
cargo +stable-x86_64-pc-windows-gnullvm test `
  -p tonans-core --test ssc_capacity_reference --locked
```

This closes the specified comparison gate only within that documented scope. It is a
**narrow normalized SSC battery-state reference replay**, not SAM certification,
maintainer endorsement, or evidence of full SAM/SSC parity.

## Reproducible release gates

### Format, lint, and tests

```powershell
cargo +stable-x86_64-pc-windows-gnullvm fmt --all -- --check
cargo +stable-x86_64-pc-windows-gnullvm clippy `
  --workspace --all-targets --locked -- -D warnings
cargo +stable-x86_64-pc-windows-gnullvm test `
  --workspace --all-targets --locked
```

These gates have passed during the hardening cycle and must pass again after
integration. The workspace forbids unsafe code and denies `todo!`, `unimplemented!`,
and `dbg!`; core wall-clock access is prohibited except for the audited injected
application clock.

The suite must retain:

- hand-calculated battery, outage, tariff, and economics cases;
- interval and aggregate conservation invariants;
- bounded SOC and one-direction battery-flow invariants;
- deterministic `model_input_hash`/`results_hash` reruns;
- money precision and component reconciliation;
- source attribution and self-sufficiency regressions;
- import abuse, time-grid, path, reparse-point, and project-identity tests;
- HTTP physical-header, preflight-before-body, absolute-deadline, token, Host,
  Origin, and response-header tests;
- unit-contract and pinned SSC reference tests.

Do not record a fixed test count here. The commands and their exit status are the
durable evidence; the count will legitimately grow as regressions are added.

### Dependency audit

```powershell
cargo audit
```

Audit the locked tree. Any advisory is dispositioned explicitly; compatibility pins
are not blanket exceptions.

### Production package

```powershell
.\packaging\package.ps1
```

The package build must:

- use release path remapping so personal and build-machine paths do not ship;
- stage an exact, reviewable payload;
- generate a CycloneDX SBOM, complete third-party notices, internal checksums, and
  an external archive digest;
- include the model, security, privacy, benchmark, licence, and support documents;
- declare the unsigned status; and
- include no payment integration, live licensing origin, or secret.

### Independent package verification

```powershell
.\packaging\verify-package.ps1
```

The verifier must independently check the exact allowlist, Windows version metadata,
Flintglade-only public identity, absence of secrets and build paths, localhost-only
network boundary, checksums, SBOM structure, licence/notice coverage, declared signing
status, and executable runtime imports. It must fail on missing **or extra** files.

### Packaged-binary smoke

```powershell
.\packaging\smoke.ps1
```

Run the staged executable against an isolated temporary workspace and a sanitized
runtime environment. Verify loopback binding, token rejection/acceptance, Host and
Origin refusal, security headers, demonstration-project installation through the
real importer, representative run/export/checksum behavior, and clean shutdown.

Avoid hard-coding a smoke assertion count in this document. The script's current
checks and exit status are the source of truth.

### Fresh-context review

A reviewer who did not implement the final slice must inspect at least:

- `engine.rs`, `economics.rs`, source attribution, and `docs/MODEL.md` together;
- CSV/time-grid/project-store/path boundaries;
- HTTP head/body ordering, deadlines, queue bounds, guards, and response writing;
- entitlement checks at use time;
- package contents, PE imports, SBOM/notices/checksums, and sanitized smoke evidence;
- public claims for parity, determinism, pricing, refund, and licensing scope.

Record findings and regressions. A review of code before a subsequent repair is not
the final review.

## Signing and sales status

The release candidate is explicitly unsigned. Windows SmartScreen may warn that the
publisher cannot be confirmed. The archive sidecar and internal checksums provide
integrity evidence, not publisher authentication.

Sales are disabled. Do not create a Stripe product, enable checkout, embed a live
licensing origin, or claim a refund policy for this candidate. Before any future
sale, finalize the legal terms and refund window, exercise purchase/activation/
recovery/refund/dispute behavior in test mode, and obtain explicit publication
approval.

## Final operator sequence

1. Verify the archive digest and extract it.
2. Complete the browser walkthrough above using the exact staged package.
3. Confirm the boundary text, assumptions, units, warnings, and exports are readable.
4. Record the package digest and outcome.
5. Keep Tonans unpublished until every final integrated gate and the manual test are
   closed.

Support: `support@flintglade.com`
