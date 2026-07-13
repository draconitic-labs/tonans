# Security

Tonans is a local application that listens on a socket. That socket is reachable by
every process on the machine and — this is the part people miss — by **every web
page you have open**, because a page may issue cross-origin requests to
`http://127.0.0.1:<port>` and does not need to read the response to do damage.

Everything below exists because of that sentence.

---

## The boundary

Tonans **never** accepts or emits an equipment-control command. There is no route
that talks to an inverter, a controller, a relay, a BMS, or SCADA. There is no
route that runs a program, and no route that reads an arbitrary file. There never
will be. This is a product boundary, not a configuration.

## The defences, in the order a request meets them

1. **Loopback only, on a random port.** The listener binds `127.0.0.1` on port 0,
   so the kernel picks the port. Nothing off the machine can connect, and the port
   is not a fixed target for a page scanning for local services. The bind is
   re-checked after the fact — if the resolved address is not a loopback address,
   the process refuses to serve at all.

2. **A launch token on every control route.** 256 bits from the operating system's
   CSPRNG, minted at launch, **never written to disk**, dead when the process exits.
   Required as `Authorization: Bearer <token>` on every `/api` route. It is compared
   in **constant time**, so the comparison's duration cannot be used to reconstruct
   it a byte at a time.

   A page in another tab can reach the socket. It cannot guess the token.

3. **The `Host` header is pinned** to the address actually bound. Without this, DNS
   rebinding works: an attacker's domain resolves to `127.0.0.1`, the browser treats
   the response as same-origin, and the same-origin policy stops helping. A request
   whose `Host` is anything else is refused with a 400 before anything else happens.

4. **A cross-origin `Origin` is refused** with a 403.

5. **Every request is bounded.** A per-route body limit checked against
   `Content-Length` *before a byte of body is read*; a bounded request line, header
   count, and header size, all checked while reading; a read timeout, so a
   connection that dribbles bytes cannot hold a worker open; a fixed worker pool, so
   the number of connections in flight is the pool size and not whatever an attacker
   opens; and a fixed-window rate limit, because the likeliest attacker here is a
   runaway loop in a page.

   There is **no `Transfer-Encoding: chunked` support**, so there is no second way
   to frame a body — which is where request smuggling lives.

6. **A restrictive Content-Security-Policy**: `default-src 'none'`, with `'self'`
   for script, style, image, font, and connect. No `unsafe-inline`, no `unsafe-eval`,
   no wildcard, no remote origin. `frame-ancestors 'none'`, `form-action 'none'`,
   `base-uri 'none'`, `object-src 'none'`. Alongside it: `X-Frame-Options: DENY`,
   `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `no-store`,
   and a `Permissions-Policy` that turns off geolocation, camera, microphone, USB,
   serial, Bluetooth, and payment.

   The CSP is honest because **every byte the browser loads is compiled into the
   executable.** There is no CDN, no font service, and no asset directory. A test
   fails the build if the UI source so much as *mentions* a remote origin.

## The user interface

Nothing is ever assigned to `innerHTML`. Every string that reaches the DOM — a
scenario name, a warning, a source filename — is set through `textContent`, and a
test fails the build if `innerHTML`, `eval`, `new Function`, `document.write`, or
`insertAdjacentHTML` appears in the script. Some of those strings came out of a CSV
that Tonans does not control.

The exported HTML report carries its own `Content-Security-Policy` of
`default-src 'none'`. It has no script and fetches nothing. It opens from a
filesystem, an email attachment, or a USB stick in ten years' time, and it does the
same thing every time.

## Untrusted input

The CSV importer treats every file as hostile and every value as a lie until proved
otherwise. It **refuses rather than repairs**: a malformed header, a unit that
cannot express its role, a `NaN` or infinity, a duplicate or backwards timestamp, a
timestamp off the declared grid, a timestamp that is ambiguous or nonexistent across
a daylight-saving transition, a negative load, a critical load above total load, an
empty cell, or an unmapped column is a hard error naming the row and the column.

Missing intervals are the single exception, and only when the user *deliberately*
configures a gap policy — in which case the synthesized intervals are counted, their
indices recorded, a critical warning rides on the data for the rest of its life, and
the whole thing is capped as a fraction of the file.

**Every unbounded input has a hard bound**, enforced during accumulation rather than
after it: file size, row count, column count, field length, value magnitude,
interval count, scenario count, sensitivity cells, outage windows, schedule entries,
and demand-charge periods. A limit that is only checked once the memory has been
allocated is not a limit.

**Names are never paths.** The browser uploads CSV *bytes* and addresses projects by
*name*. A name is validated — no separators, no colons, no traversal, no control
characters, no bidirectional overrides, no Windows reserved device names, no trailing
dots or spaces — and it is **refused, never sanitized**. A name Tonans cannot store
is an error the user gets to see and fix, not a name Tonans quietly invents. The
join beneath the workspace root is then re-checked against the resolved parent, so a
symlink or junction planted in the workspace cannot redirect a write.

**Exported CSV is defused against spreadsheet formula injection.** A cell beginning
`=`, `+`, `-`, `@`, tab, or carriage return is executed by Excel and Calc when the
file is opened — `=cmd|'/c calc'!A1` is a DDE payload, not a label. Such a field is
prefixed with a single quote *and* quoted, so that no importer's whitespace-trimming
or text-marker handling can undo the neutralisation.

## The dependency tree

Thirty-seven crates, and that number is a design constraint rather than an accident.

Tonans has no HTTP framework. It once used Axum, which brings Tokio, which brings
Mio, which brings `windows-sys` — around sixty crates into the dependency tree of a
binary whose entire pitch is that you can audit it, all of it to serve *one user, on
loopback, at human speed*. The whole HTTP surface is now about three hundred lines
of `std::net`. The argument-parser is sixty lines rather than a derive macro and its
half-dozen crates.

`unsafe_code` is **forbidden** workspace-wide, not merely denied.

`cargo audit` runs against the locked tree as a release gate, and the bill of
materials ships with the binary as `SBOM.cdx.json`.

## Signing

**This release candidate is not code-signed.** Windows SmartScreen will warn that
the publisher cannot be confirmed, and that warning is correct — an unsigned binary
is exactly what it says it is.

Verify the SHA-256 of the ZIP against the `.sha256` sidecar published beside it, and
then `SHA256SUMS.txt` inside the package, which covers the executable itself. The
`README.md` in the package shows the commands.

We would rather ship an honestly unsigned binary with a published checksum than
imply an assurance we have not bought.

## Reporting a vulnerability

`support@flintglade.com`

Tell us what you found and how to reproduce it. If it is a genuine security issue we
will fix it and credit you if you want the credit. Please do not test against
anything that is not your own machine.
