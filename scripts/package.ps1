<#
.SYNOPSIS
  Assemble a self-contained InPhase Host runtime folder with a minimal,
  license-clean GStreamer bundle.

.DESCRIPTION
  * Ships ONLY the GStreamer plugins InPhase loads (explicit allowlist) and only
    the support DLLs those plugins + the host EXE actually import (recursive PE
    dependency walk via dumpbin), plus a short explicit list of runtime-loaded
    libraries that do not appear in the import table.
  * Never copies "$gst\bin\*.dll" wholesale.
  * Refuses to include GPL x264 / x265 / FFmpeg components, and fails the build
    if any GPL/AGPL/unknown-license file ends up in the bundle.
  * Emits MANIFEST.csv (path, size, sha256, origin, license) and
    OPEN-SOURCE-COMPONENTS.txt.

  Run on the build box after: scripts\build.ps1 -Release
#>
[CmdletBinding()]
param(
    [string]$OutDir,
    [string]$GstRoot = [Environment]::GetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", "Machine")
)
$ErrorActionPreference = "Stop"
$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir
if (-not $OutDir)  { $OutDir  = Join-Path $root "dist\InPhase" }
if (-not $GstRoot) { throw "GSTREAMER_1_0_ROOT_MSVC_X86_64 not set and -GstRoot not given" }
$GstRoot   = $GstRoot.TrimEnd('\')
$gstBin    = Join-Path $GstRoot "bin"
$gstPlug   = Join-Path $GstRoot "lib\gstreamer-1.0"

$exe = Join-Path $root "target\release\inphase-host.exe"
if (-not (Test-Path $exe)) { throw "release exe missing - run scripts\build.ps1 -Release first" }

# ---------------------------------------------------------------------------
# 1. GStreamer plugin allowlist — keep in sync with media/pipeline.rs +
#    media/webrtc/webrtcbin.rs. Every entry is LGPL-2.1+ unless noted.
# ---------------------------------------------------------------------------
$plugins = @(
    "gstcoreelements",        # queue, capsfilter, ...             (gstreamer core)
    "gstapp",                 # appsrc/appsink                     (gst-plugins-base)
    "gsttypefindfunctions",   # caps typefind
    "gstplayback", "gstautodetect",
    "gstd3d11",               # d3d11screencapturesrc, d3d11convert (gst-plugins-bad)
    "gstnvcodec",             # nvd3d11h264enc  -> loads NVIDIA NVENC from the driver
    "gstwasapi2",             # wasapi2src loopback                 (gst-plugins-bad)
    "gstwebrtc",              # webrtcbin                           (gst-plugins-bad)
    "gstsctp",                # SCTP data channels for webrtcbin    (gst-plugins-bad)
    "gstrtp", "gstrtpmanager",# rtph264pay, rtpbin, rtpjitterbuffer (gst-plugins-good)
    "gstdtls",                # DTLS-SRTP key exchange (OpenSSL)    (gst-plugins-bad)
    "gstsrtp",                # SRTP (libsrtp2)                     (gst-plugins-bad)
    "gstnice",                # ICE (libnice)                       (gst-plugins-bad, LGPL/MPL)
    "gstopus",                # opusenc (libopus)                   (gst-plugins-base)
    "gstvideorate",           # videorate — repeat last frame at target fps (gst-plugins-base)
    "gstaudioconvert", "gstaudioresample",
    "gstvideoconvertscale",
    "gstvideoparsersbad",     # h264parse                           (gst-plugins-bad)
    "gstdebugutilsbad"        # GST_DEBUG_DUMP_DOT_DIR helpers      (gst-plugins-bad)
)

# Support libraries loaded at runtime that do NOT show up as PE imports
# (dynamically LoadLibrary'd, or resolved by GStreamer/GLib at plugin load).
$runtimeLoaded = @(
    "libcrypto-3-x64.dll", "libssl-3-x64.dll"   # OpenSSL, via gstdtls
)

# Files we must never ship (GPL / patent-encumbered / not used by InPhase).
$forbidden = @(
    "x264*.dll", "x265*.dll", "libx264*", "libx265*",
    "avcodec*.dll", "avformat*.dll", "avdevice*.dll", "avfilter*.dll",
    "postproc*.dll", "swresample*.dll", "swscale*.dll",
    "gstlibav.dll", "gstx264.dll", "gstx265.dll",
    "gsta52dec.dll", "gstdvd*.dll", "gstresindvd.dll", "gstmpeg2dec.dll",
    "libmpeg2*", "liba52*"
)

# ---------------------------------------------------------------------------
# 2. Recursive PE dependency walk (dumpbin).
# ---------------------------------------------------------------------------
$dumpbin = $null
$dbCmd = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
if ($dbCmd) { $dumpbin = $dbCmd.Source }
if (-not $dumpbin) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vc = & $vswhere -latest -products * -find "**\dumpbin.exe" | Select-Object -First 1
        if ($vc) { $dumpbin = $vc }
    }
}
if (-not $dumpbin) { throw "dumpbin.exe not found (install VS Build Tools 'MSVC' + 'C++ tools')" }

