# Release checklist - Tonans

The release gates for Tonans and their recorded results.

## Version 0.1.0, 2026-11-08

The owner decided on 2026-11-08 to release 0.1.0 that night. **The walkthrough with real
input has not been done on Windows.** It passed on Linux on 2026-11-04, against an earlier
source revision; the record is under "Walkthrough" below.

The release files are built by `.github/workflows/release.yml`. A tag run builds them again
from the tag and lists their digests in the release's `SHA256SUMS`, so no package digest is
recorded here: the files checked below are branch builds of the same source, not the
published bytes.

Gates run on the `release-0.1.0` branch at `2a32cd7`. Later commits change only documents and
remove a temporary trigger from the release workflow.

- **CI** (`ci.yml`, run 42540019421): formatting, strict Clippy over the workspace, the
  workspace tests on Ubuntu 24.04 including the desktop shell's, the tests without the shell
  on Windows (MSVC), the page's Node tests, a release build, `tonans --help`, and
  `tonans-desktop --version`: passed on both systems.
- **Dependency audit** (`security.yml`, `cargo audit --deny warnings` with the five
  advisories below): passed, run 42540025354.
- **CodeQL** (Rust and JavaScript): passed, run 42540025324.
- **Release workflow, Windows job** (run 42540019408): `package.ps1` built
  `tonans-0.1.0-windows-x64.zip` with rustc 1.99.1 on `stable-x86_64-pc-windows-gnullvm`,
  the toolchain the script names, and llvm-mingw 20261020 (UCRT). It ships 26 third-party
  crates and embeds 27 distinct licence texts. `verify-package.ps1` passed 15 of 15 checks
  and `smoke.ps1` 12 of 12. Two checks in `verify-package.ps1` failed falsely on its first
  run and were fixed: the mail-domain check read `@draconitic.comtext` from the executable's
  string table as a foreign domain, and the NOTICES.md origin check compared only scheme and
  host, so its exception for Barlow's copyright address could never apply.
- **Release workflow, Linux job** (run 42540019408), on Ubuntu 22.04: cargo-about 0.9.2
  wrote NOTICES.md for `tonans` and for `tonans-desktop`, each with the Barlow font licence,
  and Tauri CLI 2.12.4 built `Tonans_0.1.0_amd64.deb` and `Tonans-0.1.0-1.x86_64.rpm`. Each
  package was unpacked outside the checkout and its binary run with an empty environment:
  - from the tarball, `tonans --version`, `--help`, and `tonans run` of the demo project.
    The bundle passed `sha256sum -c`, and its model input hash `97029af07baaf314...` and
    results hash `b9512228beb1b6a1...` are the ones in the walkthrough record below;
  - `tonans-desktop --version` and `--help` from the .deb, and `--version` from the .rpm;
  - neither binary contains a `/home/runner` path, the menu entry carries
    `StartupWMClass=com.draconiticlabs.tonans`, and LICENSE and NOTICES.md are in
    `/usr/share/doc/tonans/`;
  - the .deb then installed with apt, and the installed `tonans-desktop --version` ran.
- **Local, Fedora 44**: `cargo test -p tonans-core -p tonans-app --all-targets --locked`
  passed, 313 tests. These crates are unchanged since that run.

Not run for this release:

- the walkthrough on Windows, as above, and of this exact source on Linux;
- the CycloneDX JSON Schema validation recorded for 2026-07-13.

### Earlier record, 2026-07-13

These results belong to the release-candidate archive built before Tonans became open
source.

- archive SHA-256: `da7809d611a517fa0081ea1ac42f50d5736d11fddc58a81d7b872bb3fdf8a3d8`;
- runtime: `Tonans 0.1.0 (model 1.1.1, schema v1)`;
- formatting, strict Clippy, 212 tests, and locked RustSec audit passed;
- closed-package verifier passed 15/15 checks;
- sanitized packaged-binary smoke passed 12/12 checks;
- CycloneDX 1.5 JSON Schema validation passed with the official SPDX and JSF schemas;
- Grok 4.5 maximum reasoning and an independent fresh-context Codex review both
  returned GO for the exact reviewed source and archive digest; and
- Authenticode status is intentionally `NotSigned` for this release candidate.

