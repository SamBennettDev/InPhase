# Drop any active player session so the next client isn't rejected (ADR-007
# one-active-player). Prints state before/after.
$ErrorActionPreference = "Continue"
$b = "http://127.0.0.1:47801/api/v1/admin"
$s = (Invoke-WebRequest -UseBasicParsing "$b/status").Content | ConvertFrom-Json
"before: state=$($s.state)"
Invoke-WebRequest -UseBasicParsing -Method POST "$b/disconnect" | Out-Null
Start-Sleep -Seconds 3
$s2 = (Invoke-WebRequest -UseBasicParsing "$b/status").Content | ConvertFrom-Json
"after:  state=$($s2.state) pin=$($s2.pin)"
