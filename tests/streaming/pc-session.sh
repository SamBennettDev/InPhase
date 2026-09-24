#!/bin/bash
# Run a PowerShell script in the gaming PC's *interactive console session*.
#
#   tests/streaming/pc-session.sh <local.ps1> [timeout-seconds]
#
# Why this exists: SSH into Windows lands in session 0, which has no desktop.
# Anything that touches the display, the GPU's desktop or a GUI process
# (EnumDisplaySettings, Chrome, a game) fails or returns nothing there. The
# InPhaseStress scheduled task — created with /it, targeting stress\_action.ps1
# — hops into session 1, where the real desktop lives.
#
# The payload is base64/UTF-16LE encoded so the command line contains no quotes
# or spaces that cmd.exe can mangle on the way through.
set -u

PC="${INPHASE_PC:?set INPHASE_PC=user@the-host-pc (for the admin API tunnel)}"
STRESS="${INPHASE_STRESS_DIR:?set INPHASE_STRESS_DIR=C:/Users/<you>/stress (the host PC staging dir)}"
SCRIPT="${1:?usage: pc-session.sh <local.ps1> [timeout]}"
TIMEOUT="${2:-120}"

[ -f "$SCRIPT" ] || { echo "no such script: $SCRIPT" >&2; exit 2; }

B64=$(python3 -c "
import base64,sys
print(base64.b64encode(open(sys.argv[1],'rb').read().decode('utf-8').encode('utf-16-le')).decode())
" "$SCRIPT")

SSH="ssh -o BatchMode=yes -o ConnectTimeout=10 $PC"

# Line count before, so we only print this run's output.
BEFORE=$($SSH "powershell -NoProfile -Command \"(Get-Content '$STRESS/action.log' -ErrorAction SilentlyContinue | Measure-Object -Line).Lines\"" 2>/dev/null | tr -d '\r\n ' )
BEFORE=${BEFORE:-0}

# A single command line, no quoting hazards.
$SSH "powershell -NoProfile -Command \"Set-Content -Path '$STRESS/action.txt' -Value 'powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand $B64' -Encoding ascii\"" >/dev/null 2>&1 || {
  echo "failed to stage action.txt" >&2; exit 1; }

$SSH "schtasks /run /tn InPhaseStress" >/dev/null 2>&1 || {
  echo "failed to start InPhaseStress (does the task exist? see README)" >&2; exit 1; }

# Poll for the output to land.
DEADLINE=$((SECONDS + TIMEOUT))
RESULT=""
while [ $SECONDS -lt $DEADLINE ]; do
  sleep 2
  NOW=$($SSH "powershell -NoProfile -Command \"(Get-Content '$STRESS/action.log' -ErrorAction SilentlyContinue | Measure-Object -Line).Lines\"" 2>/dev/null | tr -d '\r\n ' )
  NOW=${NOW:-0}
  if [ "$NOW" -gt "$BEFORE" ]; then
    # Give the run a moment to finish writing.
    sleep 2
    RESULT=$($SSH "powershell -NoProfile -Command \"Get-Content '$STRESS/action.log' -Tail $((NOW - BEFORE + 2))\"" 2>/dev/null)
    break
  fi
done

if [ -z "$RESULT" ]; then
  echo "timed out after ${TIMEOUT}s waiting for session-1 output" >&2
  exit 1
fi

# Strip the leading echo of the command itself, which is just base64 noise.
printf '%s\n' "$RESULT" | sed 's/^\[_action[^]]*\] *powershell.*EncodedCommand .*$/[_action] (command dispatched)/'