Use the GNU/LLVM Rust toolchain `1.96.1-x86_64-pc-windows-gnullvm`, the version
`rust-toolchain.toml` pins, and place Cargo before llvm-mingw:

```powershell
$env:PATH="$env:USERPROFILE\.cargo\bin;<llvm-mingw>\bin;$env:PATH"
```

## Walkthrough

Launch the final build in its native window and drive the actual interface with real
input, never a browser:

```text
import -> configure -> run -> compare -> sensitivity -> export -> quit -> relaunch -> reopen
```

Automated tests and the packaged-binary smoke exercise the HTTP boundary and model path,
but they do not drive the interface. Confirm visible units, warnings, source-flow metrics,
assumption text, sensitivity results, saved exports, and project reopening. Record the
tested package digest when this passes.

Passed 2026-11-04 on Linux in the Tauri window (WebKitGTK 2.54), against source revision
`cdc8bd7` and a release binary with SHA-256 `b8fb5b480ffac3ac...`. It was run on a
virtual X display, with no session bus and no window manager, and driven by an AI agent using
`xdotool` keystrokes and clicks and checking screenshots, at 1600x950 and 1280x720, in light
and dark. Every pass started from a clean launch, with the display checked to hold no other
program's window. The second window below was opened on purpose.

Walked again on the same setup, at 1600x950, at source revision `488ffd9` with a release binary
of SHA-256 `fdf9bdc23f4b14ef...`, once the commentary was out of the window: the empty state, every
screen from a clean launch, each dispatch policy and add-scenario template, a study on an energy
axis and one on a price axis, which showed no best-cell panel, and an export. A file with `P_Load_kW`,
`Gen_MW`, `Export Credit per kWh` holding 0 overnight and 8.5 later, `Retail Price (c/kWh)`,
`OA Temp (°F)`, and `Critical Power (kW)` got all its columns suggested, with a Check on the three
that need one, and its export credit tile read 0.000 to 0.085 USD/kWh. `Feed-in Rate per kWh` holding
1.2 to 1.8 got its role and no unit. A project file put back while the window kept focus saved with
no dialog. After Reopen failed on a corrupted file, moving the file away and pressing Save wrote the
edits. The bundled demo's model input and results hashes did not move.

- **Import:** `samples/riverside-works-2024.csv` through the native file picker. The
  explicit suggestion mapped 8 of 8 columns, flagged two prices whose currency the file does
  not state, and held the import until the review box was ticked. Importing with nothing
  mapped was refused. A wrong interval was refused on screen, and the same file imported
  once corrected, without choosing it again. The sample-data path marked the project as
  synthetic on screen and in the warning ledger, and the CSV template was saved through the
  native dialog.
- **File intake:** a 3 GiB sparse file and a 70 MB file were refused from their size, giving the
  64 MiB limit, with memory flat. Files named with an en dash, CJK, and an umlaut were accepted.
  A file with `Non_Critical_Load_kW`, `Load_kWh/h`, `Price (A$/kWh)`, `Solar_Irradiance_W/m2`,
  `Battery_Temp_C`, and `Price_BRL_per_kWh` got no suggestion for any of them, with a note saying
  why for each, and its plain `Load (kW)` and `PV Power (kW)` columns were still recognized. A
  103-character file name was accepted. Text typed while an import ran stayed on screen as unsaved,
  and the Import button was disabled until it finished.
- **Configure:** scenarios added from the battery-sizes, peak-shaving, backup-first, surplus,
  and blank-copy templates; imported prices applied to every scenario; tariff presets, including
  Flat energy price clearing a demand charge; a derating table edited, and a manual schedule
  window added and filled in; a replacement edited; an outage window edited through the date
  popup, labelled, and copied to every scenario; duplicate, remove, change of baseline, undo, and
  redo, by button and by key, with undo restoring the selected scenario. A blank number and `1e`
  in the export limit were refused rather than read as 0 or unlimited, and an empty outage window
  was refused with the edits kept. Switching project over unsaved edits asked first, a save that
  failed in that dialog said why and stayed put, and discarding left the project on disk unchanged.
