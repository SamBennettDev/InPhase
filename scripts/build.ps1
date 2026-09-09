<#
.SYNOPSIS  Build the InPhase host + web client.
.DESCRIPTION
  1. Builds the web client (Vite) -> web/dist/
  2. Builds the Rust host -> target/<profile>/inphase-host.exe
  Refreshes GStreamer / PATH env from the machine scope so it works in a fresh
  non-interactive shell.
.PARAMETER Release   Build the optimised release binary.
.PARAMETER SkipWeb   Skip the web build (reuse the existing web/dist/).
#>
[CmdletBinding()]
param([switch]$Release, [switch]$SkipWeb)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# Fold the machine + user PATH into whatever this shell already has, so a fresh
# non-interactive shell still finds GStreamer without dropping the caller's PATH.
$env:Path = ($env:Path, [Environment]::GetEnvironmentVariable("Path","Machine"),
             [Environment]::GetEnvironmentVariable("Path","User") -join ";")
$env:PKG_CONFIG_PATH = [Environment]::GetEnvironmentVariable("PKG_CONFIG_PATH","Machine")
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = [Environment]::GetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64","Machine")
$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"

if (-not $SkipWeb) {
  Write-Host "== web client ==" -ForegroundColor Cyan
  Push-Location "$root\web"
  if (-not (Test-Path node_modules)) { npm ci --no-audit --no-fund }
  npm run build
  Pop-Location
}

Write-Host "== host ==" -ForegroundColor Cyan
Push-Location $root
$args = @("build", "-p", "inphase-host")
if ($Release) { $args += "--release" }
& $cargo @args
$profile = if ($Release) { "release" } else { "debug" }
$exe = "$root\target\$profile\inphase-host.exe"
Pop-Location

Write-Host "`nBuilt: $exe" -ForegroundColor Green
Get-Item $exe | Select-Object Length, LastWriteTime
