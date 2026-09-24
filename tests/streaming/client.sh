#!/bin/bash
# Launch the browser client under the virtual display with a real GPU path and
# VA-API hardware video decode, exposing the DevTools protocol.
#
#   tests/streaming/client.sh start [display] [port] [origin]
#   tests/streaming/client.sh stop  [port]
#
# `stop` takes the port second, not the display: the display is irrelevant for
# stopping, and keeping identical positional order would silently stop whichever
# client happens to sit on the default port.
#
# Notes that matter:
#   --use-angle=vulkan  is what gets RADV/radeonsi instead of Mesa's llvmpipe.
#                       Under Xvfb there is no GLX acceleration, so ANGLE's GL
#                       backend silently falls back to software and 4K playback
#                       stalls. Vulkan does not have that problem.
#   --disable-gpu-vsync --disable-frame-rate-limit
#                       Xvfb reports no real refresh, so the compositor must be
#                       unthrottled for throughput numbers to mean anything.
set -u

ACTION="${1:-start}"
if [ "$ACTION" = "stop" ]; then
  DISP="${2:-:77}"
  PORT="${2:-9222}"
  ORIGIN="about:blank"
else
  DISP="${2:-:77}"
  PORT="${3:-9222}"
  ORIGIN="${4:-about:blank}"
fi

CHROME="${INPHASE_CHROME:-$HOME/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome}"
PROFILE="${INPHASE_PROFILE:-/tmp/inphase-cdp-profile}"
PIDFILE="/tmp/inphase-chrome-${PORT}.pid"
LOGFILE="/tmp/inphase-chrome-${PORT}.log"

case "$ACTION" in
  start)
    if [ ! -x "$CHROME" ]; then
      echo "browser not found at $CHROME (set INPHASE_CHROME, or run: npx playwright install chromium)" >&2
      exit 1
    fi
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "client already running on port $PORT (pid $(cat "$PIDFILE"))"
      exit 0
    fi
    rm -rf "$PROFILE"
    nohup env DISPLAY="$DISP" "$CHROME" \
      --remote-debugging-port="$PORT" --remote-allow-origins='*' \
      --user-data-dir="$PROFILE" \
      --no-first-run --no-default-browser-check --no-sandbox \
      --ignore-certificate-errors \
      --use-angle=vulkan \
      --ignore-gpu-blocklist --enable-gpu-rasterization \
      --enable-features=VaapiVideoDecoder,VaapiVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxGL \
      --disable-gpu-vsync --disable-frame-rate-limit \
      --autoplay-policy=no-user-gesture-required \
      --disable-features=CalculateNativeWinOcclusion \
      --window-size=3840,2160 --window-position=0,0 \
      --new-window "$ORIGIN" >"$LOGFILE" 2>&1 &
    echo $! >"$PIDFILE"
    for _ in $(seq 1 60); do
      if curl -sf --max-time 2 "http://127.0.0.1:$PORT/json/version" >/dev/null 2>&1; then break; fi
      sleep 0.5
    done
    if ! curl -sf --max-time 2 "http://127.0.0.1:$PORT/json/version" >/dev/null 2>&1; then
      echo "client failed to expose CDP on $PORT; see $LOGFILE" >&2
      tail -5 "$LOGFILE" >&2
      exit 1
    fi
    echo "client up on $DISP port $PORT (pid $(cat "$PIDFILE"))"
    ;;

  stop)
    if [ -f "$PIDFILE" ]; then
      PID="$(cat "$PIDFILE")"
      # Kill the whole process tree; Chrome's children outlive the parent.
      pkill -P "$PID" 2>/dev/null
      kill "$PID" 2>/dev/null
      sleep 1
      kill -9 "$PID" 2>/dev/null
      rm -f "$PIDFILE"
      echo "client on port $PORT stopped (pid $PID)"
    else
      echo "no pidfile for port $PORT"
    fi
    ;;

  *)
    echo "usage: $0 {start|stop} [display] [port] [origin]" >&2
    exit 2
    ;;
esac
