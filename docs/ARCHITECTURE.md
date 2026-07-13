# Architecture

Two crates, one binary, thirty-seven dependencies.

```
tonans-core   The deterministic model. No clock, no randomness, no I/O beyond the
              project store. Everything a number depends on lives here.
tonans-app    The shell. The loopback HTTP server, the embedded UI, the launch
              token, the demonstration project, and the one audited clock read.
```

The split is not cosmetic. `tonans-core` is the thing whose output has to be
reproducible in ten years; `tonans-app` is the thing that has a socket. Keeping them
apart is what lets the first make a promise the second cannot break.

---

## `tonans-core`

| Module | What it holds |
|---|---|
| `schema` | The versioned project: site, data set, scenarios, assumptions. Every invariant that does not need the data, and every one that does, are separate checks (`validate` / `validate_runnable`) so a project can be *storable* before it is *runnable*. |
| `csvimport` | The strict importer. Nothing guessed, nothing repaired. |
| `timegrid` | The interval grid, timezone resolution, and DST ambiguity as a hard error. |
| `engine` | The interval loop: dispatch, limits, losses, outage replay. |
| `economics` | CAPEX, OPEX, degradation, NPV, payback. |
| `sensitivity` | Bounded grids of full simulations. Each cell is a real run. |
| `run` | Every scenario, compared against the baseline. |
| `report` / `svg` | The self-contained HTML report and the charts in it, rendered to SVG here rather than in a browser. |
| `export` | The bundle: report, JSON, CSV, provenance, checksums. |
| `provenance` | What produced these numbers, from what, under which limits. |
| `canonical` | Sorted-key JSON and SHA-256, so a hash means something. |
| `paths` | Names are validated and refused; they are never sanitized and never paths. |
| `limits` | Every hard bound, in one file, published in every artifact. |
| `licensing` | The entitlement seam. One narrow trait. No key, no server, no network. |
| `units` | Every quantity carries its unit, enforced by a test over the serialized result tree. |

### Determinism

No clock. No randomness. `clippy.toml` bans `SystemTime::now` and `Instant::now`
across the whole workspace; the single audited read lives in
`crates/tonans-app/src/clock.rs` and is *injected* into the core. `powf` is avoided
in favour of iterated multiplication, because libm is not required to be
bit-identical across platforms. Nothing iterates a hash map.

The consequence is a real guarantee, not an aspiration: same inputs, same bytes,
any machine, forever. `run::RunResult::model_input_hash` lets a reader prove it.

## `tonans-app`

| Module | What it holds |
|---|---|
| `http` | The whole HTTP surface: request parsing, a bounded thread pool, a response writer. About three hundred lines of `std::net`, no dependencies. |
| `server` | Routing and the security envelope: loopback bind, `Host` pinning, `Origin` check, rate limit, token gate, CSP and friends. |
| `api` | The control routes. Every one takes a project *name*, never a path. |
| `assets` | The UI, compiled into the binary. Tests fail the build if it references a remote origin or touches `innerHTML`. |
| `token` | The launch token: 256 bits from the OS CSPRNG, never written to disk, compared in constant time. |
| `state` | The workspace, the entitlements, the one staged upload slot, the rate window. |
| `demo` | The bundled demonstration project, installed on first launch *through the real importer*. |
| `clock` | The one wall-clock read in the workspace. |

### Why there is no HTTP framework

Axum brings Tokio, which brings Mio, which brings `windows-sys` — around sixty
crates into the dependency tree of a binary whose entire pitch is that you can audit
it. All of that machinery exists to serve tens of thousands of concurrent
connections. Tonans serves **one user, on loopback, at human speed**: a few dozen
requests per interaction, from a single browser tab, on a socket no other machine
can reach.

None of that concurrency is load-bearing here, and every crate in the tree is
something a security review has to account for. So `http.rs` is the whole surface,
it has no dependencies, and it is small enough to read in one sitting. That is the
point.

The same argument retires the argument-parser: six flags do not need a derive macro
and its half-dozen crates.

## The toolchain

Tonans is built for `x86_64-pc-windows-gnullvm` with the llvm-mingw toolchain. Not
MSVC — the target is chosen so the build does not depend on a Visual Studio
installation.

`getrandom` is pinned to **0.2** deliberately. Version 0.3 reaches Windows' CSPRNG
through a `raw-dylib` import, which the `windows-gnu` toolchain cannot build without
a full MinGW binutils installation. 0.2 uses an ordinary import library, which the
toolchain already ships. This pin is a compatibility constraint, not an oversight;
`cargo audit` is clean on it, and if that ever changes the pin has to be revisited
rather than ignored.

The Windows version resource is generated by `crates/tonans-app/build.rs` and
compiled with `llvm-windres`, which the toolchain provides — rather than by a
build-dependency crate, because a build-dependency is still a dependency and this
one would be forty lines of text.

### The release build remaps its paths

Rust embeds dependency source paths in panic-location metadata, and those paths sit
under the build machine's home directory. **`strip = "debuginfo"` does not remove
them**, so an un-remapped release binary ships the packager's Windows username inside
it. `packaging/package.ps1` passes `--remap-path-prefix` for `CARGO_HOME`, the repo
root, and the user profile; `packaging/verify-package.ps1` greps the built artifact
and fails the release if any build-machine path survived.

The fix and the check are deliberately in different scripts. A check that trusts the
thing it is checking is not a check.

## The dependency tree

```
serde, serde_json    the versioned schema, exports, provenance
csv                  RFC 4180 tokenizing (the strictness is ours, not the tokenizer's)
chrono, chrono-tz    interval arithmetic and the IANA database, to catch DST ambiguity
sha2                 input, project, and artifact hashing
thiserror            the machine-coded error taxonomy
getrandom            the OS CSPRNG, for the launch token — the only randomness anywhere
```

`chrono` is built **without** the `clock` feature. `tonans-core` must contain no
wall-clock access, and the cleanest way to guarantee that is for the capability not
to be compiled in.

`unsafe_code` is **forbidden** workspace-wide.

## Packaging

```
packaging/package.ps1          build the release candidate, stage an exact file set,
                               write the SBOM, notices, and checksums, zip it
packaging/verify-package.ps1   re-derive every claim the package makes; fail on a
                               missing or extra file, wrong version metadata,
                               personal-identity leakage, a test secret or live
                               origin, or a checksum mismatch
packaging/smoke.ps1            run the packaged binary against an isolated workspace,
                               prove the loopback and token boundary, stop it cleanly
```

None of them publishes, signs, uploads, installs, or creates a shortcut.
