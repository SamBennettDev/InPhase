# Launch a Chrome InPhase client in the interactive console session with the
# DevTools protocol exposed on 127.0.0.1:9222. Non-blocking (Start-Process).
param(
  [string]$Url = "http://127.0.0.1:47800/",
  [int]$Port = 9222,
  [string]$Pos = "80,60",          # nudge onto the secondary monitor
  [string]$Size = "960,600"
)
$ErrorActionPreference = "Continue"
$chrome = "C:\Program Files\Google\Chrome\Application\chrome.exe"
$prof = "C:\Users\sambe\stress\cdp-profile"
Get-Process chrome -EA SilentlyContinue |
  Where-Object { $_.Path -eq $chrome } |
  Where-Object { (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine -match 'stress\\cdp-profile' } |
  Stop-Process -Force -EA SilentlyContinue
Start-Sleep 1
$args = @(
  "--remote-debugging-port=$Port",
  "--remote-allow-origins=*",
  "--user-data-dir=$prof",
  "--no-first-run","--no-default-browser-check","--disable-session-crashed-bubble",
  "--autoplay-policy=no-user-gesture-required",
  "--disable-features=CalculateNativeWinOcclusion",
  "--new-window","--window-position=$Pos","--window-size=$Size",
  $Url
)
Start-Process $chrome -ArgumentList $args
Start-Sleep 2
"launched chrome -> $Url  (CDP 127.0.0.1:$Port)"
