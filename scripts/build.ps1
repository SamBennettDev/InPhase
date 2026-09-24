<#
.SYNOPSIS Build the embedded web client and Windows host.
.PARAMETER Release Build the optimized binary.
.PARAMETER SkipWeb Reuse an existing web build (development only).
#>
[CmdletBinding()]
param([switch]$Release, [switch]$SkipWeb)
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
if ($Release -and $SkipWeb) { throw "Release builds must build the web client from the same source." }

$env:Path = ($env:Path, [Environment]::GetEnvironmentVariable("Path","Machine"),
             [Environment]::GetEnvironmentVariable("Path","User") -join ";")
foreach ($name in @("PKG_CONFIG_PATH", "GSTREAMER_1_0_ROOT_MSVC_X86_64")) {
    if (-not [Environment]::GetEnvironmentVariable($name, "Process")) {
        [Environment]::SetEnvironmentVariable($name, [Environment]::GetEnvironmentVariable($name, "Machine"), "Process")
    }
}
$cargo = (Get-Command cargo.exe -ErrorAction Stop).Source
Push-Location $root
try {
    if (-not $env:INPHASE_BUILD_ID) {
        $env:INPHASE_BUILD_ID = (& git rev-parse HEAD).Trim()
        if ($LASTEXITCODE -ne 0) { throw "Cannot determine build revision." }
    }
    if (-not $SkipWeb) {
        Push-Location (Join-Path $root "web")
        try {
            npm ci --no-audit --no-fund
            if ($LASTEXITCODE -ne 0) { throw "Web dependency installation failed." }
            npm run build
            if ($LASTEXITCODE -ne 0) { throw "Web build failed." }
        } finally { Pop-Location }
    }
    $cargoArgs = @("build", "--locked", "-p", "inphase-host")
    if ($Release) { $cargoArgs += "--release" }
    & $cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "Host build failed; nothing was packaged." }
    $profile = if ($Release) { "release" } else { "debug" }
    Get-Item (Join-Path $root "target\$profile\inphase-host.exe") | Select-Object FullName, Length, LastWriteTime
} finally { Pop-Location }
