param(
  [int]$Width = 1280,
  [int]$Height = 720,
  [string]$Quality = "HIGH",
  [string]$Tess = "MODERATE",
  [int]$Fullscreen = 0
)
$ErrorActionPreference = "Continue"
$root = "C:\Program Files (x86)\Unigine\Heaven Benchmark 4.0"
$bin  = Join-Path $root "bin"
$exe  = Join-Path $bin "Heaven.exe"
$log  = "$env:USERPROFILE\stress\heaven.out.log"
Remove-Item $log -EA SilentlyContinue

Get-Process Heaven, browser_x86 -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue
Start-Sleep 1

$argline = "-data_path ../ -engine_config ../data/heaven_4.0.cfg -system_script heaven/unigine.cpp " +
           "-video_app direct3d11 -video_fullscreen $Fullscreen -video_width $Width -video_height $Height " +
           "-video_refresh 0 -sound_app openal -extern_plugin `"`" " +
           "-extern_define `"LANGUAGE_EN,QUALITY_$Quality,TESSELLATION_$Tess`""

"exe=$exe" | Out-File $log -Append
"args=$argline" | Out-File $log -Append
$p = Start-Process -FilePath $exe -ArgumentList $argline -WorkingDirectory $bin -PassThru `
       -RedirectStandardOutput "$log.stdout" -RedirectStandardError "$log.stderr"
"started pid=$($p.Id)" | Out-File $log -Append
Start-Sleep 8
$procs = Get-Process Heaven, browser_x86, Unigine -EA SilentlyContinue
if ($procs) { "RUNNING: " + ($procs | ForEach-Object { "$($_.ProcessName)#$($_.Id)" } -join ", ") }
else { "NOT RUNNING. stderr:"; Get-Content "$log.stderr" -EA SilentlyContinue | Select-Object -Last 20 }
