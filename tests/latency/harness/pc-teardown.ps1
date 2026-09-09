$ErrorActionPreference = "Continue"
Get-Process Heaven, browser_x86, Heaven_x64 -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue
Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" |
  Where-Object { $_.CommandLine -match 'stress\\cdp-profile' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -EA SilentlyContinue }
Start-Sleep 2
"heaven=$((Get-Process Heaven -EA SilentlyContinue|Measure-Object).Count) chrome=$((Get-Process chrome -EA SilentlyContinue|Measure-Object).Count)"
$s = (Invoke-WebRequest -UseBasicParsing http://127.0.0.1:47801/api/v1/admin/status).Content | ConvertFrom-Json
"host state=$($s.state) uptime=$($s.uptime_secs)s"
