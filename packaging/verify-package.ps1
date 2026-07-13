<#
.SYNOPSIS
    Verifies a packaged Tonans release candidate. Exits non-zero on any failure.

.DESCRIPTION
    This is the gate, not a formality. It re-derives every claim the package makes
    rather than trusting the script that made it, and it is deliberately written to
    be readable by someone who did not write package.ps1.

    It fails on:

      1. A file set that is not exactly the allowlist -- missing OR extra.
      2. Windows version metadata that is absent, or that names anything other than
         the published Flintglade identity.
      3. Personal-identity leakage. Checked as an ALLOWLIST, not a denylist: the
         only email address permitted anywhere in the package is the published
         support address, and no build-machine path may appear. A denylist would
         have to contain the private identity in order to look for it, which would
         put that identity in this repository -- exactly what it is meant to stop.
      4. Test secrets, live origins, or a payment integration reaching the artifact.
      5. A checksum that does not match: internal (SHA256SUMS.txt) or external
         (the .sha256 sidecar).

    It reads only. It does not build, publish, sign, install, or run the binary --
    packaging/smoke.ps1 does the running.
#>
[CmdletBinding()]
param(
    [string] $Zip
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $PSScriptRoot 'out'

# Kept in step with package.ps1 by the round-trip: package.ps1 stages exactly this
# set, and a divergence fails here.
$Payload = @(
    'Tonans.exe'
    'README.md'
    'LICENSE.md'
    'TERMS.md'
    'NOTICES.md'
    'PRIVACY.md'
    'SECURITY.md'
    'MODEL.md'
    'BENCHMARK.md'
    'SBOM.cdx.json'
    'SHA256SUMS.txt'
)

# The published identity. Anything else that looks like a person is a failure.
$AllowedEmail = 'support@flintglade.com'
$AllowedCompany = 'Flintglade'
$AllowedBenchmarkUrls = @(
    'https://github.com/NatLabRockies/ssc/tree/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832'
    'https://github.com/NatLabRockies/ssc/blob/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832/test/shared_test/lib_battery_capacity_test.h#L69-L90'
    'https://github.com/NatLabRockies/ssc/blob/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832/test/shared_test/lib_battery_capacity_test.cpp#L47-L71'
    'https://github.com/NatLabRockies/ssc/blob/86cf2e0a3de4f7dae1134ad3fa22492e2f1b3832/test/shared_test/lib_battery_capacity_test.h#L43-L52'
)

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

# ---------------------------------------------------------------------------
# Locate and unpack
# ---------------------------------------------------------------------------

if (-not $Zip) {
    $candidates = @(Get-ChildItem -Path $OutDir -Filter 'tonans-*-windows-x64.zip' -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -ne 1) {
        Write-Host "FAIL: expected exactly one release zip in $OutDir, found $($candidates.Count). Pass -Zip." -ForegroundColor Red
        exit 1
    }
    $Zip = $candidates[0].FullName
}
if (-not (Test-Path $Zip)) { Write-Host "FAIL: no such file: $Zip" -ForegroundColor Red; exit 1 }

Write-Host "Verifying $(Split-Path -Leaf $Zip)" -ForegroundColor Cyan

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("tonans-verify-" + [System.Guid]::NewGuid().ToString('N').Substring(0, 12))
New-Item -ItemType Directory -Path $work -Force | Out-Null
try {
    Expand-Archive -Path $Zip -DestinationPath $work -Force

    $exe = Join-Path $work 'Tonans.exe'
    $textFiles = @(Get-ChildItem $work -File | Where-Object { $_.Name -ne 'Tonans.exe' })

    # -----------------------------------------------------------------------
    # 1. Exact file set
    # -----------------------------------------------------------------------
    Check 'file set is exactly the allowlist' {
        $actual = @(Get-ChildItem $work -Recurse -File | ForEach-Object {
            $_.FullName.Substring($work.Length + 1).Replace('\', '/')
        } | Sort-Object)
        $expected = @($Payload | Sort-Object)
        $missing = @($expected | Where-Object { $_ -notin $actual })
        $extra = @($actual | Where-Object { $_ -notin $expected })
        $problems = @()
        if ($missing) { $problems += "missing: $($missing -join ', ')" }
        if ($extra) { $problems += "extra: $($extra -join ', ')" }
        if ($problems) { $problems -join '; ' }
    }

    # -----------------------------------------------------------------------
    # 2. Windows version metadata
    # -----------------------------------------------------------------------
    Check 'version metadata names the published identity' {
        if (-not (Test-Path $exe)) { return 'Tonans.exe is not in the package' }
        $info = (Get-Item $exe).VersionInfo

        $cargoToml = Get-Content (Join-Path $RepoRoot 'Cargo.toml') -Raw
        if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') { return 'cannot read the workspace version' }
        $expectedVersion = $Matches[1]

        $problems = @()
        if (-not $info.CompanyName) { $problems += 'CompanyName is empty (no version block was stamped)' }
        if ($info.CompanyName -ne $AllowedCompany) { $problems += "CompanyName is '$($info.CompanyName)', expected '$AllowedCompany'" }
        if ($info.ProductName -ne 'Tonans') { $problems += "ProductName is '$($info.ProductName)'" }
        # VersionInfo.FileVersion is normalised to four parts by Windows.
        if ($info.FileVersion -notmatch [regex]::Escape($expectedVersion)) { $problems += "FileVersion is '$($info.FileVersion)', expected $expectedVersion" }
        if ($info.ProductVersion -notmatch [regex]::Escape($expectedVersion)) { $problems += "ProductVersion is '$($info.ProductVersion)', expected $expectedVersion" }
        if ($info.LegalCopyright -notmatch [regex]::Escape($AllowedCompany)) { $problems += "LegalCopyright does not name $AllowedCompany" }
        if ($problems) { $problems -join '; ' }
    }

    # -----------------------------------------------------------------------
    # 3. No personal-identity leakage (allowlist, in text AND in the binary)
    # -----------------------------------------------------------------------
    Check 'no email domain other than the published one appears anywhere' {
        # Checked on the DOMAIN, deliberately. An executable's string table packs its
        # literals end to end with no separators, so a local-part regex run over the
        # binary matches greedily backwards through whatever preceded the address and
        # reports a false leak. The domain is unambiguous, and the domain is what
        # actually gives a private address away: `someone@gmail.com` fails this, and
        # that is the whole point.
        $allowedDomain = '@' + $AllowedEmail.Split('@')[1]
        $offenders = @()
        foreach ($file in (Get-ChildItem $work -Recurse -File)) {
            # Latin-1, so the binary's byte stream is searchable as text and no byte
            # sequence is lost to a UTF-8 decode error.
            $content = [System.IO.File]::ReadAllText($file.FullName, [System.Text.Encoding]::GetEncoding('iso-8859-1'))
            foreach ($m in [regex]::Matches($content, '@[A-Za-z0-9\-]+(\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}')) {
                if ($m.Value -ne $allowedDomain) { $offenders += "$($file.Name): $($m.Value)" }
            }
        }
        if ($offenders) { "found a mail domain that is not $allowedDomain -- " + (($offenders | Select-Object -Unique | Select-Object -First 5) -join '; ') }
    }

    Check 'the documents name the support address and no other person' {
        # The text files have real word boundaries, so the full address can be checked
        # there without the string-table problem above.
        $offenders = @()
        foreach ($file in $textFiles) {
            $content = Get-Content $file.FullName -Raw
            foreach ($m in [regex]::Matches($content, '[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}')) {
                if ($m.Value -ne $AllowedEmail) { $offenders += "$($file.Name): $($m.Value)" }
            }
        }
        if ($offenders) { "found an address that is not $AllowedEmail -- " + (($offenders | Select-Object -Unique | Select-Object -First 5) -join '; ') }
    }

    Check 'no build-machine path is embedded' {
        # The regression this guards: Rust embeds dependency source paths in panic
        # metadata, and they sit under the packager's profile directory --
        # `strip = "debuginfo"` does not remove them. package.ps1 remaps them away.
        # If that remap is ever dropped, the shipped binary carries the packager's
        # Windows username, and this is what notices.
        $offenders = @()
        foreach ($file in (Get-ChildItem $work -Recurse -File)) {
            $content = [System.IO.File]::ReadAllText($file.FullName, [System.Text.Encoding]::GetEncoding('iso-8859-1'))
            foreach ($pattern in @('C:\\Users\\', '\\.worktrees\\', '\\AI\\Workshop\\', '/Users/', '/home/[a-z]')) {
                foreach ($m in [regex]::Matches($content, $pattern)) {
                    $offenders += "$($file.Name): $($m.Value)"
                }
            }
        }
        if ($offenders) { 'build-machine path found -- ' + (($offenders | Select-Object -Unique | Select-Object -First 5) -join '; ') }
    }

    # -----------------------------------------------------------------------
    # 4. No test secrets, live origins, or payment integration
    # -----------------------------------------------------------------------
    Check 'no payment integration or test secret reached the artifact' {
        # Sales are disabled in this build by design. A Stripe key or endpoint in a
        # shipped artifact would mean that stopped being true.
        $forbidden = @(
            'stripe', 'sk_live_', 'sk_test_', 'pk_live_', 'pk_test_',
            'BEGIN PRIVATE KEY', 'BEGIN RSA PRIVATE KEY', 'AKIA', 'xoxb-'
        )
        $offenders = @()
        foreach ($file in (Get-ChildItem $work -Recurse -File)) {
            $content = [System.IO.File]::ReadAllText($file.FullName, [System.Text.Encoding]::GetEncoding('iso-8859-1'))
            foreach ($needle in $forbidden) {
                if ($content -match [regex]::Escape($needle)) { $offenders += "$($file.Name): $needle" }
            }
        }
        if ($offenders) { ($offenders | Select-Object -Unique) -join '; ' }
    }

    Check 'documents Flintglade wrote reference no remote origin' {
        # The threat is a document of ours pointing a user at a live origin: an
        # analytics endpoint, an update server, a licensing host, a payment page.
        #
        # NOTICES.md is excluded, and the exclusion is the honest one: its URLs are
        # inside the verbatim text of the Apache-2.0 and MIT licences. Editing a URL
        # out of a licence to satisfy a scanner would alter the licence, which is a
        # worse thing to do than the thing this check exists to prevent.
        #
        # The binary is excluded too, and for a real reason: it legitimately contains
        # the string "http://" because it formats its own loopback URL. The
        # loopback-only claim is checked separately, below.
        $ours = @($textFiles | Where-Object { $_.Name -ne 'NOTICES.md' })
        $offenders = @()
        foreach ($file in $ours) {
            $content = Get-Content $file.FullName -Raw
            foreach ($m in [regex]::Matches($content, 'https?://[^\s)"''>`]+')) {
                $url = $m.Value
                if ($url -match '^https?://(127\.0\.0\.1|localhost)') { continue }
                if ($file.Name -eq 'BENCHMARK.md' -and $url -in $AllowedBenchmarkUrls) { continue }
                $offenders += "$($file.Name): $url"
            }
        }
        if ($offenders) { 'remote origin in a document we wrote -- ' + (($offenders | Select-Object -Unique | Select-Object -First 5) -join '; ') }
    }

    Check 'NOTICES.md carries only third-party licence text, not our own links' {
        # NOTICES.md is exempt from the check above, so it gets its own, narrower one:
        # every origin in it must belong to a licence steward. A Flintglade-controlled
        # origin appearing here would be smuggling exactly what that exemption exists
        # to permit.
        $stewards = @('apache.org', 'opensource.org', 'unlicense.org', 'mozilla.org', 'gnu.org', 'unicode.org', 'rust-lang.org')
        $offenders = @()
        $content = Get-Content (Join-Path $work 'NOTICES.md') -Raw
        foreach ($m in [regex]::Matches($content, 'https?://([^/\s)"''>`]+)')) {
            $originHost = $m.Groups[1].Value
            $known = $false
            foreach ($steward in $stewards) { if ($originHost -like "*$steward") { $known = $true } }
            if (-not $known) { $offenders += $m.Value }
        }
        if ($offenders) { 'an origin in NOTICES.md is not a licence steward -- ' + (($offenders | Select-Object -Unique | Select-Object -First 5) -join '; ') }
    }

    Check 'the binary binds only loopback' {
        $content = [System.IO.File]::ReadAllText($exe, [System.Text.Encoding]::GetEncoding('iso-8859-1'))
        if ($content -match '0\.0\.0\.0') { 'the binary contains the string 0.0.0.0' }
    }

    Check 'the PE imports only Windows system libraries and no libunwind' {
        $objdump = Get-Command 'llvm-objdump.exe' -ErrorAction SilentlyContinue
        if (-not $objdump) { $objdump = Get-Command 'llvm-objdump' -ErrorAction SilentlyContinue }
        if (-not $objdump) { return 'llvm-objdump is required to inspect the release executable imports' }

        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $dump = @(& $objdump.Source -p $exe 2>&1)
            $code = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $previous
        }
        if ($code -ne 0) { return "llvm-objdump exited $code" }

        $imports = @($dump | ForEach-Object {
            if ($_ -match '^\s*DLL Name:\s*(\S+)\s*$') { $Matches[1].ToLowerInvariant() }
        } | Where-Object { $_ } | Sort-Object -Unique)
        if (-not $imports) { return 'llvm-objdump reported no imported DLLs' }
        if ($imports -contains 'libunwind.dll') { return 'libunwind.dll is dynamically imported' }

        $allowed = @(
            'advapi32.dll', 'bcrypt.dll', 'crypt32.dll', 'gdi32.dll',
            'kernel32.dll', 'msvcrt.dll', 'ntdll.dll', 'ole32.dll',
            'shell32.dll', 'user32.dll', 'userenv.dll', 'ws2_32.dll'
        )
        $unexpected = @($imports | Where-Object {
            $_ -notin $allowed -and
            $_ -notlike 'api-ms-win-core-*.dll' -and
            $_ -notlike 'api-ms-win-crt-*.dll'
        })
        if ($unexpected) { "non-system DLL import(s): $($unexpected -join ', ')" }
    }

    # -----------------------------------------------------------------------
    # 5. Checksums, internal and external
    # -----------------------------------------------------------------------
    Check 'internal checksums (SHA256SUMS.txt) match every file' {
        $sums = Join-Path $work 'SHA256SUMS.txt'
        if (-not (Test-Path $sums)) { return 'SHA256SUMS.txt is not in the package' }

        $listed = @{}
        foreach ($line in (Get-Content $sums)) {
            if (-not $line.Trim()) { continue }
            if ($line -notmatch '^([0-9a-f]{64})  (.+)$') { return "malformed line: $line" }
            $listed[$Matches[2]] = $Matches[1]
        }

        # Everything except the digest file itself must be covered -- a checksum file
        # that quietly omits a file is worse than none.
        $shouldCover = @($Payload | Where-Object { $_ -ne 'SHA256SUMS.txt' } | Sort-Object)
        $covered = @($listed.Keys | Sort-Object)
        if (Compare-Object $covered $shouldCover) {
            return "coverage mismatch -- listed: $($covered -join ', '); expected: $($shouldCover -join ', ')"
        }

        $bad = @()
        foreach ($name in $listed.Keys) {
            $actual = (Get-FileHash (Join-Path $work $name) -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $listed[$name]) { $bad += "$name (listed $($listed[$name]), actual $actual)" }
        }
        if ($bad) { 'digest mismatch -- ' + ($bad -join '; ') }
    }

    Check 'external checksum sidecar matches the zip' {
        $sidecar = "$Zip.sha256"
        if (-not (Test-Path $sidecar)) { return "no sidecar at $sidecar" }
        $line = (Get-Content $sidecar -Raw).Trim()
        if ($line -notmatch '^([0-9a-f]{64})\s\s(.+)$') { return "malformed sidecar: $line" }
        $claimed = $Matches[1]
        $namedFile = $Matches[2]
        if ($namedFile -ne (Split-Path -Leaf $Zip)) { return "the sidecar names '$namedFile', not '$(Split-Path -Leaf $Zip)'" }
        $actual = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
        if ($actual -ne $claimed) { "sidecar claims $claimed, the zip hashes to $actual" }
    }

    # -----------------------------------------------------------------------
    # 6. The SBOM describes this build
    # -----------------------------------------------------------------------
    Check 'the SBOM is semantically complete and lists the shipped crates' {
        $bom = Get-Content (Join-Path $work 'SBOM.cdx.json') -Raw | ConvertFrom-Json
        $problems = @()
        if ($bom.bomFormat -ne 'CycloneDX') { $problems += "bomFormat is '$($bom.bomFormat)'" }
        if ($bom.specVersion -ne '1.5') { $problems += "specVersion is '$($bom.specVersion)', expected 1.5" }
        if ([int]$bom.version -lt 1) { $problems += "serial version is '$($bom.version)'" }

        $cargoToml = Get-Content (Join-Path $RepoRoot 'Cargo.toml') -Raw
        if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
            $problems += 'cannot read the workspace version'
            $expectedVersion = $null
        } else {
            $expectedVersion = $Matches[1]
        }
        $root = $bom.metadata.component
        if ($root.type -ne 'application') { $problems += "root type is '$($root.type)'" }
        if ($root.name -ne 'Tonans') { $problems += "root name is '$($root.name)'" }
        if ($expectedVersion -and $root.version -ne $expectedVersion) { $problems += "root version is '$($root.version)', expected $expectedVersion" }
        if ($root.publisher -ne $AllowedCompany) { $problems += "publisher is '$($root.publisher)'" }
        if ($expectedVersion -and $root.purl -ne "pkg:cargo/tonans@$expectedVersion") { $problems += "root purl is '$($root.purl)'" }
        $rootLicenses = @($root.licenses)
        if ($rootLicenses.Count -ne 1 -or
            -not ($rootLicenses[0].PSObject.Properties.Name -contains 'expression') -or
            $rootLicenses[0].expression -ne 'LicenseRef-Flintglade-Tonans-Commercial') {
            $problems += 'root license is not the commercial LicenseRef expression'
        }

        if (@($bom.components).Count -lt 10) { $problems += "only $(@($bom.components).Count) components -- the tree is bigger than that" }
        $refs = @{}
        $componentPairs = @()
        foreach ($component in $bom.components) {
            if (-not $component.name -or -not $component.version) { $problems += 'a component has no name or version' }
            if ($component.type -ne 'library') { $problems += "$($component.name) has type '$($component.type)'" }
            if ($component.scope -ne 'required') { $problems += "$($component.name) has scope '$($component.scope)'" }
            $expectedPurl = "pkg:cargo/$($component.name)@$($component.version)"
            if ($component.purl -ne $expectedPurl) { $problems += "$($component.name) has purl '$($component.purl)'" }
            if ($component.'bom-ref' -ne $expectedPurl) { $problems += "$($component.name) has bom-ref '$($component.'bom-ref')'" }
            if ($refs.ContainsKey($component.'bom-ref')) { $problems += "duplicate bom-ref '$($component.'bom-ref')'" } else { $refs[$component.'bom-ref'] = $true }

            $choices = @($component.licenses)
            if ($choices.Count -ne 1 -or -not ($choices[0].PSObject.Properties.Name -contains 'expression')) {
                $problems += "$($component.name) has no single license expression"
            } else {
                $expression = [string]$choices[0].expression
                if (-not $expression.Trim() -or $expression -eq 'see repository') { $problems += "$($component.name) has no declared license" }
                if ($expression -in @('MIT/Apache-2.0', 'Unlicense/MIT')) { $problems += "$($component.name) has an unnormalized license expression '$expression'" }
            }
            $componentPairs += "$($component.name)|$($component.version)"
        }

        $noticePairs = @()
        foreach ($line in (Get-Content (Join-Path $work 'NOTICES.md'))) {
            if ($line -match '^\| ([^|]+) \| ([^|]+) \|') {
                if ($Matches[1].Trim() -eq 'Component') { continue }
                $noticePairs += "$($Matches[1].Trim())|$($Matches[2].Trim())"
            }
        }
        $componentPairs = @($componentPairs | Sort-Object -Unique)
        $noticePairs = @($noticePairs | Sort-Object -Unique)
        if (Compare-Object $componentPairs $noticePairs) {
            $problems += 'SBOM components do not match the NOTICES component table'
        }
        if ($problems) { $problems -join '; ' }
    }

    Check 'the bundled notices include siphasher COPYING' {
        $notices = Get-Content (Join-Path $work 'NOTICES.md') -Raw
        if ($notices -notmatch '(?m)^## COPYING \(as shipped with siphasher ') {
            'siphasher COPYING is absent'
        }
    }

    Check 'the package states that it is unsigned and unpublished' {
        $readme = Get-Content (Join-Path $work 'README.md') -Raw
        $problems = @()
        if ($readme -notmatch '(?i)unsigned') { $problems += 'README.md does not say the binary is unsigned' }
        if ($readme -notmatch '(?i)planning estimate') { $problems += 'README.md does not carry the planning-estimate boundary' }
        $terms = Get-Content (Join-Path $work 'TERMS.md') -Raw
        if ($terms -notmatch '(?i)not enabled|sales are not') { $problems += 'TERMS.md does not say sales are not enabled' }
        if ($problems) { $problems -join '; ' }
    }

} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
if ($script:Failures.Count -gt 0) {
    Write-Host "VERIFICATION FAILED -- $($script:Failures.Count) problem(s)" -ForegroundColor Red
    exit 1
}
Write-Host 'VERIFICATION PASSED' -ForegroundColor Green
Write-Host 'The package is internally consistent, carries the published identity only, and is unsigned.' -ForegroundColor Green
Write-Host 'It has NOT been published. Publication remains a manual, human decision.' -ForegroundColor Yellow
exit 0
