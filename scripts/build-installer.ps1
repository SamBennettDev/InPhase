<#
.SYNOPSIS
  Build the one-download InPhase Host installer.

.DESCRIPTION
  1. scripts\build.ps1 -Release        -> target\release\inphase-host.exe + web\dist
  2. (optional) sign the host exe
  3. scripts\package.ps1               -> dist\InPhase\  (private runtime, manifest)
  4. ISCC installer\inphase.iss        -> dist\InPhaseSetup.exe
  5. (optional) sign dist\InPhaseSetup.exe

.PARAMETER SkipBuild   Reuse the existing release exe / web build.
.PARAMETER SignCmd
  Command template used to Authenticode-sign each artifact. Use $f for the file
  path, e.g.:
    -SignCmd 'signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /sha1 <THUMBPRINT> $f'
  Omit for an unsigned candidate. Public release review must disclose signing status.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$SignCmd
)
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true
$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir
$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches.Groups[1].Value
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "Cargo.toml must contain a valid three-part installer version." }

function Sign([string]$Path) {
    if (-not $SignCmd) { Write-Warning "UNSIGNED: $Path  (pass -SignCmd for a release build)"; return }
    $cmd = $SignCmd.Replace('$f', '"' + $Path + '"')
    Write-Host "Signing $Path" -ForegroundColor DarkGray
    cmd /c $cmd
    if ($LASTEXITCODE -ne 0) { throw "signing failed for $Path" }
}

if (-not $SkipBuild) {
    Write-Host "== 1. build (release) ==" -ForegroundColor Cyan
    & (Join-Path $scriptDir "build.ps1") -Release
}
$exe = Join-Path $root "target\release\inphase-host.exe"
if (-not (Test-Path $exe)) { throw "release exe missing" }

Write-Host "== 2. sign host exe ==" -ForegroundColor Cyan
Sign $exe

Write-Host "== 3. package private runtime ==" -ForegroundColor Cyan
& (Join-Path $scriptDir "package.ps1")

Write-Host "== 4. compile installer (ISCC) ==" -ForegroundColor Cyan
$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue)
if ($iscc) { $iscc = $iscc.Source }
if (-not $iscc) {
    $cands = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $cands) { if (Test-Path $p) { $iscc = $p; break } }
}
if (-not $iscc) { throw "ISCC.exe not found - install Inno Setup 6 (winget install JRSoftware.InnoSetup)" }

$isccArgs = @("/Qp", "/DAppVersion=$version", (Join-Path $root "installer\inphase.iss"))
if ($SignCmd) {
    # Inno's SignTool= expects a named tool; register an inline one.
    $isccArgs = @("/Sbyname=`"$($SignCmd.Replace('$f','$f'))`"", "/DSignTool=byname") + $isccArgs
}
& $iscc @isccArgs
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }

$setup = Join-Path $root "dist\InPhaseSetup.exe"
if (-not (Test-Path $setup)) { throw "installer not produced" }

Write-Host "== 5. sign installer ==" -ForegroundColor Cyan
Sign $setup

"{0:N1} MB" -f ((Get-Item $setup).Length / 1MB) | ForEach-Object {
    Write-Host "`nBuilt: $setup  ($_)  version $version" -ForegroundColor Green
}