- **Run and compare:** five scenarios, 8,760 intervals, and a 105,120-interval file with four
  scenarios. Year 0 of the cash flow table shows installed cost in its own column, and the annual
  tile says when it is scaled from a short file. The same year starting on January 1, starting on
  July 15, and stamped in UTC for a New York site raised nothing, then billed 13 months and said so,
  on screen, in the report, and in `provenance.json` and `summary.json`. A battery with no capital
  cost read "No capital to repay" in the comparison, the heat table, and the report. The bundled demo, run through `tonans run`, gave
  model input `97029af07baaf314...` and results `b9512228beb1b6a1...` at model 1.2.0. A build of
  `main` at 1.1.1 gives `4e25a00008c5ccdc...` and `65b99cea6ef175ec...`, and this source with its
  model version set back to 1.1.1 gives exactly those. No number in `summary.json` differs from
  `main`, and the timeseries and scenario CSVs are byte-identical. `summary.json` and the report
  also differ in the fade wording, and the report in its cash flow table's new column.
- **Sensitivity:** a 9-cell grid and a 12-cell grid with installed cost moving at 550 USD per
  kWh, with the best cell marked as the best of those tested. An unreadable axis value was
  refused, and a cell was added as a scenario and undone. With the form already open, changing
  installed cost from 220,000 to 440,000 USD moved the rate to 1,100, and the study's cell at
  400 kWh / 150 kW read -448,355 USD, as Results did. Import-price and demand-price axes gave
  the values in `CHANGELOG.md`, stated 0.8 exactly, and offered no cell as a scenario. Removing the
  study's scenario left the form working, on another scenario.
- **Late answers:** on the 105,120-interval file, a run, a 256-cell study, and an export each
  finished after opening another project, and after saving a change. Each was dropped with a
  message, or for an export, said where the bundle went, and nothing was marked Current.
- **One heavy job:** Run on that file, open another project, Run: the second was told what was
  running, and its button came back when that finished. The application peaked at 2.7 GB and read
  under 700 MB after runs and 2.4 GB after an export. Over HTTP under a 6 GiB address-space limit,
  three overlapping runs answered 200, 409, 409, and the process survived.
- **When the file changes underneath:** a Reopen that could not read a corrupted file kept the edits,
  still unsaved, with Save enabled, and the reason stayed on screen. Further typing and a Save after
  restoring the file put all of it on disk. A project whose file was moved away said so, and Save
  wrote it back. A save that had not yet been told got a dialog and wrote nothing, and leaving asked
  first. A read-only project file refused the save with a clear message, and it worked once writable.
- **Two windows on one workspace:** the first window said its project had changed elsewhere when it
  got focus. Saving from it was refused with a dialog, the other window's import stayed on disk,
  Enter kept the edits on screen, and reopening showed the import. Results overtaken this way read
  Out of date.
- **Export:** a bundle of 11 files. One file was saved through the native save dialog and
  the whole bundle through the native folder dialog. All 10 listed files passed
  `sha256sum -c` on `SHA256SUMS.txt`, and `report.html` holds no script. With a study and unsaved
  edits, the page and a dialog said saving would leave the study out, and the bundle did. Saving over
  a 0600 file kept its mode, and saving over a link wrote through it and left it a link.
- **Quit and reopen:** Quit, and a window-close message, both asked first over unsaved edits. Enter and
  Escape kept the work from both, and only a click on Quit without saving discarded it. After quitting
  and relaunching, the reopened project gave identical hashes: results `67fa5611b21f4c3f...`, model
  input `0f4be9472fda11cf...`, project `6eacdf850cfb6795...`.

Not covered by this record:

- **Windows was not walked through.** No Windows build has been run, and no package digest
  applies to this source build. What Enter and Escape do in the quit dialog there comes from
  reading the dialog library.
- Wayland and a real compositor were not exercised: the frames went to a virtual display, and
  that hides renderer problems. The WebKit settings for the NVIDIA driver are unit-tested only.
- With no session bus, Show in file manager took its fallback of opening the folder, so the
  desktop's file-manager service was not exercised. Links reached a stand-in for `xdg-open`,
  so no real handler ran.
- `scripts/install-desktop.sh` has not been run. Its file-recognition functions were
  exercised against a scratch home folder.
