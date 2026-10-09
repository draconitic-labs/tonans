# Privacy

Tonans is a local application. It processes your data on your machine and sends it
nowhere.

This document is short because there is little to say, and that is the point. Every
claim in it is checkable — the last section says how.

---

## What Tonans collects

**Nothing.**

- No telemetry. No analytics. No crash reporting. No usage counters.
- No update check, and therefore no request that would reveal your IP address, your
  version, or the fact that you ran the program.
- No account, no sign-in, no licence activation, no device fingerprint.
- No advertising identifiers, no cookies, no third-party scripts, no fonts fetched
  from anywhere.
- No payment data of any kind: sales are not enabled in this build and there is no
  payment integration compiled into the executable.

Flintglade does not receive your interval data, your project files, your scenario
assumptions, your results, or the fact that Tonans is installed.

## What Tonans stores, and where

One folder, per user, not roamed:

```
%LOCALAPPDATA%\Flintglade\Tonans
```

You can put it somewhere else with `Tonans.exe --workspace <DIR>`.

It contains:

| | |
|---|---|
| `*.tonans.json` | Your projects: the imported interval data, your assumptions, your scenarios. Plain JSON. |
| `exports/` | Export bundles you asked for: the HTML report, CSV, JSON, the provenance manifest, and checksums. |

Uploaded CSV bytes are held **in memory only** while you map the columns, and are
discarded when the import completes or you abandon it. The file you selected is
never copied into the workspace; only the data you actually imported, and a SHA-256
digest of the bytes it came from, is stored.

Browser-driven project and export writes stay in the workspace. The headless CLI
can also write an export to the directory you explicitly choose with `--out`;
remove that directory separately when uninstalling. Tonans creates no registry
keys, services, scheduled tasks, or shell extensions. For workspace operations, names
are validated and joined beneath the workspace root, and the join is re-checked
against the resolved parent so that a symlink planted in the workspace cannot
redirect a write.

## Removing your data

Delete the workspace folder. Delete `Tonans.exe`. That is the whole uninstall.

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Flintglade\Tonans"
```

Also delete any headless export directories you chose with `--out`. You do not
need to ask us to delete your project data, because it was never sent to us.

## The network

Tonans opens exactly one socket: a TCP listener on `127.0.0.1`, on a port the
operating system chooses. That socket serves the user interface — which is compiled
into the executable — to your browser, and nothing else.

It makes **no outbound connections at all.**

You are welcome to confirm this. Run it behind a firewall that blocks everything,
or watch it with Resource Monitor, Process Monitor, or Wireshark. Nothing will
happen, which is the intended result.

The one thing Tonans asks the operating system to do outside itself is open your
browser at the loopback URL on launch. `Tonans.exe --no-browser` suppresses even
that.

## What leaves your machine, if you choose to send it

Only what you deliberately send. An export bundle is a set of ordinary files in a
folder you chose; what you do with them is up to you.

If you report a problem, `provenance.json` from an export identifies which build
produced which numbers from which inputs — it carries versions, hashes, assumptions,
warnings, and the enforced limits. **It also carries your assumptions and the digest
of your data file.** Read it before you send it. We would rather have a description
of the problem than a copy of your site's load profile.

## Children

Tonans is professional engineering-planning software. It is not directed at
children and collects nothing from anyone.

## Changes

This document describes build 0.1.0. If a future build ever collects anything, that
will be stated here, in the release notes, and on screen — and it will be optional.
A silent change to this policy would make every other claim in it worthless.

---

Flintglade — `support@flintglade.com`
