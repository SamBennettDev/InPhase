# Tear down the stress-test processes (Heaven + the CDP Chrome), leave the host.
$ErrorActionPreference = "Continue"
Get-Process Heaven,Heaven_x64 -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue
$chrome = "C:\Program Files\Google\Chrome\Application\chrome.exe"
Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" |
  Where-Object { $_.CommandLine -match 'stress\\cdp-profile' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -EA SilentlyContinue }
"stopped heaven + cdp chrome"
