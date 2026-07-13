# Tonans

Tonans is Flintglade's local-first microgrid scenario workbench. It compares
transparent battery, tariff, renewable-generation, and outage-planning scenarios
without controlling equipment or pretending to be a safety-certified design
authority.

## Status: release candidate, for operator testing. Not published.

The build gates pass — formatting, strict clippy, the full test suite, `cargo audit`,
a production build, a packaged-binary smoke test, an SBOM, notices, checksums, and a
package scan. `docs/RELEASE-CHECKLIST.md` records each one with the command that
closes it.

**Three gates from `PRODUCT-SPEC.md` are still open, and they block publication:**

1. **No independent comparison** against an established public tool or published
   reference case. Tonans is internally consistent — golden hand calculations,
   energy conservation, deterministic reruns — but internal consistency is not
   correctness, and we do not yet know whether the model agrees with the rest of
   the world.
2. **No fresh-context review.** The review so far was done by the context that wrote
   the code, and shares its blind spots. An independent energy analyst should read
   the engine against `docs/MODEL.md`.
3. **No browser DOM smoke test.** The HTTP boundary and the
   `import → run → export` pipeline are covered end to end on the packaged binary;
   clicking through the actual screens is not automated.

Sales are not enabled: there is no purchase flow, no licence key, no activation
server, and no payment integration in the executable. Every build runs in the
Evaluation tier.

## What it is

Deterministic single-bus energy accounting, producing **planning estimates for human
review**. It is not an AC power-flow study, a protection or interconnection analysis,
a battery lifetime model, an optimiser, or a guarantee of savings. `docs/MODEL.md`
states the boundary in full, and the application repeats it on every screen and in
every artifact it writes.

It is local-first in the literal sense: it binds only `127.0.0.1`, serves only assets
compiled into the binary, makes no outbound connection of any kind, and stores your
data in one folder you can delete.

## Documentation

| | |
|---|---|
| `PRODUCT-SPEC.md` | The authoritative requirements and the finish line. |
| `AGENTS.md` | The engineering contract. |
| `docs/MODEL.md` | What the model does, what it refuses to do, and how it is validated. |
| `docs/ARCHITECTURE.md` | Two crates, why there is no HTTP framework, and the toolchain. |
| `docs/SECURITY.md` | The threat model and every defence, in the order a request meets them. |
| `docs/PRIVACY.md` | What is collected (nothing), where data lives, and how to remove it. |
| `docs/RELEASE-CHECKLIST.md` | Every gate, its command, and what is still blocking. |
| `TERMS.md` | Licence tiers and proposed pricing. Sales are not enabled. |
| `LICENSE.md` | The software licence. Proprietary; third-party components are not. |

## Building

Windows x64, with the llvm-mingw toolchain:

```powershell
$env:PATH='<cargo>\bin;<llvm-mingw>\bin;'+$env:PATH
cargo +stable-x86_64-pc-windows-gnullvm test  --workspace --all-targets --locked
cargo +stable-x86_64-pc-windows-gnullvm clippy --workspace --all-targets --locked -- -D warnings
```

## Packaging

```powershell
.\packaging\package.ps1          # build the release candidate, SBOM, notices, checksums, zip
.\packaging\verify-package.ps1   # 13 checks: file set, version metadata, identity, secrets, checksums
.\packaging\smoke.ps1            # 13 checks: run the packaged binary, prove its boundary, stop it
```

None of the three publishes, signs, uploads, installs, or creates a shortcut.
Publication remains a manual, human decision, and it should not be made until the
three gates above are closed.

Support identity: `support@flintglade.com`.
