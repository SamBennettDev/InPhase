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
    if an unreviewed media plugin or unknown-license binary enters the bundle.
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
$PSNativeCommandUseErrorActionPreference = $true
$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $scriptDir
if (-not $OutDir)  { $OutDir  = Join-Path $root "dist\InPhase" }
if (-not $GstRoot) { $GstRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 }
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
    "gstamfcodec",            # AMD AMF hardware encoder
    "gstqsv",                 # Intel Quick Sync hardware encoder
    "gstmediafoundation",     # Windows Media Foundation fallback
    "gstwasapi2",             # wasapi2src loopback                 (gst-plugins-bad)
    "gstwebrtc",              # webrtcbin                           (gst-plugins-bad)
    "gstsctp",                # SCTP data channels for webrtcbin    (gst-plugins-bad)
    "gstrtp", "gstrtpmanager",# rtph264pay, rtpbin, rtpjitterbuffer (gst-plugins-good)
    "gstdtls",                # DTLS-SRTP key exchange (OpenSSL)    (gst-plugins-bad)
    "gstsrtp",                # SRTP (libsrtp2)                     (gst-plugins-bad)
    "gstnice",                # ICE (libnice)                       (gst-plugins-bad, LGPL/MPL)
    "gstopus",                # opusenc (libopus)                   (gst-plugins-base)
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

# Media components excluded from this hardware-encoder-only distribution.
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
    $imports = & $dumpbin /nologo /dependents $Path
    if ($LASTEXITCODE -ne 0) { throw "Cannot inspect PE imports: $Path" }
    $imports |
        Where-Object { $_ -match '^\s{4,}\S+\.dll\s*$' } |
        ForEach-Object { $_.Trim().ToLowerInvariant() } |
        # API-set forwarders / OS umbrellas — always provided by the OS, never
        # present as real files in System32.
        Where-Object { $_ -notmatch '^(api-ms-win-|ext-ms-|api-ms-onecore)' }
}

$sysRoots = @("$env:WINDIR\System32", "$env:WINDIR\SysWOW64") | ForEach-Object { $_.ToLowerInvariant() }
# VC runtimes on the build machine are not Windows system components.
# Resolve them from the redistributable directory instead of silently omitting them.
$redistRoots = @()
$vswhere = Join-Path ([Environment]::GetFolderPath("ProgramFilesX86")) "Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $vsRoot = & $vswhere -latest -products * -property installationPath
    if ($LASTEXITCODE -ne 0) { throw "vswhere failed." }
    if ($vsRoot) {
        $redistRoots = @(Get-ChildItem "$vsRoot\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT" -Directory -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending | ForEach-Object { $_.FullName })
    }
}
$resolved = [System.Collections.Generic.HashSet[string]]::new()
$sources = @{}
$queue    = [System.Collections.Generic.Queue[string]]::new()

# seeds: the host EXE + every allowlisted plugin DLL
$queue.Enqueue($exe)
$inspector = Join-Path $gstBin "gst-inspect-1.0.exe"
if (-not (Test-Path $inspector)) { throw "GStreamer inspection tool missing." }
$queue.Enqueue($inspector)
foreach ($rl in $runtimeLoaded) {
    $src = Join-Path $gstBin $rl
    if (-not (Test-Path $src)) { throw "Required runtime library missing: $rl" }
    [void]$resolved.Add($rl.ToLowerInvariant())
    $sources[$rl.ToLowerInvariant()] = $src
    $queue.Enqueue($src)
}
foreach ($p in $plugins) {
    $dll = Join-Path $gstPlug "$p.dll"
    if (Test-Path $dll) { $queue.Enqueue($dll) } else { throw "Required plugin missing from GStreamer install: $p" }
}

while ($queue.Count -gt 0) {
    $cur = $queue.Dequeue()
    foreach ($imp in Get-PEImports $cur) {
        if ($resolved.Contains($imp)) { continue }
        $src = Join-Path $gstBin $imp
        $isVcRuntime = $imp -match '^(vcruntime|msvcp|concrt)'
        if (-not (Test-Path $src) -and $isVcRuntime) {
            $src = $null
            foreach ($redist in $redistRoots) {
                $candidate = Join-Path $redist $imp
                if (Test-Path $candidate) { $src = $candidate; break }
            }
            if (-not $src) { throw "VC++ redistributable DLL missing: $imp" }
        }
        if ($src -and (Test-Path $src)) {
            [void]$resolved.Add($imp)
            $sources[$imp] = $src
            $queue.Enqueue($src)
            continue
        }
        $inSys = $false
        foreach ($systemRoot in $sysRoots) {
            if (Test-Path (Join-Path $systemRoot $imp)) { $inSys = $true; break }
        }
        if (-not $inSys) { throw "Unresolved DLL $imp imported by $cur" }
    }
}

# ---------------------------------------------------------------------------
# 3. Assemble.
# ---------------------------------------------------------------------------
Remove-Item -Recurse -Force $OutDir -ErrorAction SilentlyContinue
foreach ($d in @("runtime\gstreamer\lib\gstreamer-1.0", "licenses")) {
    $null = New-Item -ItemType Directory -Force -Path (Join-Path $OutDir $d)
}

Copy-Item $exe (Join-Path $OutDir "InPhaseHost.exe")
Copy-Item $inspector $OutDir

