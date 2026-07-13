<#
.SYNOPSIS
    Launches the packaged Tonans binary against an isolated workspace, checks its
    loopback and UI boundary, and stops it cleanly. Exits non-zero on any failure.

.DESCRIPTION
    verify-package.ps1 reads the package. This runs it -- the difference between
    "the artifact looks right" and "the artifact works".

    Everything happens in a temporary directory. The real workspace under
    %LOCALAPPDATA%\Flintglade\Tonans is never touched, no browser is opened
    (--no-browser), no shortcut is created, and the process is stopped through the
    application's own shutdown route rather than being killed -- so a failure to
    shut down cleanly is itself a finding.

    What it proves:

      * The binary starts, installs its demonstration project, and binds a port.
      * That port is on 127.0.0.1 and nothing else.
      * The UI is served, and it carries the planning-estimate boundary in markup
        (not only from script -- a page whose script failed must still say what it
        is).
      * The control API refuses a request with no token, refuses one with a wrong
        token, and refuses one whose Host header is not the address it bound (the
        DNS-rebinding guard).
      * With the launch token it answers, and reports the Evaluation tier.
      * It shuts down when asked, and the process exits.
#>
[CmdletBinding()]
param(
    [string] $Zip
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$OutDir = Join-Path $PSScriptRoot 'out'

$script:Failures = @()
function Check($name, [scriptblock] $body) {
    try {
        $problem = & $body
        if ($problem) {
            $script:Failures += "$name -- $problem"
            Write-Host "  FAIL  $name" -ForegroundColor Red
            Write-Host "        $problem" -ForegroundColor Red
        } else {
            Write-Host "  ok    $name" -ForegroundColor Green
        }
    } catch {
        $script:Failures += "$name -- threw: $_"
        Write-Host "  FAIL  $name" -ForegroundColor Red
        Write-Host "        threw: $_" -ForegroundColor Red
    }
}

# A raw request, so the Host header can be set to something the .NET client would
# not allow. That header is the DNS-rebinding guard, and it has to be testable.
function Invoke-Raw {
    param(
        [string] $Method, [string] $Path, [string] $HostHeader,
        [int] $Port, [string] $Token, [string] $Origin
    )
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $client.Connect('127.0.0.1', $Port)
        $stream = $client.GetStream()
        $lines = @("$Method $Path HTTP/1.1", "Host: $HostHeader")
        if ($Token) { $lines += "Authorization: Bearer $Token" }
        if ($Origin) { $lines += "Origin: $Origin" }
        $lines += @('Connection: close', '', '')
        $bytes = [System.Text.Encoding]::ASCII.GetBytes($lines -join "`r`n")
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush()
        $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
        $reader.ReadToEnd()
    } finally {
        $client.Close()
    }
}

function Get-Status([string] $response) {
    if ($response -match '^HTTP/1\.1 (\d{3})') { [int]$Matches[1] } else { -1 }
}

# ---------------------------------------------------------------------------
# Unpack
# ---------------------------------------------------------------------------

