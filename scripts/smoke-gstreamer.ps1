<#
.SYNOPSIS
  Prove the three risky primitives at the GStreamer level (architecture report
  sec 19 Phase 0, sec 29 steps 3-5) without needing the full Rust app.

.DESCRIPTION
  1. Capture -> D3D11Memory caps (no CPU copy)         [sec 5.1]
  2. Capture -> NVENC H.264 low-latency -> fakesink    [sec 6]  measures encode fps
  3. Element inventory for the pipeline InPhase builds
#>
[CmdletBinding()]
param([int]$Monitor = 0, [int]$Fps = 120, [int]$Seconds = 8)
$ErrorActionPreference = "Continue"
$gst = ([Environment]::GetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64","Machine")).TrimEnd('\')
$env:Path = "$gst\bin;$env:Path"
$launch  = "$gst\bin\gst-launch-1.0.exe"
$inspect = "$gst\bin\gst-inspect-1.0.exe"

Write-Host "== 1. element inventory ==" -ForegroundColor Cyan
foreach ($e in "d3d11screencapturesrc","d3d11convert","nvd3d11h264enc","nvd3d11h265enc","h264parse","rtph264pay","webrtcbin","wasapi2src","opusenc") {
  $ok = & $inspect $e 2>$null
  "{0,-22} {1}" -f $e, ($(if ($LASTEXITCODE -eq 0) { "OK" } else { "MISSING" }))
}

Write-Host "`n== 2. DXGI capture -> D3D11Memory NV12 (caps check) ==" -ForegroundColor Cyan
& $launch -v -e `
  d3d11screencapturesrc capture-api=dxgi monitor-index=$Monitor show-cursor=false num-buffers=30 `
  "!" "queue max-size-buffers=1 leaky=downstream" `
  "!" d3d11convert `
  "!" "video/x-raw(memory:D3D11Memory),format=NV12" `
  "!" fakesink sync=false 2>&1 | Select-String "D3D11Memory|caps = |format" | Select-Object -First 6

Write-Host "`n== 3. capture -> NVENC H.264 (low-latency) -> fakesink, ${Seconds}s ==" -ForegroundColor Cyan
$job = Start-Job -ScriptBlock {
  param($launch,$mon,$fps)
  & $launch -e `
    d3d11screencapturesrc capture-api=dxgi monitor-index=$mon show-cursor=false `
    "!" "queue max-size-buffers=1 leaky=downstream" `
    "!" d3d11convert `
    "!" "video/x-raw(memory:D3D11Memory),format=NV12,framerate=$fps/1" `
    "!" "nvd3d11h264enc b-frames=0 rc-lookahead=0 zerolatency=true bitrate=30000" `
    "!" "h264parse" `
    "!" "fpsdisplaysink video-sink=fakesink text-overlay=false sync=false" 2>&1
} -ArgumentList $launch,$Monitor,$Fps
Start-Sleep -Seconds $Seconds
Stop-Job $job
Receive-Job $job 2>&1 | Select-String "average-rate|current-rate|dropped" | Select-Object -Last 4
Remove-Job $job -Force
Write-Host "`ndone - see report sec 20 acceptance gates for interpretation." -ForegroundColor Green
