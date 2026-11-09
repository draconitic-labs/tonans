# Tonans

Tonans is Draconitic Labs' local-first microgrid scenario workbench. It compares
transparent battery, tariff, renewable-generation, and outage-planning scenarios
without controlling equipment or presenting planning estimates as certified designs.

## Status

Version 0.1.0 is the first binary release. Tonans is open-source software under the
[Apache License 2.0](LICENSE).

The release files were built and checked by `.github/workflows/release.yml`.
`docs/RELEASE-CHECKLIST.md` records the gates that ran for this release and their
results.

The required public-reference comparison is closed by `docs/BENCHMARK.md` and the
executable SSC regression. Its scope is deliberately narrow: a normalized SSC
battery-state reference replay pinned to SSC commit
`86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832`. It is not certification, endorsement,
or a claim of SAM/SSC parity.

The walkthrough (import -> configure -> run -> compare -> sensitivity -> export ->
quit -> relaunch -> reopen) passed on 2026-11-04 in the native window on Linux with real
input. It has not been done on Windows: the Windows package was released without it, by
the owner's decision. `docs/RELEASE-CHECKLIST.md` records both.

## Install

Download the files for your system from the
[v0.1.0 release](https://github.com/draconitic-labs/tonans/releases/tag/v0.1.0) on the
[Releases page](https://github.com/draconitic-labs/tonans/releases). None of them is
signed. `SHA256SUMS` lists the SHA-256 of every file; check a download with:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

| File | What it is |
|---|---|
| `Tonans_0.1.0_amd64.deb` | The desktop window, for Ubuntu 22.04 or later and Debian 12 or later, on x86_64 |
| `Tonans-0.1.0-1.x86_64.rpm` | The desktop window, for Fedora on x86_64 |
| `tonans-0.1.0-linux-x86_64.tar.gz` | The `tonans` command, without a window, for x86_64 Linux with glibc 2.34 or later |
| `tonans-0.1.0-windows-x64.zip` | The `tonans` command, without a window, for 64-bit Windows |

**Debian and Ubuntu.** Install the package, which also installs WebKitGTK if it is
missing:

```sh
sudo apt install ./Tonans_0.1.0_amd64.deb
```

Tonans is then in your applications menu, and `tonans-desktop` starts it from a terminal.
`sudo apt remove tonans` removes it.

**Fedora.**

```sh
sudo dnf install ./Tonans-0.1.0-1.x86_64.rpm
```

Start it from the applications menu or with `tonans-desktop`. `sudo dnf remove tonans`
removes it.

**The `tonans` command on Linux.** Extract the tarball anywhere and run the binary in
it. Nothing needs installing:

```sh
tar -xzf tonans-0.1.0-linux-x86_64.tar.gz
./tonans-0.1.0-linux-x86_64/tonans --help
./tonans-0.1.0-linux-x86_64/tonans run --project "Demo - Riverside Works"
```

**Windows.** The ZIP holds `Tonans.exe`, the `tonans` command; the desktop window is not
in it. Unblock the ZIP (right-click, **Properties**, **Unblock**), extract it to a folder
you own, and run `Tonans.exe` from a command prompt. Windows SmartScreen warns about an
unsigned program; the `README.md` in the ZIP explains that and how to check the files
against `SHA256SUMS.txt` before you run it.

Uninstalling never touches your projects. They stay in the workspace folder, described in
`docs/PRIVACY.md`, until you delete it.

## What it is

Tonans performs deterministic single-bus energy accounting and produces **planning
estimates for human review**. It is not an AC power-flow study, a protection or
interconnection analysis, a battery lifetime model, an optimiser, or a guarantee of
savings. `docs/MODEL.md` states the boundary in full, and Help and every export
repeat it.

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
| `docs/ARCHITECTURE.md` | Crate boundaries, the desktop shell, HTTP design, toolchain, and packaging goal. |
| `docs/SECURITY.md` | Threat model and defenses in request and filesystem order. |
| `docs/PRIVACY.md` | What is collected, where data lives, and how to remove it. |
| `docs/RELEASE-CHECKLIST.md` | Reproducible release gates and their recorded results. |
| `CHANGELOG.md` | Changes that affect what Tonans tells you, including results that were wrong. |
| `LICENSE` | Apache License 2.0; third-party components remain under their own licences. |

## Running it

Tonans is a native window: `tonans-desktop` runs the loopback server in-process and
shows the interface in a Tauri webview. Closing the window ends the application.

To build and install it from this repository on Linux instead:

```sh
scripts/install-desktop.sh              # builds a release, then installs under ~/.local
scripts/install-desktop.sh --uninstall  # removes only the files it recognises as its own
```

This needs Rust and the WebKitGTK and GTK development packages. The script names them
if they are missing. Your projects are never touched by installing or uninstalling.

To run it without installing:

```sh
cargo run --release --locked -p tonans-desktop
```

The `tonans` command is Tonans without a window. `tonans run --project <NAME>` runs a
project and writes the export bundle, for scripts. `tonans` alone starts the loopback
server. It never opens a browser, and it prints the link with its one-launch token only
to a terminal; `--print-link` asks for it when output is being captured, and is what
the packaged smoke test uses.

### Memory

Memory needed grows with intervals times scenarios. The largest project Tonans allows, 420,480
rows such as four scenarios of a five-minute year, needs **about 2.6 GB of free memory for a run or
an export**, on top of the window itself, and works under an address-space limit of 3.5 GiB and not
under 3 GiB. An hourly year with three scenarios needs far less: about 160 MB.

Each window does one such job at a time, and a second is told what is already running. If the
computer cannot supply the memory anyway, **Tonans stops at once and that window's unsaved edits
are lost**. An allocation failure cannot be caught, so save before running the largest projects on
a machine that is short of memory. Files already saved stay intact.

## Building

On Linux, with the pinned toolchain and the packages above:

```sh
cargo test --workspace --all-targets --locked
```

CI builds the whole workspace, including the window, for Windows with the default MSVC
Rust target. The window has not been run on Windows. The release package uses the
llvm-mingw toolchain and builds the `tonans` command only:

```powershell
$env:PATH="$env:USERPROFILE\.cargo\bin;<llvm-mingw>\bin;$env:PATH"
cargo +stable-x86_64-pc-windows-gnullvm test -p tonans-core -p tonans-app --all-targets --locked
cargo +stable-x86_64-pc-windows-gnullvm clippy -p tonans-core -p tonans-app --all-targets --locked -- -D warnings
```

## Packaging

```powershell
.\packaging\package.ps1
.\packaging\verify-package.ps1
.\packaging\smoke.ps1
```

These scripts build, check, and run the Windows package of the `tonans` command. They do
not publish, upload, install, or create a shortcut. The Windows package does not contain
the desktop window.

The release workflow, `.github/workflows/release.yml`, runs those three scripts on
Windows. On Ubuntu 22.04 it builds the desktop `.deb` and `.rpm` with the Tauri bundler
and the `tonans` tarball, writes each package's third-party notices with cargo-about from
`packaging/about.toml`, and runs every binary from its extracted package. A tag run drafts
a release with all of them and `SHA256SUMS`.

The interface's typeface is Barlow, under the SIL Open Font License, compiled into the
binary from `crates/tonans-app/ui/fonts`.

Support: `support@draconitic.com`.
