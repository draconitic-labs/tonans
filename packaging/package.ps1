<#
.SYNOPSIS
    Builds the Tonans Windows x64 release candidate and packages it.

.DESCRIPTION
    Produces, under packaging/out/:

      tonans-<version>-windows-x64.zip          the release candidate
      tonans-<version>-windows-x64.zip.sha256   external checksum sidecar

    The ZIP holds an exact, closed set of files -- see $Payload below. Nothing
    else goes in, and packaging/verify-package.ps1 fails the release if anything
    is missing or extra.

    Two things this script does that a plain `cargo build --release` does not:

    * It remaps source-path prefixes out of the binary. Rust embeds the paths of
      dependency sources in panic-location metadata, and those paths live under
      the build machine's home directory -- so an un-remapped release binary
      ships the packager's Windows username inside it. `strip = "debuginfo"`
      does not remove them. verify-package.ps1 proves the remap worked.

    * It writes the provenance a recipient needs and cannot reconstruct: a
      CycloneDX SBOM, third-party notices, and internal checksums.

    This script does NOT publish, sign, upload, install, or create a shortcut.
    It writes files into packaging/out/ and stops.

.PARAMETER SkipBuild
    Package whatever is already in target/release. For iterating on the
    packaging itself; a real release candidate must be built by this script.
#>
[CmdletBinding()]
param(
    [switch] $SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $PSScriptRoot 'out'
$StageDir = Join-Path $OutDir 'stage'
$Toolchain = 'stable-x86_64-pc-windows-gnullvm'

# The exact set of files in the ZIP. verify-package.ps1 reads this same list.
# A file not named here is a packaging bug, not a bonus.
$Payload = @(
    'Tonans.exe'
    'README.md'
    'LICENSE'
    'NOTICES.md'
    'PRIVACY.md'
    'SECURITY.md'
    'MODEL.md'
    'BENCHMARK.md'
    'SBOM.cdx.json'
    'SHA256SUMS.txt'
)

function Step($message) { Write-Host "==> $message" -ForegroundColor Cyan }
function Fail($message) { Write-Host "FAIL: $message" -ForegroundColor Red; exit 1 }

# Windows PowerShell 5.1 turns anything a native program writes to stderr into an
# ErrorRecord, and `$ErrorActionPreference = 'Stop'` then aborts on it -- so cargo's
# ordinary "Compiling ..." progress would kill this script. A native tool reports
# failure through its exit code, and that is what gets checked.
function Invoke-Native {
    param([Parameter(Mandatory)][scriptblock] $Command, [Parameter(Mandatory)][string] $What)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Command
        if ($LASTEXITCODE -ne 0) { Fail "$What exited $LASTEXITCODE" }
    } finally {
        $ErrorActionPreference = $previous
    }
}

# ---------------------------------------------------------------------------
# Version
# ---------------------------------------------------------------------------

$cargoToml = Get-Content (Join-Path $RepoRoot 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    Fail 'could not read the workspace version out of Cargo.toml'
}
$Version = $Matches[1]
$Stem = "tonans-$Version-windows-x64"
Step "Tonans $Version -- Windows x64 release candidate"

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

$exePath = Join-Path $RepoRoot 'target\release\tonans.exe'

if (-not $SkipBuild) {
    Step 'Building (release, path-prefixes remapped)'

    # The leak this closes: dependency sources live under CARGO_HOME, which lives
    # under the packager's profile directory, and their paths survive into the
    # binary's panic metadata. Remap each prefix to a neutral token. Derived at
    # run time -- nothing machine-specific is committed to the repository.
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $remaps = @(
        '-C target-feature=+crt-static'
        '-C link-arg=-Wl,-Bstatic'
        '-C link-arg=-lunwind'
        '-C link-arg=-Wl,-Bdynamic'
        "--remap-path-prefix=$cargoHome=/cargo"
        "--remap-path-prefix=$RepoRoot=/tonans"
        "--remap-path-prefix=$($env:USERPROFILE)=/home"
    )
    $env:RUSTFLAGS = ($remaps -join ' ')

    Push-Location $RepoRoot
    try {
        # Touch the build script so the version resource is re-stamped even when
        # nothing else changed: a stale resource is exactly the kind of thing a
        # release should not inherit from a previous build.
        (Get-Item (Join-Path $RepoRoot 'crates\tonans-app\build.rs')).LastWriteTime = Get-Date
        Invoke-Native { & cargo "+$Toolchain" build --release --locked -p tonans-app } 'cargo build'
    } finally {
        Pop-Location
        Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue
    }
}

if (-not (Test-Path $exePath)) { Fail "no binary at $exePath" }

# ---------------------------------------------------------------------------
# Stage
# ---------------------------------------------------------------------------

Step 'Staging the payload'
if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

