# Architecture

Tonans is two Rust crates compiled into one local Windows application:

```text
tonans-core   Deterministic model, project store, reports, and exports.
tonans-app    Loopback HTTP shell, embedded UI, launch token, and audited clock read.
```

The split is a capability boundary. `tonans-core` owns every numeric dependency and
has no ambient clock, randomness, or network access. `tonans-app` owns the socket,
embedded browser assets, launch-time randomness, and the one injected wall-clock
value used for artifact metadata.

## `tonans-core`

| Module | Responsibility |
|---|---|
| `schema` | Versioned project and separate storable/runnable validation. |
| `csvimport` | Strict interval import; ambiguity is refused rather than guessed. |
| `timegrid` | Exact interval grid, timezone resolution, and DST refusal. |
| `engine` | Fixed-order interval dispatch, limits, losses, source attribution, and outage replay. |
| `economics` | CAPEX, OPEX, fade approximation, NPV, and payback. |
| `sensitivity` | Bounded grids of complete model runs. |
| `run` | Baseline and candidate orchestration. |
| `report` / `svg` | Self-contained HTML report and local SVG charts. |
| `export` / `provenance` | Atomic bundle creation, artifact hashes, assumptions, and warnings. |
| `canonical` | Sorted-key canonical JSON and SHA-256 model/result identities. |
| `paths` / `store` | Name validation, confinement, reparse-point refusal, and atomic writes. |
| `limits` | The compiled safety envelope, in one source of truth. |
| `licensing` | Narrow local entitlement seam; no key server or network client. |
| `units` | Serialized numeric fields mapped to explicit units. |

### Determinism contract

No clock or randomness exists in the core, observable ordering is explicit, and
negative zero is normalized before canonical hashing. `powf` is avoided in the
long-horizon calculation so platform libm differences are not introduced there.

The supported claim is intentionally narrower than byte identity across every
machine forever: for the same canonical model inputs, `MODEL_VERSION`, and supported
build target, the numeric result produces the same `results_hash`. Presentation
metadata and nonnumeric prose are outside that numeric identity. `MODEL_VERSION`
changes whenever an unchanged input could produce a different number.

## `tonans-app`

| Module | Responsibility |
|---|---|
| `http` | HTTP/1.1 parsing, fixed worker pool, bounded accept queue, deadlines, and response writing. |
| `server` | Routing and request guards: loopback/Host, Origin, rate, bearer token, and response headers. |
| `api` | Control routes that accept project names, never arbitrary paths. |
| `assets` | UI bytes embedded in the executable and scanned for unsafe DOM or remote-origin use. |
| `token` | Per-launch 256-bit OS-CSPRNG token, never persisted, constant-time comparison. |
| `state` | Workspace, entitlements, staged upload slot, and request-rate window. |
| `demo` | Bundled demonstration project installed through the real importer. |
| `clock` | The single audited wall-clock read, injected into core operations. |

### HTTP boundary

Tonans serves one browser on loopback, so a small blocking server is easier to audit
than an asynchronous framework stack. The listener uses a fixed worker pool and a
bounded accept queue; a full queue receives a bounded busy response rather than
creating another thread. Request-line, header-count, physical header-line, and body
limits are enforced during parsing. An inactivity timeout is supplemented by one
absolute deadline spanning request-head and body reads.

The parser reads the request head first. `Host`, `Origin`, rate, and control-route
bearer-token guards run in preflight **before the body is read**. Only an accepted
request proceeds to route-specific `Content-Length` validation and body allocation.
Chunked request bodies and keep-alive are deliberately unsupported, leaving one
framing rule and one request per connection.

## Filesystem boundary

Project names are validated as names and never sanitized into paths. Workspace,
project, and export paths are resolved beneath their expected parent. Existing
files and directories are inspected without following links; Windows reparse points
(including junctions) and non-Windows symlinks are refused. The resolved direct
parent is rechecked at use time, so replacing a previously opened child directory
with a junction cannot redirect a later read or write. Writes stage to unique files
and become visible through an atomic replacement.

## Toolchain

Tonans targets `x86_64-pc-windows-gnullvm` with llvm-mingw so the release does not
depend on a Visual Studio installation. `chrono` is built without its clock feature,
and `unsafe_code` is forbidden workspace-wide.

`getrandom` remains pinned to 0.2 because that version reaches the Windows CSPRNG
through an import supported by the selected GNU/LLVM toolchain. The pin remains
subject to the locked dependency audit; it is a compatibility decision, not an
exception to the security gate.

The Windows version resource is generated locally by the application build and
compiled with `llvm-windres`.

### Build-path privacy

Rust can embed dependency and panic-location paths even when debuginfo is stripped.
The release build remaps `CARGO_HOME`, repository, and user-profile prefixes. The
independent package verifier scans the artifact and fails if a build-machine path or
personal identity survives.

## Release-package goal

The release target is a self-contained Windows x64 package whose declared payload
is sufficient on a clean supported host. The packaging gate must inspect executable
imports and either prove that only supported Windows system libraries are required
or include, license, hash, and inventory every runtime file. A smoke test that
inherits a compiler-toolchain directory on `PATH` is not evidence of that goal.

```text
packaging/package.ps1          builds and stages the exact payload, SBOM, notices,
                               checksums, and archive
packaging/verify-package.ps1   independently checks payload, metadata, identity,
                               imports, SBOM shape, and hashes
packaging/smoke.ps1            runs the staged package in an isolated workspace on
                               a sanitized runtime environment
```

These scripts do not publish, upload, install, or create a shortcut.
