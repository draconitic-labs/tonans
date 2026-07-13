# Security

Tonans is a local application that listens on a socket. That socket is reachable by
every process on the machine and by web pages that can issue requests to loopback,
even when the browser prevents those pages from reading the response. The HTTP and
filesystem boundaries below are designed around that threat.

## Product boundary

Tonans never accepts or emits an equipment-control command. It has no inverter,
relay, BMS, SCADA, shell-execution, arbitrary-file-read, or arbitrary-path route.
It produces planning estimates for human review, not live control decisions.

## Request defenses in order

1. **Random loopback listener.** The listener binds `127.0.0.1` on a kernel-selected
   port and verifies the resulting address is loopback. Nothing off the machine can
   connect directly.

2. **Bounded admission.** A fixed worker pool consumes a bounded accept queue. When
   the queue is full, the new connection receives a bounded busy response; Tonans
   does not create an unbounded thread or retain an unbounded connection list.

3. **Bounded request head and absolute deadline.** Request-line length, header count,
   and each physical header-line length are checked while reading. A header line must
   terminate within its byte cap. Per-read inactivity limits are enforced inside one
   absolute deadline spanning request-head and body reads, so a peer cannot retain a
   worker indefinitely by sending one byte before each timeout.

4. **Head-only security preflight before body allocation.** Tonans parses method,
   path, and headers, then runs these guards before reading a request body:

   - `Host` must equal the address/port Tonans actually bound (or the corresponding
     `localhost` form), preventing DNS rebinding;
   - a present `Origin` must be Tonans' own loopback origin;
   - the local fixed-window request-rate bound must allow the request; and
   - every `/api` control route must carry the launch bearer token.

   A refused peer cannot make Tonans receive or allocate a large control-route body
   first.

5. **Per-route body framing and limits.** Only an accepted head proceeds to the
   route's `Content-Length` limit. Chunked request bodies are unsupported, leaving
   one request-framing rule. One request is served per connection; keep-alive,
   multipart upload, WebSockets, HTTP/2, and compression are deliberately absent.

6. **Restrictive response policy.** Responses add a Content-Security-Policy with
   `default-src 'none'`, limited `'self'` sources for the embedded application,
   `frame-ancestors 'none'`, `form-action 'none'`, `base-uri 'none'`, and
   `object-src 'none'`. Tonans also sends frame, content-type, referrer, cache,
   cross-origin, and permissions headers that disable unnecessary browser
   capabilities. Response header names and values containing CR or LF are discarded.

## Launch token

Each process mints 256 random bits from the operating system CSPRNG. The token is
never written to disk and dies with the process. Control routes require it as an
`Authorization: Bearer` value, compared in constant time. Static assets are public
to loopback because the page must load before it can send the token; they contain no
project data or secret.

The bootstrap URL carries the token only for the initial local navigation. Responses
use `Referrer-Policy: no-referrer` and `Cache-Control: no-store` so it is not sent
onward or cached by Tonans.

## Embedded and exported UI

Browser assets are compiled into the executable. They reference no CDN, remote font,
analytics service, advertisement, or remote origin. Build tests reject unsafe DOM
sinks such as `innerHTML`, `eval`, `new Function`, `document.write`, and
`insertAdjacentHTML`; untrusted labels are written with `textContent`.

Exported HTML contains no script or remote fetch and carries its own restrictive CSP.
It is designed to remain a static evidence artifact when opened from disk or an
attachment.

## Untrusted CSV and project input

The importer refuses ambiguity rather than repairing it silently. It rejects
malformed or duplicate headers, unit mismatches, non-finite numbers, backwards,
duplicate, or off-grid timestamps, ambiguous/nonexistent local times, invalid load
relationships, empty required cells, and columns that are neither mapped nor
explicitly ignored. Field length is checked even for ignored and previewed cells.

Gap synthesis occurs only under an explicit bounded user policy. Synthesized indices
and counts are retained and a critical warning follows the data set into every run,
report, and provenance manifest.

Input limits are enforced during accumulation, before the attacker-controlled size
has already been allocated: request body, file, row, column, field, interval,
scenario, sensitivity, outage, schedule, demand-period, and numeric-magnitude bounds.

Exported CSV fields that begin with spreadsheet formula markers (`=`, `+`, `-`, `@`,
tab, or carriage return) are prefixed and quoted so Excel/Calc do not evaluate a
project-controlled label as a formula.

## Filesystem and junction defenses

User-visible identifiers are names, never paths. Validation rejects separators,
drive/stream colons, traversal, controls, bidirectional overrides, Windows device
names, and trailing dots or spaces. Invalid names are refused rather than sanitized.

The workspace root is resolved once, but that is not the only check. Before each
operation Tonans verifies that expected child directories and existing files are
direct physical children of their resolved parent. On Windows, any reparse point -
including a junction planted after startup - is refused; on other supported build
environments, symbolic links are refused. The canonical direct-parent relationship
is rechecked at use time. Project files whose internal project name does not match
their filename are refused rather than silently loaded under another identity.

Writes use unique temporary files followed by atomic replacement, so concurrent or
interrupted writes do not share a predictable staging path or expose a partial JSON
document.

## Dependencies and unsafe code

The application uses a small blocking `std::net` HTTP surface instead of an
asynchronous framework stack. The exact locked dependency tree, not a prose count,
is the source of truth. `unsafe_code` is forbidden workspace-wide, `cargo audit`
runs as a release gate, and the shipped package includes a CycloneDX SBOM and the
applicable third-party licence texts.

## Package and runtime boundary

The release goal is a self-contained Windows x64 package. Verification must inspect
the executable's PE imports and prove the staged payload works with compiler and
llvm-mingw directories removed from `PATH`. If a non-system runtime is required, it
must be included in the payload, listed in the SBOM and notices, and covered by the
checksums. A smoke pass that succeeds only because a build toolchain happens to be
installed is not release evidence.

Release builds remap repository, Cargo, and user-profile paths. The independent
package scan rejects personal identity, build-machine paths, test secrets, payment
origins, and undeclared files.

## Signing

This release candidate is explicitly unsigned. Windows SmartScreen may warn that the
publisher cannot be confirmed, and that warning is accurate. The external archive
digest and internal `SHA256SUMS.txt` establish integrity against published values;
they do not provide publisher authentication.

## Reporting a vulnerability

Email `support@flintglade.com` with a reproducible description. Test only systems and
data you are authorized to test.