Copy-Item $exePath (Join-Path $StageDir 'Tonans.exe')
Copy-Item (Join-Path $RepoRoot 'packaging\payload\README.md') (Join-Path $StageDir 'README.md')
Copy-Item (Join-Path $RepoRoot 'LICENSE') $StageDir
Copy-Item (Join-Path $RepoRoot 'docs\PRIVACY.md') $StageDir
Copy-Item (Join-Path $RepoRoot 'docs\SECURITY.md') $StageDir
Copy-Item (Join-Path $RepoRoot 'docs\MODEL.md') $StageDir
Copy-Item (Join-Path $RepoRoot 'docs\BENCHMARK.md') $StageDir

# ---------------------------------------------------------------------------
# SBOM and notices, from the locked dependency graph
# ---------------------------------------------------------------------------

Step 'Reading the locked dependency graph'
Push-Location $RepoRoot
try {
    # stdout only: cargo writes its progress to stderr, and that is not JSON.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $metadataJson = & cargo "+$Toolchain" metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-gnullvm 2>$null
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($code -ne 0) { Fail "cargo metadata exited $code" }
} finally {
    Pop-Location
}
$metadata = $metadataJson | ConvertFrom-Json

# Only what actually ships: the binary's normal (non-dev, non-build) dependency
# closure. Proc-macro and build-time crates run on the packager's machine and are
# not in the artifact, so listing them as shipped components would be a lie.
$workspaceIds = @($metadata.workspace_members)
$nodesById = @{}
foreach ($node in $metadata.resolve.nodes) { $nodesById[$node.id] = $node }
$packagesById = @{}
foreach ($package in $metadata.packages) { $packagesById[$package.id] = $package }

$shippedIds = New-Object System.Collections.Generic.HashSet[string]
$queue = New-Object System.Collections.Generic.Queue[string]
$rootId = ($metadata.packages | Where-Object { $_.name -eq 'tonans-app' }).id
[void]$queue.Enqueue($rootId)
[void]$shippedIds.Add($rootId)

while ($queue.Count -gt 0) {
    $id = $queue.Dequeue()
    if (-not $nodesById.ContainsKey($id)) { continue }
    foreach ($dep in $nodesById[$id].deps) {
        # `dep_kinds` distinguishes normal from build/dev. A proc-macro crate is
        # normal-kind but compiles to a compiler plugin, never into the exe.
        $isNormal = $false
        foreach ($kind in $dep.dep_kinds) { if ($null -eq $kind.kind) { $isNormal = $true } }
        if (-not $isNormal) { continue }
        $depPackage = $packagesById[$dep.pkg]
        if ($depPackage.PSObject.Properties.Name -contains 'targets') {
            $isProcMacro = $false
            foreach ($t in $depPackage.targets) { if ($t.kind -contains 'proc-macro') { $isProcMacro = $true } }
            if ($isProcMacro) { continue }
        }
        if ($shippedIds.Add($dep.pkg)) { [void]$queue.Enqueue($dep.pkg) }
    }
}

$components = @()
$thirdParty = @()
foreach ($id in $shippedIds) {
    $package = $packagesById[$id]
    if ($workspaceIds -contains $id) { continue }
    if (-not $package.license) {
        Fail "shipped crate $($package.name) $($package.version) declares no license"
    }
    $license = switch ($package.license) {
        'MIT/Apache-2.0' { 'MIT OR Apache-2.0' }
        'Unlicense/MIT' { 'Unlicense OR MIT' }
        default { $package.license }
    }
    $components += [pscustomobject]@{
        type      = 'library'
        'bom-ref' = "pkg:cargo/$($package.name)@$($package.version)"
        name      = $package.name
        version   = $package.version
        purl      = "pkg:cargo/$($package.name)@$($package.version)"
        licenses  = @(@{ expression = $license })
        scope     = 'required'
    }
    $thirdParty += [pscustomobject]@{
        Name       = $package.name
        Version    = $package.version
        License    = $license
        Repository = $package.repository
        ManifestDir = (Split-Path -Parent $package.manifest_path)
    }
}
$thirdParty = $thirdParty | Sort-Object Name
$components = $components | Sort-Object name

Step "Shipped third-party crates: $($thirdParty.Count)"

# --- CycloneDX 1.5 -------------------------------------------------------------
# Hand-built rather than shelling out to cargo-cyclonedx: one fewer tool the
# release machine has to have, and the schema surface used here is small.
$bom = [ordered]@{
    bomFormat    = 'CycloneDX'
    specVersion  = '1.5'
    version      = 1
    metadata     = [ordered]@{
        tools      = @(@{ vendor = 'Flintglade'; name = 'packaging/package.ps1'; version = $Version })
        component  = [ordered]@{
            type      = 'application'
            'bom-ref' = "pkg:cargo/tonans@$Version"
            name      = 'Tonans'
            version   = $Version
            purl      = "pkg:cargo/tonans@$Version"
            publisher = 'Flintglade'
            licenses  = @(@{ expression = 'Apache-2.0' })
            description = 'Local-first deterministic microgrid scenario workbench.'
        }
        # No timestamp: it would make two packagings of identical inputs differ,
        # and the whole point of the checksums below is that they can be compared.
        properties = @(
            @{ name = 'tonans:target'; value = 'x86_64-pc-windows-gnullvm' }
            @{ name = 'tonans:signed'; value = 'false' }
        )
    }
    components   = $components
}
$bom | ConvertTo-Json -Depth 12 | Set-Content (Join-Path $StageDir 'SBOM.cdx.json') -Encoding utf8