- HiDPI, assistive technology, and keyboard-only use were not walked through.

The Windows walkthrough is still open. 0.1.0 was released without it by the owner's
decision of 2026-11-08.

## Final integrated gate rerun

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
cargo +1.96.1-x86_64-pc-windows-gnullvm test `
  -p tonans-core --test ssc_capacity_reference --locked
```

This closes the specified comparison gate only within that documented scope. It is a
**narrow normalized SSC battery-state reference replay**, not SAM certification,
maintainer endorsement, or evidence of full SAM/SSC parity.

## Reproducible release gates

### Format, lint, and tests

```powershell
cargo +1.96.1-x86_64-pc-windows-gnullvm fmt --all -- --check
cargo +1.96.1-x86_64-pc-windows-gnullvm clippy `
  -p tonans-core -p tonans-app --all-targets --locked -- -D warnings
cargo +1.96.1-x86_64-pc-windows-gnullvm test `
  -p tonans-core -p tonans-app --all-targets --locked
```

The package holds the `tonans` command, so these name its crates. The desktop shell is
gated separately in CI. Formatting and Clippy run with `--workspace` on Ubuntu and on the
default Windows target. Tests run with `--workspace` on Ubuntu. On Windows they run with
`--exclude tonans-desktop`, because the shell's test binary has no application manifest and
cannot load there; its tests are platform-neutral, and the release build still covers the
binary.

The page's own rules are tested under Node, with nothing else installed:

```sh
node --test crates/tonans-app/ui-tests/logic.test.mjs
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
cargo audit --deny warnings
```

Audit the locked tree. Any advisory is dispositioned explicitly; compatibility pins
are not blanket exceptions. CI uses the same command, so a new warning fails the job.
`.cargo/audit.toml` holds the list below, and nothing else may be added to it without a row
here. Each of these comes from a stack the Tauri version pins, so check them again whenever
Tauri moves.

| Advisory | Crate | What it is | Why it is accepted for now |
|---|---|---|---|
| RUSTSEC-2024-0429 | glib 0.18.5 | Unsound `VariantStrIter` | Fixed in glib 0.20, but gtk 0.18 under Tauri holds it at 0.18. Tonans calls only `glib::set_prgname`, and no other crate in the locked tree names `VariantStrIter` or `array_iter_str` (searched 2026-11-04). |
| RUSTSEC-2024-0370 | proc-macro-error 1.0.4 | Unmaintained | Used only at compile time through glib-macros and gtk3-macros. Not in the binary. |
| RUSTSEC-2026-0370 | zvariant 5.15.1 | Unmaintained, merged into zbus 6 | No patched release. Linux only, through zbus 5 under tauri-plugin-opener, for the one call asking the local file manager to show a folder. |
| RUSTSEC-2026-0371 | zbus_names 4.3.5 | Unmaintained, merged into zbus 6 | As above. |
| RUSTSEC-2026-0372 | zvariant_derive 5.15.1 | Unmaintained, merged into zbus 6 | As above, at compile time. |

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
- include no payment integration or secret.

### Independent package verification

```powershell
.\packaging\verify-package.ps1
```

The verifier must independently check the exact allowlist, Windows version metadata,
Draconitic Labs-only public identity, absence of secrets and build paths, localhost-only
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
- package contents, PE imports, SBOM/notices/checksums, and sanitized smoke evidence;
- public claims for parity and determinism.

Record findings and regressions. A review of code before a subsequent repair is not
the final review.

## Signing status

Release 0.1.0 is unsigned, on every platform. Windows SmartScreen may warn that the
publisher cannot be confirmed. The archive sidecar and internal checksums provide
integrity evidence, not publisher authentication.

## Final release sequence

1. Merge the release branch, and run the release workflow by hand on `main` if anything
   changed since its last green run.
2. Tag the merge commit `v<version>` and push the tag. The workflow builds every package
   again, repeats the checks above, and creates a draft release with the files and
   `SHA256SUMS`.
3. Download the draft's files, check them against `SHA256SUMS`, and run them.
4. Complete the walkthrough using those files, or record the decision to release without it.
5. Publish the draft.

Support: `support@draconitic.com`