if (-not $Zip) {
    $candidates = @(Get-ChildItem -Path $OutDir -Filter 'tonans-*-windows-x64.zip' -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -ne 1) {
        Write-Host "FAIL: expected exactly one release zip in $OutDir, found $($candidates.Count). Pass -Zip." -ForegroundColor Red
        exit 1
    }
    $Zip = $candidates[0].FullName
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("tonans-smoke-" + [System.Guid]::NewGuid().ToString('N').Substring(0, 12))
$appDir = Join-Path $work 'app'
$workspaceDir = Join-Path $work 'workspace'
New-Item -ItemType Directory -Path $appDir, $workspaceDir -Force | Out-Null

Write-Host "Smoke-testing $(Split-Path -Leaf $Zip)" -ForegroundColor Cyan
Write-Host "  isolated workspace: $workspaceDir"

$process = $null
$originalPath = $env:PATH
try {
    Expand-Archive -Path $Zip -DestinationPath $appDir -Force
    $exe = Join-Path $appDir 'Tonans.exe'

    # A release smoke test must not succeed because the build toolchain happens to
    # carry a DLL that the package forgot. Restrict child-process DLL resolution to
    # Windows system directories and run from the extracted application directory.
    $env:PATH = @(
        (Join-Path $env:SystemRoot 'System32')
        $env:SystemRoot
        (Join-Path $env:SystemRoot 'System32\Wbem')
    ) -join ';'

    $stdout = Join-Path $work 'stdout.txt'
    $stderr = Join-Path $work 'stderr.txt'

    # --no-browser: this must not open a window on the operator's desktop.
    # --port 0:     the kernel picks, which is what a real launch does.
    $process = Start-Process -FilePath $exe `
        -ArgumentList @('--no-browser', '--port', '0', '--workspace', $workspaceDir) `
        -WorkingDirectory $appDir `
        -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr

    # `Start-Process -PassThru` does not keep the process handle open, and without the
    # handle .NET cannot report an exit code once the process is gone -- `ExitCode`
    # comes back empty. Reading `Handle` here makes .NET cache it, so the clean-exit
    # check at the end can actually tell 0 from anything else.
    $null = $process.Handle

    # Wait for the URL line rather than sleeping a fixed amount: the first launch
    # imports the demonstration CSV through the real importer, which takes a moment.
    $url = $null
    $deadline = [datetime]::UtcNow.AddSeconds(30)
    while ([datetime]::UtcNow -lt $deadline) {
        if (Test-Path $stdout) {
            $text = Get-Content $stdout -Raw -ErrorAction SilentlyContinue
            if ($text -and $text -match '(http://127\.0\.0\.1:(\d+)/\?token=([0-9a-f]{64}))') {
                $url = $Matches[1]; $port = [int]$Matches[2]; $token = $Matches[3]
                break
            }
        }
        if ($process.HasExited) { break }
        Start-Sleep -Milliseconds 200
    }

    Check 'the binary starts and prints a loopback launch URL' {
        if ($process.HasExited) {
            $err = if (Test-Path $stderr) { Get-Content $stderr -Raw } else { '' }
            return "the process exited with code $($process.ExitCode). stderr: $err"
        }
        if (-not $url) { return "no launch URL was printed within 30s. stdout: $(Get-Content $stdout -Raw -ErrorAction SilentlyContinue)" }
    }
    if (-not $url) { throw 'cannot continue without a launch URL' }

    Write-Host "  bound: 127.0.0.1:$port"

    Check 'it listens on loopback and on nothing else' {
        # The bind is 127.0.0.1 in code; this checks the running process, which is
        # the only claim that actually protects the user.
        $listening = @(Get-NetTCPConnection -State Listen -OwningProcess $process.Id -ErrorAction SilentlyContinue)
        if (-not $listening) { return 'the process has no listening socket' }
        $offenders = @($listening | Where-Object { $_.LocalAddress -notin @('127.0.0.1', '::1') })
        if ($offenders) {
            return 'listening off loopback: ' + (($offenders | ForEach-Object { "$($_.LocalAddress):$($_.LocalPort)" }) -join ', ')
        }
    }

    Check 'the UI is served and states the boundary in markup' {
        $response = Invoke-Raw -Method GET -Path '/' -HostHeader "127.0.0.1:$port" -Port $port
        $status = Get-Status $response
        if ($status -ne 200) { return "GET / returned $status" }
        $problems = @()
        # In the markup, not merely from script: a page whose script failed to load
        # must still say what it is.
        if ($response -notmatch 'Planning estimate') { $problems += 'the page does not carry the planning-estimate boundary' }
        if ($response -notmatch "default-src 'none'") { $problems += 'no restrictive Content-Security-Policy header' }
        if ($response -notmatch '(?i)x-frame-options: DENY') { $problems += 'the page is framable' }
        if ($problems) { $problems -join '; ' }
    }

    Check 'the control API refuses a request carrying no token' {
        $response = Invoke-Raw -Method GET -Path '/api/session' -HostHeader "127.0.0.1:$port" -Port $port
        $status = Get-Status $response
        if ($status -ne 401) { "GET /api/session with no token returned $status, expected 401" }
    }

    Check 'the control API refuses a wrong token' {
        $wrong = '0' * 64
        $response = Invoke-Raw -Method GET -Path '/api/session' -HostHeader "127.0.0.1:$port" -Port $port -Token $wrong
        $status = Get-Status $response
        if ($status -ne 401) { "GET /api/session with a wrong token returned $status, expected 401" }
    }

    Check 'the DNS-rebinding guard refuses a foreign Host header' {
        # An attacker's domain resolving to 127.0.0.1 reaches this socket, and the
        # browser calls it same-origin. Pinning Host is what stops it.
        $response = Invoke-Raw -Method GET -Path '/api/session' -HostHeader 'evil.test' -Port $port -Token $token
        $status = Get-Status $response
        if ($status -ne 400) { "a request with Host: evil.test returned $status, expected 400" }
    }

    Check 'a cross-origin request is refused' {
        $response = Invoke-Raw -Method GET -Path '/api/session' -HostHeader "127.0.0.1:$port" -Port $port -Token $token -Origin 'http://evil.test'
        $status = Get-Status $response
        if ($status -ne 403) { "a cross-origin request returned $status, expected 403" }
    }

    Check 'with the launch token it answers, in the Evaluation tier' {
        $response = Invoke-Raw -Method GET -Path '/api/session' -HostHeader "127.0.0.1:$port" -Port $port -Token $token
        $status = Get-Status $response
        if ($status -ne 200) { return "GET /api/session returned $status" }
        $body = ($response -split "`r`n`r`n", 2)[1]
        $session = $body | ConvertFrom-Json
        $problems = @()
        if ($session.tier -ne 'evaluation') { $problems += "tier is '$($session.tier)', expected evaluation" }
        if (-not $session.disclaimer) { $problems += 'the session carries no disclaimer' }
        if ($session.disclaimer -notmatch 'Planning estimate') { $problems += 'the disclaimer does not lead with the boundary' }
        # The shipped build must not have licensing wired up.
        if ($session.entitlements.client_report_rights) { $problems += 'the shipped build claims client-facing report rights' }
        if ($problems) { $problems -join '; ' }
    }

    Check 'the demonstration project is installed and openable' {
        $response = Invoke-Raw -Method GET -Path '/api/projects' -HostHeader "127.0.0.1:$port" -Port $port -Token $token
        if ((Get-Status $response) -ne 200) { return 'GET /api/projects failed' }
        $body = ($response -split "`r`n`r`n", 2)[1]
        $listing = $body | ConvertFrom-Json
        if (@($listing.projects).Count -lt 1) { 'no demonstration project was installed on first launch' }
    }

    Check 'it writes only inside the workspace it was given' {
        # The isolated workspace should now hold the demo project; nothing should
        # have been written next to the binary.
        $strays = @(Get-ChildItem $appDir -Recurse -File | Where-Object { $_.Name -notin @(
            'Tonans.exe', 'README.md', 'LICENSE.md', 'TERMS.md', 'NOTICES.md',
            'PRIVACY.md', 'SECURITY.md', 'MODEL.md', 'BENCHMARK.md',
            'SBOM.cdx.json', 'SHA256SUMS.txt') })
        if ($strays) { 'the application wrote next to its binary: ' + (($strays | ForEach-Object { $_.Name }) -join ', ') }
    }

    Check 'it shuts down when asked, rather than having to be killed' {
        $response = Invoke-Raw -Method POST -Path '/api/shutdown' -HostHeader "127.0.0.1:$port" -Port $port -Token $token
        if ((Get-Status $response) -ne 200) { return 'POST /api/shutdown was not accepted' }
        if (-not $process.WaitForExit(15000)) { return 'the process was still running 15s after being asked to stop' }
        # The parameterless overload settles the exit state after the timed one has
        # confirmed the process is gone.
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) { "the process exited with code $($process.ExitCode), expected 0" }
    }

    # -----------------------------------------------------------------------
    # The product pipeline, end to end, on the packaged binary
    # -----------------------------------------------------------------------

    Check 'it runs a project headlessly and writes a verifiable export bundle' {
        # Everything above tests the envelope. This tests the thing inside it: the
        # demonstration project was imported through the real importer on first
        # launch, and `run` takes it through the engine, the economics, the report,
        # and the export -- then checks the bundle's own checksums.
        $bundleDir = Join-Path $work 'bundle'
        $runOut = Join-Path $work 'run-stdout.txt'
        $runErr = Join-Path $work 'run-stderr.txt'

        # Start-Process joins -ArgumentList with spaces and quotes nothing, so an
        # argument containing a space has to carry its own quotes -- otherwise the
        # demo project's name arrives as four separate arguments.
        $runner = Start-Process -FilePath $exe -PassThru -NoNewWindow -Wait `
            -WorkingDirectory $appDir `
            -ArgumentList @('run',
                            '--project', '"Demo - Riverside Works"',
                            '--workspace', "`"$workspaceDir`"",
                            '--out', "`"$bundleDir`"") `
            -RedirectStandardOutput $runOut -RedirectStandardError $runErr
        if ($runner.ExitCode -ne 0) {
            $why = Get-Content $runErr -Raw -ErrorAction SilentlyContinue
            return "'tonans run' exited $($runner.ExitCode): $why"
        }

        $problems = @()
        foreach ($artifact in @('report.html', 'summary.json', 'scenarios.csv', 'provenance.json', 'SHA256SUMS.txt')) {
            if (-not (Test-Path (Join-Path $bundleDir $artifact))) { $problems += "the bundle has no $artifact" }
        }
        if ($problems) { return $problems -join '; ' }

        # The bundle's SHA256SUMS covers every other file in it, provenance included.
        # If it does not verify, the export is not self-defending and the whole
        # provenance story is decoration.
        foreach ($line in (Get-Content (Join-Path $bundleDir 'SHA256SUMS.txt'))) {
            if (-not $line.Trim()) { continue }
            if ($line -notmatch '^([0-9a-f]{64})  (.+)$') { $problems += "malformed checksum line: $line"; continue }
            $path = Join-Path $bundleDir $Matches[2]
            if (-not (Test-Path $path)) { $problems += "SHA256SUMS names $($Matches[2]), which is not in the bundle"; continue }
            $actual = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $Matches[1]) { $problems += "digest mismatch for $($Matches[2])" }
        }

        # The report must carry the boundary and must not be able to fetch anything.
        $report = Get-Content (Join-Path $bundleDir 'report.html') -Raw
        if ($report -notmatch 'Planning estimate') { $problems += 'the exported report does not carry the planning-estimate boundary' }
        if ($report -notmatch "default-src 'none'") { $problems += 'the exported report has no restrictive Content-Security-Policy' }
        if ($report -match '<script') { $problems += 'the exported report contains a script tag' }

        # The energy balance must close. A residual the model cannot explain is the
        # one number that invalidates every other number in the bundle.
        $summary = Get-Content (Join-Path $bundleDir 'summary.json') -Raw | ConvertFrom-Json
        foreach ($scenario in $summary.scenarios) {
            $residual = [math]::Abs([double]$scenario.energy.energy_balance_residual_kwh)
            if ($residual -gt 1e-6) {
                $problems += "scenario '$($scenario.scenario_name)' has an energy-balance residual of $residual kWh"
            }
        }
        if ($problems) { $problems -join '; ' }
    }

} finally {
    $env:PATH = $originalPath
    if ($process -and -not $process.HasExited) {
        Write-Host '  (stopping the process: it did not exit on its own)' -ForegroundColor Yellow
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
if ($script:Failures.Count -gt 0) {
    Write-Host "SMOKE TEST FAILED -- $($script:Failures.Count) problem(s)" -ForegroundColor Red
    exit 1
}
Write-Host 'SMOKE TEST PASSED' -ForegroundColor Green
Write-Host 'The packaged binary serves on loopback, enforces its token and Host boundary, and stops cleanly.' -ForegroundColor Green
exit 0
