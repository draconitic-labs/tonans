# Architecture

Tonans is three Rust crates:

```text
tonans-core     Deterministic model, project store, reports, and exports.
tonans-app      Loopback HTTP server, embedded UI, launch token, audited clock read,
                and the headless `tonans` command.
tonans-desktop  The application: a Tauri window showing that server's UI, in-process.
```

The split is a capability boundary. `tonans-core` owns every numeric dependency and
has no ambient clock, randomness, or network access. `tonans-app` owns the socket,
embedded interface assets, launch-time randomness, and the one injected wall-clock
value used for artifact metadata. `tonans-desktop` adds the window and the few things
only a native process can do. It reaches the model only through `tonans-app`.

## `tonans-core`

| Module | Responsibility |
|---|---|
| `schema` | Versioned project and separate storable/runnable validation. |
| `csvimport` | Strict interval import; ambiguity is refused rather than guessed. Mapping suggestions, on request, each with its reason. |
| `profile` | Sums and extremes of imported data, with no model involved. |
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
| `assets` | UI bytes embedded in the executable and scanned for unsafe DOM or remote-origin use. The page's rules are in `ui/logic.mjs`, with no page in them, and `ui-tests/` runs them under Node. |
| `token` | Per-launch 256-bit OS-CSPRNG token, never persisted, constant-time comparison. |
| `state` | Workspace, staged upload slot, and request-rate window. |
| `demo` | Bundled demonstration project installed through the real importer. |
| `clock` | The single audited wall-clock read, injected into core operations. |

### HTTP boundary

Tonans serves one window on loopback, so a small blocking server is easier to audit
than an asynchronous framework stack. The listener uses a fixed worker pool and a
bounded accept queue; a full queue receives a bounded busy response rather than
creating another thread. Request-line, header-count, physical header-line, and body
limits are enforced during parsing. An inactivity timeout is supplemented by one
absolute deadline spanning request-head and body reads.

The parser reads the request head first. The `Host`, `Origin`, control-route bearer-token,
and rate guards run in preflight **before the body is read**. Only an accepted
request proceeds to route-specific `Content-Length` validation and body allocation.
Chunked request bodies and keep-alive are deliberately unsupported, leaving one
framing rule and one request per connection.

A panicking handler does not take the application down. Each request runs inside
`catch_unwind`, the client gets a 500 with the code `http.internal`, and the worker goes on
to its next connection. The release profile therefore unwinds, and a test holds it to that.
A lock poisoned by such a panic is recovered, because what it guards stays valid.

Each request is answered on a thread that ends with it. A run of the largest allowed project
allocates about 2.4 GB, and glibc keeps what a thread frees for that thread, then hands it to
the next thread that starts. So memory is reused from one run to the next, rather than each of
the eight workers keeping its own copy. It stays at about 2 GB after such a run, and is not
returned to the system until Tonans quits.

Two windows on one workspace are two processes, so a project's revision is the SHA-256 of
its file. Every read returns it in `x-tonans-revision`. A save must send it, and gets 428
without it and 409 with an old one, so a stale window cannot overwrite data another window
imported. A run, a study, or an export that sends it is refused the same way. Checking and
writing happen under an advisory lock on `<workspace>/.tonans-write-lock`. The page tags each
of those requests with the project and revision it was made from, and drops an answer that
no longer matches what is on screen.

## `tonans-desktop`

The window loads the UI over HTTP from the server on a thread in the same process, so
there is one interface and one server and the shell cannot drift from the security
envelope. Three things in a browser do not carry over to a webview, so the shell provides
them as commands: saving a file (a native dialog), opening a link (the operating
system's handler), and quitting. The file picker for CSV import is the webview's own.

| Decision | Why |
|---|---|
| The token arrives through an initialization script | It never appears in a URL or history, and a reload still works. The script checks the origin first. |
| Commands are granted to this launch's exact origin at launch | No capability file exists, and no plugin command is granted. A page on any other origin has nothing to call. |
| The page names an artifact or a link, never a path or a URL | Paths come from the dialog the user answers; links come from a fixed list. |
| Navigation is confined to this launch's origin | Anything else is refused, and new windows are denied. |
| The webview's own small data folder is inside the workspace | One folder holds everything Tonans writes. |

On the NVIDIA driver, WebKitGTK's DMA-BUF renderer leaves the window blank under X11 and
runs slowly under Wayland, so the shell sets `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1` before
the webview starts, unless `WEBKIT_DISABLE_DMABUF_RENDERER` or
`WEBKIT_DMABUF_RENDERER_FORCE_SHM` is already set. Other drivers keep WebKit's defaults.
`WEBKIT_DISABLE_DMABUF_RENDERER=0` is the way to ask for WebKit's own behaviour.

`tauri.conf.json`'s `identifier` is the app id. The shell names the program after it
before GTK starts, which is where GTK takes the Wayland app id from. Tauri's
`enableGTKAppId` is left off, because it also registers on the session bus and that stops
a second window from opening. `packaging/linux/` holds the menu entry, which is named after
the same id so that the desktop can match a window to its launcher and icon. A test checks
that they agree.

The `tonans` command remains for headless runs and as the HTTP boundary the packaged smoke
test drives. It never opens a browser.

## Filesystem boundary

Project names are validated as names and never sanitized into paths. Workspace,
project, and export paths are resolved beneath their expected parent. Existing
files and directories are inspected without following links; Windows reparse points
(including junctions) and non-Windows symlinks are refused. The resolved direct
parent is rechecked at use time, so replacing a previously opened child directory
with a junction cannot redirect a later read or write. Writes stage to unique files
and become visible through an atomic replacement.

## Toolchain

The release package targets `x86_64-pc-windows-gnullvm` with llvm-mingw so it does not
depend on a Visual Studio installation. That package holds the `tonans` command only. CI builds
the window for Windows with the default MSVC target, and it has not been tried with this
toolchain. `chrono` is built without its clock feature,
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

The release target is a self-contained Windows x64 package of the `tonans` command whose
declared payload is sufficient on a clean supported host. It does not yet contain the
desktop window, which also needs the WebView2 runtime. The Linux path is
`scripts/install-desktop.sh`, which builds from source. The packaging gate must inspect executable
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