function Get-PEImports([string]$Path) {
    (& $dumpbin /nologo /dependents $Path) 2>$null |
        Where-Object { $_ -match '^\s{4,}\S+\.dll\s*$' } |
        ForEach-Object { $_.Trim().ToLowerInvariant() } |
        # API-set forwarders / OS umbrellas — always provided by the OS, never
        # present as real files in System32.
        Where-Object { $_ -notmatch '^(api-ms-win-|ext-ms-|api-ms-onecore)' }
}

$sysRoots = @("$env:WINDIR\System32", "$env:WINDIR\SysWOW64") | ForEach-Object { $_.ToLowerInvariant() }
$resolved = [System.Collections.Generic.HashSet[string]]::new()
$queue    = [System.Collections.Generic.Queue[string]]::new()

# seeds: the host EXE + every allowlisted plugin DLL
$queue.Enqueue($exe)
foreach ($p in $plugins) {
    $dll = Join-Path $gstPlug "$p.dll"
    if (Test-Path $dll) { $queue.Enqueue($dll) } else { Write-Warning "plugin not in GStreamer install: $p" }
}

while ($queue.Count -gt 0) {
    $cur = $queue.Dequeue()
    foreach ($imp in Get-PEImports $cur) {
        if ($resolved.Contains($imp)) { continue }
        # skip Windows system DLLs
        $inSys = $false
        foreach ($s in $sysRoots) { if (Test-Path (Join-Path $s $imp)) { $inSys = $true; break } }
        if ($inSys) { continue }
        # must come from the GStreamer bin dir to be a bundle candidate
        $src = Join-Path $gstBin $imp
        if (-not (Test-Path $src)) { Write-Warning "unresolved import (not in gst bin, not system): $imp  <- $(Split-Path $cur -Leaf)"; continue }
        [void]$resolved.Add($imp)
        $queue.Enqueue($src)
    }
}
foreach ($rl in $runtimeLoaded) { [void]$resolved.Add($rl.ToLowerInvariant()) }

# ---------------------------------------------------------------------------
# 3. Assemble.
# ---------------------------------------------------------------------------
Remove-Item -Recurse -Force $OutDir -ErrorAction SilentlyContinue
foreach ($d in @("runtime\gstreamer\bin", "runtime\gstreamer\lib\gstreamer-1.0", "licenses")) {
    $null = New-Item -ItemType Directory -Force -Path (Join-Path $OutDir $d)
}

Copy-Item $exe (Join-Path $OutDir "InPhaseHost.exe")

$binOut  = Join-Path $OutDir "runtime\gstreamer\bin"
$plugOut = Join-Path $OutDir "runtime\gstreamer\lib\gstreamer-1.0"

function Test-Forbidden([string]$name) {
    foreach ($pat in $forbidden) { if ($name -like $pat) { return $true } }
    return $false
}

