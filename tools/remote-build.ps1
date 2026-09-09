<#
.SYNOPSIS
  Remote half of tools/ship.sh - extract, build, verify, install, restart, verify.

.DESCRIPTION
  Runs on the Windows host. Invoked from a *file* rather than an inline ssh
  command on purpose: threading a multi-step script through ssh -> cmd ->
  powershell quoting is what ate paths in earlier deploys.

  Every stage verifies its own output. The last stage asks the *running* host
  what build it is, which is the only check that actually proves a deploy took.

.PARAMETER BuildId
  Stamped into both the Vite bundle and the Rust binary. The running host must
  report this exact value or the script fails.

.PARAMETER Install
  1 = replace the installed exe and restart; 0 = build and verify only.

.OUTPUTS
  Exit 0 success, 3 build failure, 4 verification failure.
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$BuildId,
  [int]$Install = 1
)

$ErrorActionPreference = "Stop"

$Root       = 'C:\Users\sambe\InPhase-wt'
$Tarball    = 'C:\Users\sambe\inphase-ship.tgz'
$InstallExe = 'C:\Program Files\InPhase\InPhaseHost.exe'
$TaskName   = 'InPhaseWTStart'
$StatusUrl  = 'http://127.0.0.1:47800/api/v1/status'

function Say  ($m) { Write-Host "`n== $m" -ForegroundColor Cyan }
function OK   ($m) { Write-Host "   ok $m" -ForegroundColor Green }
function Info ($m) { Write-Host "      $m" -ForegroundColor DarkGray }
function Fail ($code, $m) { Write-Host "`nFAIL $m" -ForegroundColor Red; exit $code }

# --- extract ----------------------------------------------------------------
# Source trees are removed first so a file deleted in git cannot linger here and
# keep compiling. target/ and web/node_modules survive, purely as build cache.
Say "extract $BuildId"
if (-not (Test-Path $Tarball)) { Fail 3 "no source tarball at $Tarball" }
foreach ($d in @('crates', 'tools', 'scripts', 'web\src', 'web\public', 'docs', 'installer')) {
  $p = Join-Path $Root $d
  if (Test-Path $p) { Remove-Item $p -Recurse -Force }
}
if (-not (Test-Path $Root)) { New-Item -ItemType Directory -Path $Root -Force | Out-Null }
& tar.exe -xzf $Tarball -C $Root
if ($LASTEXITCODE -ne 0) { Fail 3 "tar extract failed ($LASTEXITCODE)" }
OK "sources extracted to $Root"

# --- toolchain env ----------------------------------------------------------
# A non-interactive SSH shell does not inherit the machine PATH that carries
# GStreamer, so fold machine+user scope in explicitly (same as scripts/build.ps1).
$env:Path = @($env:Path,
              [Environment]::GetEnvironmentVariable("Path", "Machine"),
              [Environment]::GetEnvironmentVariable("Path", "User")) -join ";"
$env:PKG_CONFIG_PATH                = [Environment]::GetEnvironmentVariable("PKG_CONFIG_PATH", "Machine")
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = [Environment]::GetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", "Machine")
$env:INPHASE_BUILD_ID               = $BuildId
$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
if (-not (Test-Path $cargo)) { Fail 3 "cargo not found at $cargo" }

# --- web build --------------------------------------------------------------
# dist/ is deleted first: if Vite fails, the build must NOT quietly proceed to
# embed a previous bundle. That silent-stale-web case is exactly how the host
# ended up serving a bundle that existed nowhere on disk.
Say "web bundle"
$dist = Join-Path $Root 'web\dist'
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
Push-Location (Join-Path $Root 'web')
if (-not (Test-Path 'node_modules')) {
  & npm ci --no-audit --no-fund
  if ($LASTEXITCODE -ne 0) { Pop-Location; Fail 3 "npm ci failed" }
}
& npm run build
$webRc = $LASTEXITCODE
Pop-Location
if ($webRc -ne 0) { Fail 3 "vite build failed ($webRc)" }