# Windows resolves linked DLLs before main(): keep dependencies beside the EXE.
$binOut  = $OutDir
$plugOut = Join-Path $OutDir "runtime\gstreamer\lib\gstreamer-1.0"

function Test-Forbidden([string]$name) {
    foreach ($pat in $forbidden) { if ($name -like $pat) { return $true } }
    return $false
}

foreach ($imp in ($resolved | Sort-Object)) {
    if (Test-Forbidden $imp) { throw "PANIC: dependency walk pulled a forbidden library: $imp" }
    Copy-Item $sources[$imp] $binOut
}
foreach ($p in $plugins) {
    $src = Join-Path $gstPlug "$p.dll"
    if (Test-Path $src) { Copy-Item $src $plugOut }
}

# No launcher script. InPhaseHost.exe is a windowless (GUI-subsystem) binary
# that points GStreamer at .\runtime\gstreamer itself (`point_at_bundled_runtime`
# in main.rs) and logs to %LOCALAPPDATA%\InPhase\host.log.

# ---------------------------------------------------------------------------
# 4. Manifest + licence scan (PKG-004).
# ---------------------------------------------------------------------------
# name-glob -> @{ License ; Source }.  '' license => flagged as UNKNOWN.
$lic = [ordered]@{
    "inphasehost.exe"        = @{ L = "GPL-3.0-or-later"; S = "https://github.com/SamBennettDev/InPhase" }
    "gst-inspect-1.0.exe"   = @{ L = "LGPL-2.1-or-later"; S = "https://gitlab.freedesktop.org/gstreamer/gstreamer" }
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

InPhase-owned code: GPL-3.0-or-later (see licenses\InPhase-LICENSE.txt).

Third-party (dynamically linked / bundled):
$($rows | Where-Object { $_.license -notin @("n/a","GPL-3.0-or-later","UNKNOWN") } |
    Group-Object license | Sort-Object Name | ForEach-Object {
        "  [$($_.Name)]`n" + ($_.Group | ForEach-Object { "    - $($_.path)  ($($_.source))" } | Sort-Object -Unique | Out-String)
    } | Out-String)

This is a binary component inventory, not a substitute for corresponding source.
The public release must include matching source and third-party notices as
specified in docs/RELEASING.md. This candidate is not a completed source offer.
"@ | Set-Content (Join-Path $OutDir "OPEN-SOURCE-COMPONENTS.txt") -Encoding utf8

Copy-Item (Join-Path $root "LICENSE") (Join-Path $OutDir "licenses\InPhase-LICENSE.txt")
Copy-Item (Join-Path $root "NOTICE") (Join-Path $OutDir "licenses\NOTICE.txt")
& (Join-Path $PSScriptRoot "collect-notices.ps1") -OutDir (Join-Path $OutDir "licenses")
# Preserve notices supplied by the exact GStreamer distribution used to build.
$gstLicenses = Join-Path $GstRoot "share\licenses"
if (Test-Path $gstLicenses) {
    Copy-Item $gstLicenses (Join-Path $OutDir "licenses\gstreamer-distribution") -Recurse
} else {
    throw "GStreamer distribution license notices are missing."
}
# Rust dependency license inventory (compiled into the exe, not separate files).
Push-Location $root
& cargo tree --locked -e no-dev --prefix none -p inphase-host |
    Sort-Object -Unique | Set-Content (Join-Path $OutDir "licenses\rust-crates.txt")
if ($LASTEXITCODE -ne 0) { throw "Rust dependency inventory failed." }
Pop-Location

# Include notices and inventories in the checksum manifest. The manifest excludes
# itself; the component table above contains the binary license classifications.
$binaryRows = @{}
foreach ($row in $rows) { $binaryRows[$row.path] = $row }
$rows = @(Get-ChildItem $OutDir -Recurse -File | Where-Object { $_.Name -ne "MANIFEST.csv" } | ForEach-Object {
    $relative = $_.FullName.Substring($OutDir.Length).TrimStart('\')
    if ($binaryRows.ContainsKey($relative)) { $binaryRows[$relative] }
    else {
        [pscustomobject]@{
            path = $relative
            bytes = $_.Length
            sha256 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            license = "n/a"
            source = ""
        }
    }
})
$rows | Sort-Object path | Export-Csv (Join-Path $OutDir "MANIFEST.csv") -NoTypeInformation -Encoding utf8

# ---------------------------------------------------------------------------
# 5. Gate.
# ---------------------------------------------------------------------------
$fail = $false
if ($forbiddenHit.Count) { Write-Host "FORBIDDEN files in bundle:" -ForegroundColor Red; $forbiddenHit | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }; $fail = $true }
if ($unknown.Count)      { $fail = $true; Write-Host "UNKNOWN-license files (add to the table or remove):" -ForegroundColor Yellow; $unknown | ForEach-Object { Write-Host "  $_" -ForegroundColor Yellow } }

$size = "{0:N1} MB" -f ((Get-ChildItem $OutDir -Recurse | Measure-Object Length -Sum).Sum / 1MB)
Write-Host "Packaged -> $OutDir  ($size, $($rows.Count) files, $($resolved.Count) support DLLs, $($plugins.Count) plugins)" -ForegroundColor Green
if ($fail) { throw "packaging gate failed - resolve forbidden or unknown-license files" }
