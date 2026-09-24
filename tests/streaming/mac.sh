#!/bin/bash
# Drive the stream on a real client Mac over CDP (hardware decode, real Wi-Fi).
#
#   tests/streaming/mac.sh <label> [duration] [settings-json]
#
# Needs: SSH to the Mac (INPHASE_MAC=user@mac) with a
# Chrome started there as
#   open -na "Google Chrome" --args --remote-debugging-port=9223 \
#     --user-data-dir=/tmp/inphase-cdp --no-first-run
# (a separate profile: the owner's own Chrome is never touched), and SSH to the
# host PC for the admin API. Opens both tunnels for the run, closes them after.
set -u
cd "$(dirname "$0")"
LABEL="${1:?usage: mac.sh <label> [duration] [settings-json]}"
DURATION="${2:-90}"
SETTINGS="${3:-{\"width\":2560,\"height\":1440,\"fps\":120,\"maxBitrateKbps\":80000\}}"
MAC="${INPHASE_MAC:?set INPHASE_MAC=user@the-mac (runs the client browser)}"
PC="${INPHASE_PC:?set INPHASE_PC=user@the-host-pc (for the admin API tunnel)}"
ORIGIN="${INPHASE_ORIGIN:?set INPHASE_ORIGIN=https://<the host play URL>}"

ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -N -L 47811:127.0.0.1:47801 "$PC" & A=$!
ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -N -L 9223:127.0.0.1:9223 "$MAC" & B=$!
trap 'kill $A $B 2>/dev/null' EXIT
for _ in $(seq 20); do
  curl -sf --max-time 2 http://127.0.0.1:47811/api/v1/admin/status >/dev/null &&
    curl -sf --max-time 2 http://127.0.0.1:9223/json/version >/dev/null && break
  sleep 0.5
done
node xbrowser.mjs --engine chromium --cdp http://127.0.0.1:9223 --origin "$ORIGIN" \
  --duration "$DURATION" --label "$LABEL" --settings "$SETTINGS"
