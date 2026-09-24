#!/bin/bash
# End-to-end target validation: drive a real browser client against a real host,
# then assert the stream actually held its target.
#
#   tests/streaming/run.sh <label> <origin> <duration> [settings-json]
#
# Env:
#   INPHASE_TEST_PIN   temporary pairing PIN (required for a fresh profile)
#   HOST_SSH           user@pc — optional; enables the host-side frametrace slice
#   INPHASE_MODE       x11 (default) | ozone
#
# x11   — Chrome on the Xvfb display. A compositor exists, so the client's own
#         frame telemetry (requestVideoFrameCallback) works. On this box that
#         path cannot reach the GPU decoder, because Xvfb has no DRI3 and
#         Chrome's Linux VA-API import needs it. Expect software decode.
# ozone — Chrome on --ozone-platform=headless. Hardware decode works, but there
#         is no compositor, so rVFC never fires and frame telemetry is empty.
#         Use it to measure decode and network, not presentation.
#
# Neither mode gives both on this box; see README.md ("The client-side wall").
set -u
cd "$(dirname "$0")"
HERE="$(pwd)"
HARNESS="$HERE/../latency/harness"

LABEL="${1:?usage: run.sh <label> <origin> <duration> [settings-json]}"
ORIGIN="${2:?origin required, e.g. https://192.168.1.100:47800}"
DURATION="${3:-120}"
SETTINGS="${4:-}"
MODE="${INPHASE_MODE:-x11}"
DISPLAY_NUM=":77"
PORT=9222
OUT="$HERE/out"
mkdir -p "$OUT"

case "$MODE" in
  x11)   ./xvfb.sh start "$DISPLAY_NUM" 3840x2160 >/dev/null || exit 1 ;;
  ozone) : ;;
  *)     echo "INPHASE_MODE must be x11 or ozone" >&2; exit 2 ;;
esac

echo "== client =="
if [ "$MODE" = "ozone" ]; then
  CHROME="${INPHASE_CHROME:-$HOME/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome}"
  pkill -x chrome 2>/dev/null; sleep 1
  rm -rf /tmp/inphase-ozone-profile
  nohup "$CHROME" --remote-debugging-port=$PORT --remote-allow-origins='*' \
    --user-data-dir=/tmp/inphase-ozone-profile --no-first-run --no-default-browser-check \
    --no-sandbox --ignore-certificate-errors --ozone-platform=headless --use-angle=vulkan \
    --ignore-gpu-blocklist \
    --enable-features=VaapiVideoDecoder,VaapiVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxGL \
    --disable-gpu-vsync --disable-frame-rate-limit \
    --autoplay-policy=no-user-gesture-required about:blank \
    >/tmp/inphase-ozone.log 2>&1 &
  echo $! >"/tmp/inphase-chrome-$PORT.pid"
  for _ in $(seq 1 60); do curl -sf --max-time 2 "http://127.0.0.1:$PORT/json/version" >/dev/null && break; sleep 0.5; done
else
  ./client.sh start "$DISPLAY_NUM" "$PORT" about:blank || exit 1
fi

echo "== decode capabilities =="
node gpucheck.mjs --port $PORT --require any | tee "$OUT/$LABEL.gpu.json"
DECODE_HW=$(python3 -c "import json,sys;print(json.load(open('$OUT/$LABEL.gpu.json'))['videoDecode'])" 2>/dev/null || echo "?")
if [ "$MODE" = "x11" ] && [ "$DECODE_HW" != "enabled" ]; then
  echo "  WARNING: hardware decode is not enabled; results will show software decode." >&2
fi

PIN="${INPHASE_TEST_PIN:-}"
echo "== stream: $ORIGIN for ${DURATION}s =="
if [ -n "${HOST_SSH:-}" ]; then
  ssh -o ConnectTimeout=8 "$HOST_SSH" \
    "powershell -NoProfile -File \"${INPHASE_STRESS_DIR:-C:/Users/$USER/stress}/pc-reset-session.ps1\"" \
    && echo "  host session reset" || echo "  host reset failed (continuing)"
fi

node "$HARNESS/cdp-probe.mjs" --port $PORT --origin "$ORIGIN" --pin "$PIN" \
  --duration "$DURATION" --label "$LABEL" --out "$OUT" ${SETTINGS:+--settings "$SETTINGS"}
PROBE_RC=$?

# Assert against the mode that was actually requested. Hardcoding 4K120 here
# scored every other mode as a failure, which reads as "the stream is broken"
# when the stream is fine and only the expectation was wrong.
TARGET=$(python3 -c "
import json,sys
try: s=json.loads(sys.argv[1]) if sys.argv[1] else {}
except Exception: s={}
print(f\"{s.get('width',3840)}x{s.get('height',2160)}@{s.get('fps',120)}\")
" "$SETTINGS")
BITRATE=$(python3 -c "
import json,sys
try: s=json.loads(sys.argv[1]) if sys.argv[1] else {}
except Exception: s={}
print(s.get('maxBitrateKbps',80000))
" "$SETTINGS")

echo
echo "== verify against $TARGET / $BITRATE kbps =="
if [ -f "$OUT/$LABEL.summary.json" ]; then
  node verify.mjs --summary "$OUT/$LABEL.summary.json" --target "$TARGET" --bitrate "$BITRATE"
  VERIFY_RC=$?
else
  echo "no summary produced (probe exit $PROBE_RC)"; VERIFY_RC=1
fi

if [ -n "${HOST_SSH:-}" ] && [ -f "$OUT/$LABEL.summary.json" ]; then
  echo "== host frametrace slice =="
  NLINES=$(( DURATION * 5 + 40 ))
  ssh -o ConnectTimeout=8 "$HOST_SSH" \
    "powershell -NoProfile -ExecutionPolicy Bypass -File \"${INPHASE_STRESS_DIR:-C:/Users/$USER/stress}/pc-hostlog-slice.ps1\" -Tail $NLINES" \
    && scp -q "$HOST_SSH:${INPHASE_STRESS_DIR:-C:/Users/$USER/stress}/slice.txt" "$OUT/$LABEL.hostlog.txt" \
    && node "$HARNESS/hostagg.mjs" "$OUT/$LABEL.hostlog.txt" | tee "$OUT/$LABEL.hostagg.json" \
    || echo "  host slice unavailable"
fi

exit $VERIFY_RC
