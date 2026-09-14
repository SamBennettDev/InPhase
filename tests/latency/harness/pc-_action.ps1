# Runs whatever command line is in stress\action.txt, in the interactive
# console session (this script is the target of the InPhaseStress task).
$ErrorActionPreference = "Continue"
$f = "$env:USERPROFILE\stress\action.txt"
$cmd = (Get-Content $f -Raw).Trim()
"[_action $(Get-Date -Format HH:mm:ss)] $cmd" | Out-File $env:USERPROFILE\stress\action.log -Append
try { Invoke-Expression $cmd 2>&1 | Out-File $env:USERPROFILE\stress\action.log -Append }
catch { "ERR: $_" | Out-File $env:USERPROFILE\stress\action.log -Append }