# --- Third-party notices -------------------------------------------------------
Step 'Writing third-party notices'
$notices = New-Object System.Text.StringBuilder
[void]$notices.AppendLine('# Third-party notices')
[void]$notices.AppendLine()
[void]$notices.AppendLine("Tonans $Version for Windows x64 links the open-source components below.")
[void]$notices.AppendLine('Each is used under its own licence, reproduced or referenced here. The')
[void]$notices.AppendLine('machine-readable form of this list is `SBOM.cdx.json`.')
[void]$notices.AppendLine()
[void]$notices.AppendLine('Build-time-only components (procedural macros, build scripts) are not listed:')
[void]$notices.AppendLine('they run on the machine that produced this binary and are not part of it.')
[void]$notices.AppendLine()
[void]$notices.AppendLine('| Component | Version | Licence |')
[void]$notices.AppendLine('|---|---|---|')
foreach ($crate in $thirdParty) {
    [void]$notices.AppendLine("| $($crate.Name) | $($crate.Version) | $($crate.License) |")
}
[void]$notices.AppendLine()

# The licence texts themselves, deduplicated: nearly every crate here is
# "MIT OR Apache-2.0" and shipping forty identical copies helps nobody.
$seenLicenceText = @{}
foreach ($crate in $thirdParty) {
    $licenceFiles = @(Get-ChildItem -Path $crate.ManifestDir -File -ErrorAction SilentlyContinue | Where-Object {
        $_.Name -like 'LICENSE*' -or
        $_.Name -like 'COPYING*' -or
        $_.Name -like 'NOTICE*' -or
        $_.Name -like 'UNLICENSE*'
    } | Sort-Object Name)
    foreach ($file in $licenceFiles) {
        $text = (Get-Content $file.FullName -Raw).Trim()
        $key = [System.BitConverter]::ToString(
            [System.Security.Cryptography.SHA256]::Create().ComputeHash(
                [System.Text.Encoding]::UTF8.GetBytes($text)))
        if ($seenLicenceText.ContainsKey($key)) { continue }
        $seenLicenceText[$key] = $file.Name
        [void]$notices.AppendLine("## $($file.Name) (as shipped with $($crate.Name) $($crate.Version))")
        [void]$notices.AppendLine()
        [void]$notices.AppendLine('```')
        [void]$notices.AppendLine($text)
        [void]$notices.AppendLine('```')
        [void]$notices.AppendLine()
    }
}
$notices.ToString() | Set-Content (Join-Path $StageDir 'NOTICES.md') -Encoding utf8
Step "Distinct licence texts embedded: $($seenLicenceText.Count)"

# ---------------------------------------------------------------------------
# Internal checksums
# ---------------------------------------------------------------------------

Step 'Writing internal checksums'
$sumsPath = Join-Path $StageDir 'SHA256SUMS.txt'
$lines = @()
foreach ($name in ($Payload | Where-Object { $_ -ne 'SHA256SUMS.txt' } | Sort-Object)) {
    $path = Join-Path $StageDir $name
    if (-not (Test-Path $path)) { Fail "payload file missing from the stage: $name" }
    $hash = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
    # `sha256sum -c` format: digest, two spaces, name. Get-FileHash reads it too.
    $lines += "$hash  $name"
}
# Written without a BOM: `sha256sum -c` treats a BOM as part of the first digest.
[System.IO.File]::WriteAllText($sumsPath, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding $false))

# ---------------------------------------------------------------------------
# Zip, and the external sidecar
# ---------------------------------------------------------------------------

$actual = (Get-ChildItem $StageDir -File | Select-Object -ExpandProperty Name | Sort-Object)
$expected = ($Payload | Sort-Object)
if (Compare-Object $actual $expected) {
    Fail "the stage does not match the payload allowlist:`n  staged:   $($actual -join ', ')`n  expected: $($expected -join ', ')"
}

$zipPath = Join-Path $OutDir "$Stem.zip"
Step "Creating $Stem.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Compress-Archive -Path (Join-Path $StageDir '*') -DestinationPath $zipPath -CompressionLevel Optimal

$zipHash = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
$sidecar = "$zipPath.sha256"
[System.IO.File]::WriteAllText($sidecar, "$zipHash  $Stem.zip`n", (New-Object System.Text.UTF8Encoding $false))

Write-Host ''
Step 'Packaged'
Write-Host "  zip:      $zipPath"
Write-Host "  sha256:   $zipHash"
Write-Host "  sidecar:  $sidecar"
Write-Host "  files:    $($Payload.Count)"
Write-Host ''
Write-Host '  This is a release candidate. It is unsigned, and it has NOT been published.' -ForegroundColor Yellow
Write-Host '  Next: packaging\verify-package.ps1, then packaging\smoke.ps1.' -ForegroundColor Yellow
