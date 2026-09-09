param([int]$Tail = 6000)
# Write the last $Tail frametrace summary lines to slice.txt, unwrapped, ANSI-stripped.
$out = "C:\Users\sambe\stress\slice.txt"
$lines = Get-Content C:\Users\sambe\host.log -Tail $Tail |
  Where-Object { $_ -match 'missing=' -and $_ -match 'send_gap_ms' } |
  ForEach-Object { $_ -replace '\x1b\[[0-9;]*m', '' }
[System.IO.File]::WriteAllLines($out, $lines)
"wrote $($lines.Count) lines -> $out"
