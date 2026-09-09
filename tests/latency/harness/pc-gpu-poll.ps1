param([int]$Count = 1200, [int]$IntervalSec = 2)
$smi = "C:\Windows\System32\nvidia-smi.exe"
$q = "utilization.gpu,utilization.encoder,utilization.decoder,memory.used,temperature.gpu,power.draw,clocks.sm"
"iso_time,gpu_pct,enc_pct,dec_pct,mem_mib,temp_c,power_w,sm_mhz"
for ($i = 0; $i -lt $Count; $i++) {
  $t = (Get-Date -Format "HH:mm:ss")
  $g = (& $smi --query-gpu=$q --format=csv,noheader,nounits) -replace "\s", ""
  "$t,$g"
  Start-Sleep -Seconds $IntervalSec
}