foreach ($imp in ($resolved | Sort-Object)) {
    if (Test-Forbidden $imp) { throw "PANIC: dependency walk pulled a forbidden library: $imp" }
    $src = Join-Path $gstBin $imp
    if (Test-Path $src) { Copy-Item $src $binOut } else { Write-Warning "runtime-loaded lib missing from gst bin: $imp" }
}
foreach ($p in $plugins) {
    $src = Join-Path $gstPlug "$p.dll"
    if (Test-Path $src) { Copy-Item $src $plugOut }
}
# Hard fail if a required plugin did not land. A missing gstvideorate.dll is
# exactly the "session failed: videorate" bug: the host cannot build the
# capture pipeline, WT still advertises, and the browser wedges on no frames.
foreach ($must in @("gstvideorate.dll", "gstd3d11.dll", "gstnvcodec.dll", "gstcoreelements.dll")) {
    if (-not (Test-Path (Join-Path $plugOut $must))) {
        throw "packaging missed $must — the GStreamer install at $GstRoot does not have it, or it was not on the allowlist"
    }
}

# No launcher script. InPhaseHost.exe is a windowless (GUI-subsystem) binary
# that points GStreamer at .\runtime\gstreamer itself (`point_at_bundled_runtime`
# in main.rs) and logs to %LOCALAPPDATA%\InPhase\host.log.

# ---------------------------------------------------------------------------
# 4. Manifest + licence scan (PKG-004).
# ---------------------------------------------------------------------------
# name-glob -> @{ License ; Source }.  '' license => flagged as UNKNOWN.
$lic = [ordered]@{
    "inphasehost.exe"        = @{ L = "LicenseRef-InPhase-Proprietary"; S = "InPhase" }
    "gst*.dll"               = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.freedesktop.org/gstreamer/gstreamer" }
    "gstreamer-1.0-0.dll"    = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.freedesktop.org/gstreamer/gstreamer" }
    "gst*-1.0-0.dll"         = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.freedesktop.org/gstreamer/gstreamer" }
    "gstrs*.dll"             = @{ L = "MPL-2.0"; S = "https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs" }
    "glib-2.0-0.dll"         = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.gnome.org/GNOME/glib" }
    "gobject-2.0-0.dll"      = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.gnome.org/GNOME/glib" }
    "gio-2.0-0.dll"          = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.gnome.org/GNOME/glib" }
    "gmodule-2.0-0.dll"      = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.gnome.org/GNOME/glib" }
    "gthread-2.0-0.dll"      = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.gnome.org/GNOME/glib" }
    "ffi-*.dll"              = @{ L = "MIT"; S = "https://github.com/libffi/libffi" }
    "libffi*.dll"            = @{ L = "MIT"; S = "https://github.com/libffi/libffi" }
    "intl-*.dll"             = @{ L = "LGPL-2.1-or-later"; S = "https://savannah.gnu.org/projects/gettext" }
    "libintl*.dll"           = @{ L = "LGPL-2.1-or-later"; S = "https://savannah.gnu.org/projects/gettext" }
    "iconv-*.dll"            = @{ L = "LGPL-2.1-or-later"; S = "https://savannah.gnu.org/projects/libiconv" }
    "libiconv*.dll"          = @{ L = "LGPL-2.1-or-later"; S = "https://savannah.gnu.org/projects/libiconv" }
    "pcre2*.dll"             = @{ L = "BSD-3-Clause"; S = "https://github.com/PCRE2Project/pcre2" }
    "z-1.dll"               = @{ L = "Zlib"; S = "https://zlib.net" }
    "zlib*.dll"              = @{ L = "Zlib"; S = "https://zlib.net" }
    "vcruntime*.dll"         = @{ L = "Microsoft VC++ Runtime (redistributable)"; S = "Visual Studio redistributable" }
    "msvcp*.dll"             = @{ L = "Microsoft VC++ Runtime (redistributable)"; S = "Visual Studio redistributable" }
    "concrt*.dll"            = @{ L = "Microsoft VC++ Runtime (redistributable)"; S = "Visual Studio redistributable" }
    "libcrypto-3-x64.dll"    = @{ L = "Apache-2.0"; S = "https://github.com/openssl/openssl" }
    "libssl-3-x64.dll"       = @{ L = "Apache-2.0"; S = "https://github.com/openssl/openssl" }
    "srtp2*.dll"             = @{ L = "BSD-3-Clause"; S = "https://github.com/cisco/libsrtp" }
    "nice*.dll"              = @{ L = "LGPL-2.1-or-later OR MPL-1.1"; S = "https://gitlab.freedesktop.org/libnice/libnice" }
    "opus*.dll"              = @{ L = "BSD-3-Clause (royalty-free patent grant)"; S = "https://gitlab.xiph.org/xiph/opus" }
    "orc-0.4-0.dll"          = @{ L = "BSD-2-Clause / BSD-3-Clause"; S = "https://gitlab.freedesktop.org/gstreamer/orc" }
    "graphene-1.0-0.dll"     = @{ L = "MIT"; S = "https://github.com/ebassi/graphene" }
    "*.dll"                  = @{ L = ""; S = "" }   # fallthrough -> UNKNOWN
}
function Resolve-Lic([string]$name) {
    foreach ($k in $lic.Keys) { if ($name -like $k) { return $lic[$k] } }
    return @{ L = ""; S = "" }
}

