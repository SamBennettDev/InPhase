#!/bin/bash
# runcdp.sh <label> <cdp-port> <origin> <duration> <settings-json> [host-side y/n for hostlog slice]
set -u
D=/tmp/claude-1000/-home-cin-dev-inphase/261d2e08-d713-4c6c-8489-43ee526a1d19/scratchpad/stress
cd "$D"
LABEL=$1 PORT=$2 ORIGIN=$3 DUR=$4 SETTINGS=$5 HOSTSLICE=${6:-y}
PIN=$(cat .pin)

echo "== reset host session =="
ssh -o ConnectTimeout=8 sambe@100.127.176.18 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\sambe\stress\pc-reset-session.ps1'

T0=$(date -u +%s)
node cdp-probe.mjs --port "$PORT" --origin "$ORIGIN" --pin "$PIN" --duration "$DUR" --label "$LABEL" --out results --settings "$SETTINGS"
RC=$?
T1=$(date -u +%s)

if [ "$HOSTSLICE" = "y" ]; then
  echo "== host.log frametrace slice =="
  NLINES=$(( (T1-T0)*5 + 40 ))
  ssh -o ConnectTimeout=8 sambe@100.127.176.18 "powershell -NoProfile -ExecutionPolicy Bypass -File C:\\Users\\sambe\\stress\\pc-hostlog-slice.ps1 -Tail $NLINES"
  scp -q sambe@100.127.176.18:'C:/Users/sambe/stress/slice.txt' "results/${LABEL}.hostlog.txt"
  wc -l "results/${LABEL}.hostlog.txt"
  node hostagg.mjs "results/${LABEL}.hostlog.txt" | tee "results/${LABEL}.hostagg.json"
fi
exit $RC
