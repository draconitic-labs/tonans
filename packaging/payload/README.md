# Tonans — Microgrid Scenario Workbench

**Version 0.1.0 · Windows x64 · Release candidate · Unsigned · Apache-2.0**

Tonans compares battery-storage and microgrid planning scenarios on your own
interval data, on your own machine. It performs deterministic single-bus energy
accounting and shows its work: every assumption, every unit, every warning, and
the hashes needed to reproduce the numbers.

---

## What this is, and what it is not

**Planning estimate for human review.** Tonans is not an AC power-flow study, not
a protection or interconnection analysis, not a safety certification, and not a
guarantee of savings. It does not size conductors, coordinate protection, compute
short-circuit duty, dispatch equipment, or talk to a controller — and it never
will. Results must be reviewed by a qualified engineer before any procurement or
design decision.

It is deliberately not an optimiser. Dispatch policies are named rules evaluated
interval by interval, with no view of the future. Tonans will not claim an optimum
it did not compute.

The full list of what the model excludes is in `MODEL.md`, and the application
repeats it on screen and in every report it writes.

## Installing

There is no installer. Tonans is a single executable that writes nothing to the
registry and nothing outside its own workspace folder.

1. Unblock the ZIP before extracting it. Windows marks files downloaded from the
   internet, and the mark is inherited by everything inside: right-click the ZIP →
   **Properties** → tick **Unblock** → **OK**.
2. Extract it somewhere you own — `C:\Tools\Tonans` or a folder on your desktop.
   Do not extract into `C:\Program Files`; Tonans is unsigned and does not need
   administrator rights, and it should not be given any.
3. Verify the download before you run it (see **Verifying** below).
4. Double-click `Tonans.exe`.

**This binary is unsigned.** Windows SmartScreen will warn you, and it is right to:
an unsigned executable is one whose publisher Windows cannot confirm. If you did
not obtain this file directly from Flintglade, do not run it. If you did, choose
**More info → Run anyway**. Verifying the checksum first is the honest way to
satisfy yourself the file is the one that was published.

On first launch Tonans installs a bundled demonstration project — Riverside Works —
by importing a sample CSV through the real importer. Nothing is smuggled past
validation.

## Verifying

In PowerShell, from the folder holding the ZIP:

```powershell
Get-FileHash .\tonans-0.1.0-windows-x64.zip -Algorithm SHA256
```

Compare the result with the `.sha256` file published alongside the ZIP. After
extracting, `SHA256SUMS.txt` covers every file in the package, including the
executable:

```powershell
Get-Content .\SHA256SUMS.txt | ForEach-Object {
  $digest, $name = $_ -split '\s\s', 2
  $actual = (Get-FileHash $name -Algorithm SHA256).Hash.ToLower()
  "{0}  {1}" -f $(if ($actual -eq $digest) { 'ok  ' } else { 'BAD ' }), $name
}
```

`SBOM.cdx.json` is a CycloneDX bill of materials for everything linked into the
binary. `NOTICES.md` carries the third-party licences.

## Running it

Double-clicking `Tonans.exe` starts a local server and opens your browser at it.

- Tonans binds **only to `127.0.0.1`**, on a port the operating system picks. No
  other machine can reach it.
- Each launch mints a **launch-scoped bearer token**, printed in the URL. Without that token
  the control API refuses every request — because a web page you happen to have
  open in another tab can also reach `127.0.0.1`, and the token is what it cannot
  guess.
- **Nothing is sent anywhere.** No telemetry, no analytics, no update check, no
  fonts, no remote scripts. The interface is compiled into the executable.

From a command prompt:

```
Tonans.exe                                   Launch (opens a browser)
Tonans.exe --no-browser                      Launch and just print the URL
Tonans.exe --workspace D:\Work\Tonans        Keep projects somewhere else
Tonans.exe run --project "Demo - Riverside Works"   Run headlessly, write the export bundle
Tonans.exe --help
```

## Where your data lives

By default:

```
%LOCALAPPDATA%\Flintglade\Tonans
```

That is one folder, per user, not roamed. Projects are JSON. Exports are ordinary
files. Tonans writes nowhere else, and it never writes outside the workspace you
give it. To remove Tonans completely, delete that folder and delete `Tonans.exe`.
There is nothing else — no registry keys, no services, no scheduled tasks, no
cached credentials. See `PRIVACY.md`.

## The workflow

1. **Create a project** and state the site's timezone.
2. **Import interval CSV** — load, and optionally critical load, generation,
   import/export prices, temperature, and demand-charge periods. You map every
   column and state every unit. Tonans sniffs nothing and repairs nothing: a
   duplicate timestamp, an ambiguous local time during a daylight-saving
   transition, a `NaN`, or a critical load above total load is a hard error naming
   the row and the column.
3. **State the assumptions** — battery, inverter, reserve floor, efficiencies,
   degradation, outage windows, tariff, and economics. Each carries its unit.
4. **Run** baseline and candidate scenarios under named dispatch policies.
5. **Compare** energy balance, grid exchange, peak demand, state of charge,
   curtailment, losses, survivability, unmet critical load, and economics.
6. **Sensitivities** over capacity, power, reserve, tariff, and degradation.
7. **Export** a self-contained HTML report plus CSV, JSON, and a provenance
   manifest carrying input hashes, versions, assumptions, warnings, and the limits
   the run happened inside.

## License

Tonans is open-source software under the Apache License 2.0; see `LICENSE`. `NOTICES.md`
carries the third-party licenses.

## Support

Flintglade — `support@flintglade.com`

When reporting a problem, the provenance manifest from an export
(`provenance.json`) identifies exactly which build produced which numbers from
which inputs. Please attach it. Do not send us your data unless you mean to.
