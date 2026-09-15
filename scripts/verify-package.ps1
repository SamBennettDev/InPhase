<#
.SYNOPSIS Verify packaged DLLs and plugins without the developer's GStreamer PATH.
.DESCRIPTION This checks startup and plugin registration, not GPU capture or streaming.
#>
[CmdletBinding()]
param([string]$PackageDir = (Join-Path (Split-Path -Parent $PSScriptRoot) "dist\InPhase"))
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true
$PackageDir = (Resolve-Path $PackageDir).Path
$names = @("Path", "GSTREAMER_1_0_ROOT_MSVC_X86_64", "PKG_CONFIG_PATH", "GST_PLUGIN_PATH",
           "GST_PLUGIN_SYSTEM_PATH", "GST_REGISTRY", "GST_REGISTRY_FORK")
$previous = @{}
foreach ($name in $names) { $previous[$name] = [Environment]::GetEnvironmentVariable($name, "Process") }
$temp = Join-Path ([IO.Path]::GetTempPath()) ("inphase-check-" + [Guid]::NewGuid())
New-Item -ItemType Directory $temp | Out-Null
try {
    $env:Path = "$env:WINDIR\System32;$env:WINDIR"
    $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = ""
    $env:PKG_CONFIG_PATH = ""
    $env:GST_PLUGIN_PATH = Join-Path $PackageDir "runtime\gstreamer\lib\gstreamer-1.0"
    $env:GST_PLUGIN_SYSTEM_PATH = ""
    $env:GST_REGISTRY = Join-Path $temp "registry.bin"
    $env:GST_REGISTRY_FORK = "no"
    Push-Location $temp
    try {
        $proc = Start-Process (Join-Path $PackageDir "InPhaseHost.exe") -ArgumentList "--version" -PassThru -RedirectStandardOutput "$temp\version.txt" -RedirectStandardError "$temp\host-error.txt"
        if (-not $proc.WaitForExit(15000)) { $proc.Kill(); throw "Packaged host did not exit within 15 seconds." }
        if ($proc.ExitCode -ne 0) { throw "Packaged host failed DLL/startup check (exit $($proc.ExitCode))." }
        $inspector = Join-Path $PackageDir "gst-inspect-1.0.exe"
        foreach ($element in @("queue", "appsink", "d3d11screencapturesrc", "d3d11convert",
            "videorate", "h264parse", "h265parse", "webrtcbin", "nicesink", "dtlssrtpenc",
            "srtpenc", "wasapi2src", "opusenc", "rtph264pay", "rtpopuspay")) {
            & $inspector $element | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "Required packaged GStreamer element failed to load: $element" }
            Write-Host "PASS: $element"
        }
    } finally { Pop-Location }
    @"
Package smoke check: PASS
Host starts with the package and Windows system directories only.
Core capture, conversion, parser, audio and WebRTC plugins register.
GPU encoding, display capture, real audio/input, installation and network playback still require hardware testing.
"@ | Set-Content (Join-Path (Split-Path $PackageDir -Parent) "PACKAGE-CHECK.txt")
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $previous[$name], "Process") }
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
}
