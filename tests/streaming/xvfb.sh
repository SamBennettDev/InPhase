#!/bin/bash
# Bring up / tear down a virtual X display sized for the stream target.
#
#   tests/streaming/xvfb.sh start [display] [WxH]
#   tests/streaming/xvfb.sh stop  [display]
#
# Xvfb has no real refresh or vsync, so anything that measures *presentation*
# timing (compositor -> glass) is meaningless here. What this display is for is
# giving a headed browser somewhere to composite, so decode, pacing, frame drops
# and bitrate can be measured on real content. See README.md.
set -u

ACTION="${1:-start}"
DISP="${2:-:77}"
GEOM="${3:-3840x2160}"
PIDFILE="/tmp/inphase-xvfb${DISP//:/}.pid"

case "$ACTION" in
  start)
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "display $DISP already up (pid $(cat "$PIDFILE"))"
      exit 0
    fi
    if ! command -v Xvfb >/dev/null; then
      echo "Xvfb not installed (apt install xvfb)" >&2
      exit 1
    fi
    nohup Xvfb "$DISP" -screen 0 "${GEOM}x24" -nolisten tcp -ac \
      >"/tmp/inphase-xvfb${DISP//:/}.log" 2>&1 &
    echo $! >"$PIDFILE"
    # Wait for the socket rather than sleeping a fixed amount.
    for _ in $(seq 1 40); do
      if DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1; then break; fi
      sleep 0.25
    done
    if ! DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1; then
      echo "display $DISP failed to come up; see /tmp/inphase-xvfb${DISP//:/}.log" >&2
      exit 1
    fi
    echo "display $DISP up at $GEOM (pid $(cat "$PIDFILE"))"
    DISPLAY="$DISP" xdpyinfo | grep -E 'dimensions|depth of root'
    ;;

  stop)
    if [ -f "$PIDFILE" ]; then
      PID="$(cat "$PIDFILE")"
      kill "$PID" 2>/dev/null
      rm -f "$PIDFILE"
      echo "display $DISP stopped (pid $PID)"
    else
      echo "no pidfile for $DISP"
    fi
    ;;

  *)
    echo "usage: $0 {start|stop} [display] [WxH]" >&2
    exit 2
    ;;
esac
