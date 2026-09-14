#!/usr/bin/env bash
# Remote-connectivity E2E for the WT video path (ADR-0011).
#
# Mints a pairing invite on the host, drives a real browser through it with
# tools/e2e/run.js, and asserts the harness's verdict.
#
# What changed and why: this used to grep the harness's console output for
# "wt first glass age" and a `framesPresented` counter. Both are *decoder*
# facts. A decoder can run perfectly while nothing reaches the screen - the iOS
# Safari case - so the suite passed all night while a Mac and a phone were
# black. The harness now samples the canvas and computes the verdict itself
# (non-blank pixels, and pixels that actually change); this script reads that
# verdict rather than re-deriving one from log text it might not match.
#
# Usage: tools/remote-e2e.sh [duration_s] [pc_addr] [label] [--kill]
#   pc_addr: address the page + dial use. Default is the tailnet IP.
#            NOTE: from a machine on the same LAN as the host, its LAN IP *and*
#            its global IPv6 are both on-link - such a run proves nothing about
#            internet reachability. Use an off-net vantage point for that.
#   --kill:  additionally hard-kill the browser mid-session and require the host
#            to free the session slot.
#
# Env: E2E_CHROME  path to a Chrome binary; NODE_PATH  where playwright-core lives.

set -uo pipefail

DUR="${1:-45}"
ADDR="${2:?Pass the gaming PC address as argument 2}"
LABEL="${3:-clean}"
KILL=""
for a in "$@"; do [ "$a" = "--kill" ] && KILL="--kill"; done

HOST_SSH="${HOST_SSH:?Set HOST_SSH to user@pc}"
OUT="${OUT:-/tmp/wte2e}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)

fail() { echo "FAIL $LABEL: $1" >&2; exit 1; }

INVITE=$(ssh -o BatchMode=yes -o ConnectTimeout=20 "$HOST_SSH" \
  "curl.exe -s -X POST http://127.0.0.1:47800/api/v1/admin/pair-invite") \
  || fail "could not mint an invite"

URL=$(printf '%s' "$INVITE" | python3 -c '
import json, sys, re
raw = sys.stdin.read()
try:
    u = json.loads(raw)["url"]
except Exception:
    sys.exit("could not parse invite: " + raw[:200])
print(re.sub(r"//[^/:]+", "//" + sys.argv[1], u, count=1))
' "$ADDR") || fail "could not build the invite URL"

echo "== $LABEL: ${DUR}s against $ADDR"
E2E_STATUS="${E2E_STATUS:-}" node "$ROOT/tools/e2e/run.js" "$URL" "$DUR" --out "$OUT" $KILL
RC=$?

RESULT="$OUT/result.json"
[ -f "$RESULT" ] || fail "the harness produced no result.json (exit $RC)"

# H.265-only (user directive): a headless client without an HEVC decoder is
# refused client-side at negotiate() - the video gate cannot run there. That
# is the contract working, not a regression; the real clients (Safari on the
# user's phone) decode HEVC in hardware.
if [ "$RC" -ne 0 ] && grep -q "can't decode H.265" "$OUT/console.log" 2>/dev/null; then
  echo "SKIP-HEVC $LABEL: client cannot decode H.265 - H.265-only refusal fired as designed (video gate untested here; phone verifies the real path)"
  exit 0
fi

# The verdict is the harness's, not this script's - one place decides.
python3 - "$RESULT" "$LABEL" <<'PY' || exit 1
import json, sys
r = json.load(open(sys.argv[1])); label = sys.argv[2]
px = r["pixels"]
print(f"  first glass : {r['firstGlassMs']} ms")
print(f"  samples     : {px['sampled']} ({px['blankSamples']} blank)")
print(f"  non-blank   : {px['everNonBlank']}")
print(f"  changing    : {px['everChanged']}")
print(f"  blank tail  : {px.get('blankAfterGood', 0)}")
if r.get("killTested"):
    print(f"  slot freed  : {r['killRecovered']}")
if not r["ok"]:
    for f in r["failures"]:
        print(f"FAIL {label}: {f}", file=sys.stderr)
    sys.exit(1)
print(f"PASS {label}")
PY
