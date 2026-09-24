#!/bin/bash
# Run ios.mjs against Safari on the iPhone paired with the Mac.
#
#   tests/streaming/ios.sh <label> [duration] [settings-json]
#
# Starts safaridriver on the Mac (INPHASE_MAC), tunnels it and the host's admin
# API here, and closes everything after. See ios.mjs for the one-time setup.
set -u
cd "$(dirname "$0")"
LABEL="${1:?usage: ios.sh <label> [duration] [settings-json]}"
DURATION="${2:-90}"
SETTINGS="${3:-{\"width\":1920,\"height\":1080,\"fps\":60,\"maxBitrateKbps\":80000\}}"
MAC="${INPHASE_MAC:?set INPHASE_MAC=user@the-mac (runs the client browser)}"
PC="${INPHASE_PC:?set INPHASE_PC=user@the-host-pc (for the admin API tunnel)}"
ORIGIN="${INPHASE_ORIGIN:?set INPHASE_ORIGIN=https://<the host play URL>}"

# A fresh safaridriver per run: a session whose page crashed stays "paired"
# with the phone's Safari and refuses every new session until the driver goes.
ssh -o BatchMode=yes "$MAC" 'pkill -f "safaridriver -p 4444"; sleep 1; (nohup safaridriver -p 4444 >/tmp/safaridriver.log 2>&1 &); sleep 1'
ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -N -L 47811:127.0.0.1:47801 "$PC" & A=$!
ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -N -L 4444:127.0.0.1:4444 "$MAC" & B=$!
trap 'kill $A $B 2>/dev/null' EXIT
for _ in $(seq 20); do
  curl -sf --max-time 2 http://127.0.0.1:47811/api/v1/admin/status >/dev/null &&
    curl -sf --max-time 2 http://127.0.0.1:4444/status >/dev/null && break
  sleep 0.5
done
node ios.mjs --origin "$ORIGIN" --label "$LABEL" --duration "$DURATION" --settings "$SETTINGS" "${@:4}"