$index = Join-Path $dist 'index.html'
if (-not (Test-Path $index)) { Fail 3 "vite reported success but produced no index.html" }
if ((Get-Content $index -Raw) -match 'Web client bundle not built') {
  Fail 3 "dist/index.html is the build.rs placeholder - the real bundle was not produced"
}
# The bundle must actually carry the id, or the page cannot detect staleness.
$stamped = Get-ChildItem (Join-Path $dist 'assets') -Filter *.js |
           Where-Object { (Get-Content $_.FullName -Raw) -match [regex]::Escape($BuildId) }
if (-not $stamped) { Fail 3 "no JS bundle contains build id $BuildId - is vite.config.ts define: still wired?" }
OK ("bundle stamped: " + ($stamped | ForEach-Object Name) -join ', ')

# --- host build -------------------------------------------------------------
Say "host binary"
Push-Location $Root
& $cargo build --release -p inphase-host
$rc = $LASTEXITCODE
Pop-Location
if ($rc -ne 0) { Fail 3 "cargo build failed ($rc)" }

$built = Join-Path $Root 'target\release\inphase-host.exe'
if (-not (Test-Path $built)) { Fail 3 "cargo reported success but $built is missing" }
$b = Get-Item $built
$builtHash = (Get-FileHash $built -Algorithm SHA256).Hash
OK ("{0:N0} bytes  {1}" -f $b.Length, $b.LastWriteTime)
Info "sha256 $builtHash"
# Guard against shipping a binary cargo decided it did not need to relink.
if ($b.LastWriteTime -lt (Get-Date).AddMinutes(-30)) {
  Fail 3 "built exe is older than 30 min - cargo did not relink; build id would be wrong"
}

if ($Install -ne 1) { Say "done (--no-install)"; exit 0 }

# --- install ----------------------------------------------------------------
Say "install"
$prev = if (Test-Path $InstallExe) { (Get-FileHash $InstallExe -Algorithm SHA256).Hash } else { "(none)" }
Info "replacing $prev"
Get-Process InPhaseHost -ErrorAction SilentlyContinue | ForEach-Object {
  Info "stopping pid $($_.Id)"
  Stop-Process -Id $_.Id -Force
}
Start-Sleep -Milliseconds 700
try {
  Copy-Item $built $InstallExe -Force
} catch {
  Fail 3 "could not write $InstallExe - $($_.Exception.Message)"
}
$now = (Get-FileHash $InstallExe -Algorithm SHA256).Hash
if ($now -ne $builtHash) { Fail 4 "installed exe hash $now != built $builtHash" }
OK "installed, hash matches the build"

# --- restart ----------------------------------------------------------------
# Launch via the scheduled task, not directly: this SSH session is not the
# interactive console session, and DXGI desktop duplication only captures from
# one. The task runs as sambe/Interactive, which lands in session 1.
Say "restart"
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $task) { Fail 4 "scheduled task $TaskName is missing - cannot start into the interactive session" }
$vbs = ($task.Actions | ForEach-Object { $_.Arguments }) -join ' '
if ($vbs -notmatch [regex]::Escape($Root)) {
  Fail 4 "task $TaskName launches '$vbs', which is not under the canonical dir $Root"
}
& schtasks.exe /run /tn $TaskName | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 4 "schtasks /run failed ($LASTEXITCODE)" }
OK "task $TaskName triggered"

# --- verify the *running* host ---------------------------------------------
# The one check that proves the deploy took. Everything above can pass while the
# live host is still the old binary.
Say "verify running host"
$deadline = (Get-Date).AddSeconds(45)
$seen = $null
while ((Get-Date) -lt $deadline) {
  Start-Sleep -Seconds 2
  $body = & curl.exe -s -k --max-time 4 $StatusUrl 2>$null
  if (-not $body) { continue }
  try { $seen = ($body | ConvertFrom-Json).build_id } catch { continue }
  if ($seen -eq $BuildId) {
    $p = Get-Process InPhaseHost -ErrorAction SilentlyContinue
    OK "host reports build_id $seen (pid $($p.Id -join ','))"
    if (@($p).Count -gt 1) {
      Write-Host "   WARN more than one InPhaseHost process is running" -ForegroundColor Yellow
    }
    Say "deploy verified"
    exit 0
  }
}
if ($seen) { Fail 4 "host is up but reports build_id '$seen', expected '$BuildId' - the old binary is still running" }
Fail 4 "host never answered $StatusUrl within 45s - check C:\ProgramData\InPhase\host.log"
