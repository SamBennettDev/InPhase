#!/bin/bash
# Cross-browser x network-profile matrix against a live host.
#
#   tests/streaming/matrix.sh "<engines>" "<profiles>" [duration] [settings-json]
#   e.g. matrix.sh "chromium firefox" "clean jitter loss1 burst wifi cap40" 75
#
# Owns the SSH forward to the host's loopback admin API for its whole run (a
# forward left behind by an earlier shell dies with it), applies each netem
# profile, runs xbrowser.mjs, and always clears the impairment on exit.
# Env: INPHASE_PC=user@host-pc (required), INPHASE_ORIGIN (default https://192.168.1.100).
set -u
cd "$(dirname "$0")"
ENGINES="${1:-chromium firefox}"
PROFILES="${2:-clean}"
DURATION="${3:-75}"
SETTINGS="${4:-{\}}"
PC="${INPHASE_PC:?set INPHASE_PC=user@the-host-pc (for the admin API tunnel)}"
ORIGIN="${INPHASE_ORIGIN:-https://192.168.1.100}"
TAG="${INPHASE_TAG:-$(date +%H%M)}"

ssh -o BatchMode=yes -o ServerAliveInterval=15 -o ExitOnForwardFailure=yes \
  -N -L 47811:127.0.0.1:47801 "$PC" &
FWD=$!
cleanup() { ./netem.sh clear >/dev/null 2>&1; kill "$FWD" 2>/dev/null; }
trap cleanup EXIT
for _ in $(seq 20); do curl -sf --max-time 2 http://127.0.0.1:47811/api/v1/admin/status >/dev/null && break; sleep 0.5; done

SUMMARY=()
for profile in $PROFILES; do
  ./netem.sh "$profile" >/dev/null || exit 1
  for engine in $ENGINES; do
    label="$TAG-$engine-$profile"
    node xbrowser.mjs --engine "$engine" --origin "$ORIGIN" --profile "$profile" \
      --duration "$DURATION" --label "$label" --settings "$SETTINGS" 2>&1 \
      | grep -E 'PASS|FAIL|RESULT|renderer|t=[0-9]+'
    SUMMARY+=("$(node -e '
      const r = require("./out/" + process.argv[1] + ".json"), m = r.measurements || {};
      console.log([process.argv[1].padEnd(28), r.passed ? "PASS" : "FAIL",
        "dec", m.dec_fps_p50, "p5", m.dec_fps_p5, "enc", m.enc_kbps_p50, "rx", m.rx_kbps_p50,
        "stall", m.stall_seconds, "freeze", m.freeze_ms, r.fatal ? "FATAL " + r.fatal.slice(0, 80) : ""].join(" "));
    ' "$label")")
    sleep 3
  done
done
./netem.sh clear >/dev/null
printf '\n== matrix summary ==\n'; printf '%s\n' "${SUMMARY[@]}"
