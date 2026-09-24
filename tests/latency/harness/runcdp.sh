#!/bin/bash
# runcdp.sh <label> <cdp-port> <origin> <duration> <settings-json> [host-side y/n for hostlog slice]
set -u
D=$(cd "$(dirname "$0")" && pwd)
HOST_SSH="${HOST_SSH:?Set HOST_SSH to user@pc}"
REMOTE_STRESS="${INPHASE_STRESS_DIR:?Set INPHASE_STRESS_DIR to the remote stress directory, e.g. C:/Users/you/stress}"
cd "$D"
LABEL=$1 PORT=$2 ORIGIN=$3 DUR=$4 SETTINGS=$5 HOSTSLICE=${6:-y}
PIN="${INPHASE_TEST_PIN:?Set the temporary test PIN in INPHASE_TEST_PIN}"

echo "== reset host session =="
ssh -o ConnectTimeout=8 "$HOST_SSH" "powershell -NoProfile -File \"$REMOTE_STRESS/pc-reset-session.ps1\""

T0=$(date -u +%s)
node cdp-probe.mjs --port "$PORT" --origin "$ORIGIN" --pin "$PIN" --duration "$DUR" --label "$LABEL" --out results --settings "$SETTINGS"
RC=$?
T1=$(date -u +%s)

if [ "$HOSTSLICE" = "y" ]; then
  echo "== host.log frametrace slice =="
  NLINES=$(( (T1-T0)*5 + 40 ))
  ssh -o ConnectTimeout=8 "$HOST_SSH" "powershell -NoProfile -ExecutionPolicy Bypass -File \"$REMOTE_STRESS/pc-hostlog-slice.ps1\" -Tail $NLINES"
  scp -q "$HOST_SSH:$REMOTE_STRESS/slice.txt" "results/${LABEL}.hostlog.txt"
  wc -l "results/${LABEL}.hostlog.txt"
  node hostagg.mjs "results/${LABEL}.hostlog.txt" | tee "results/${LABEL}.hostagg.json"
fi
exit $RC
