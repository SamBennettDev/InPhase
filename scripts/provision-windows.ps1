<#
.SYNOPSIS
  One-shot dev-environment provisioner for the InPhase Windows host build
  (architecture report sec 2.1 stack, sec 19 Phase 0).

.DESCRIPTION
  Installs, if missing:
    * Rust (MSVC toolchain) via rustup
    * Visual Studio 2022 Build Tools (VC++ x64 + Windows 11 SDK)
    * Node.js LTS
    * GStreamer 1.28.6 MSVC x86-64 - COMPLETE install (runtime + devel), via the
      official Inno installer with /TYPE=complete (the winget package is
      runtime-only and has no pkg-config / headers).
  Then sets machine env vars: GSTREAMER_1_0_ROOT_MSVC_X86_64, PKG_CONFIG_PATH,
  and prepends the GStreamer bin dir to PATH.

  Run from an elevated PowerShell. Idempotent.
#>
[CmdletBinding()]
param(
  [string]$GStreamerVersion = "1.28.6"
)
$ErrorActionPreference = "Stop"

function Have($cmd) { $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue) }

if (-not (Have winget)) { throw "winget is required (App Installer)." }

Write-Host "== Rust (MSVC) =="
if (-not (Test-Path "$env:USERPROFILE\.cargo\bin\rustc.exe")) {
  winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements --disable-interactivity
}
& "$env:USERPROFILE\.cargo\bin\rustup.exe" default stable-x86_64-pc-windows-msvc

Write-Host "== VS 2022 Build Tools =="
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$haveVc = (Test-Path $vswhere) -and (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
if (-not $haveVc) {
  winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-source-agreements --accept-package-agreements --disable-interactivity `
    --override "--wait --quiet --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.26100 --includeRecommended"
}

Write-Host "== Node.js LTS =="
if (-not (Have node)) {
  winget install --id OpenJS.NodeJS.LTS -e --accept-source-agreements --accept-package-agreements --disable-interactivity
}

Write-Host "== GStreamer $GStreamerVersion (complete) =="
$gst = "C:\Program Files\gstreamer\1.0\msvc_x86_64"
if (-not (Test-Path "$gst\lib\pkgconfig\gstreamer-1.0.pc")) {
  $url = "https://gstreamer.freedesktop.org/data/pkg/windows/$GStreamerVersion/msvc/gstreamer-1.0-msvc-x86_64-$GStreamerVersion.exe"
  $exe = "$env:TEMP\gstreamer-$GStreamerVersion.exe"
  if (-not (Test-Path $exe) -or (Get-Item $exe).Length -lt 100MB) { curl.exe -sL -o $exe $url }
  Start-Process $exe -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-','/NOCANCEL','/TYPE=complete' -Wait
}
if (-not (Test-Path "$gst\lib\pkgconfig\gstreamer-1.0.pc")) { throw "GStreamer devel install failed." }

Write-Host "== machine env vars =="
[Environment]::SetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", "$gst\", "Machine")
[Environment]::SetEnvironmentVariable("PKG_CONFIG_PATH", "$gst\lib\pkgconfig", "Machine")
$p = [Environment]::GetEnvironmentVariable("Path","Machine")
if ($p -notlike "*$gst\bin*") {
  [Environment]::SetEnvironmentVariable("Path", "$gst\bin;$p", "Machine")
}

Write-Host "`nDone. Open a new shell, then: scripts\build.ps1" -ForegroundColor Green
& pkg-config --modversion gstreamer-1.0 2>$null