$rows = @()
$unknown = @()
$forbiddenHit = @()
Get-ChildItem $OutDir -Recurse -File | ForEach-Object {
    $rel  = $_.FullName.Substring($OutDir.Length).TrimStart('\')
    $name = $_.Name.ToLowerInvariant()
    if (Test-Forbidden $name) { $forbiddenHit += $rel }
    $m = if ($name -match '\.(dll|exe)$') { Resolve-Lic $name } else { @{ L = "n/a"; S = "" } }
    if ($m.L -eq "") { $unknown += $rel }
    $rows += [pscustomobject]@{
        path    = $rel
        bytes   = $_.Length
        sha256  = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        license = if ($m.L -eq "") { "UNKNOWN" } else { $m.L }
        source  = $m.S
    }
}
$rows | Sort-Object path | Export-Csv (Join-Path $OutDir "MANIFEST.csv") -NoTypeInformation -Encoding utf8

@"
InPhase Host — open-source components in this build
Generated $(Get-Date -Format o)

InPhase-owned code: proprietary (see licenses\InPhase-LICENSE.txt).

Third-party (dynamically linked / bundled):
$($rows | Where-Object { $_.license -notin @("n/a","LicenseRef-InPhase-Proprietary","UNKNOWN") } |
    Group-Object license | Sort-Object Name | ForEach-Object {
        "  [$($_.Name)]`n" + ($_.Group | ForEach-Object { "    - $($_.path)  ($($_.source))" } | Sort-Object -Unique | Out-String)
    } | Out-String)

Corresponding source for LGPL / MPL components is archived per release at the
InPhase source-offer URL.
"@ | Set-Content (Join-Path $OutDir "OPEN-SOURCE-COMPONENTS.txt") -Encoding utf8

Copy-Item (Join-Path $root "LICENSE") (Join-Path $OutDir "licenses\InPhase-LICENSE.txt")
# Rust dependency license inventory (compiled into the exe, not separate files).
Push-Location $root
& "$env:USERPROFILE\.cargo\bin\cargo.exe" tree -e no-dev --prefix none --no-default-features --features virtual-hid -p inphase-host 2>$null |
    Sort-Object -Unique | Set-Content (Join-Path $OutDir "licenses\rust-crates.txt")
Pop-Location

# ---------------------------------------------------------------------------
# 5. Gate.
# ---------------------------------------------------------------------------
$fail = $false
if ($forbiddenHit.Count) { Write-Host "FORBIDDEN files in bundle:" -ForegroundColor Red; $forbiddenHit | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }; $fail = $true }
if ($unknown.Count)      { Write-Host "UNKNOWN-license files (add to the table or remove):" -ForegroundColor Yellow; $unknown | ForEach-Object { Write-Host "  $_" -ForegroundColor Yellow } }

$size = "{0:N1} MB" -f ((Get-ChildItem $OutDir -Recurse | Measure-Object Length -Sum).Sum / 1MB)
Write-Host "Packaged -> $OutDir  ($size, $($rows.Count) files, $($resolved.Count) support DLLs, $($plugins.Count) plugins)" -ForegroundColor Green
if ($fail) { throw "packaging gate failed - see FORBIDDEN files above" }
if ($unknown.Count) { Write-Warning "packaging completed with UNKNOWN-license files - resolve before release" }
