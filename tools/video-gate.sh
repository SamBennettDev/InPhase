#!/usr/bin/env bash
# Does the host actually deliver a decodable video stream?
#
# Mints an invite over the loopback admin API, runs the browser-free probe
# (crates/host/examples/wt_probe.rs) to pull frames off the real transport, and
# decodes the result with ffmpeg.
#
# This exists because the browser harness cannot answer the question. Headless
# Chrome has no HEVC decoder, so since the H.265-only switch it produced neither
# pixels nor a decoded-frame count and asserted nothing about video at all -
# every regression after that was found by a person looking at a phone.
#
# Decoding in the browser is not what needs testing. Either the encoder's
# bitstream reaches a client intact and is well-formed, or it does not, and
# ffmpeg decodes HEVC in software anywhere. On its first run this caught the
# encoder emitting length-prefixed hvc1 while the client config declared
# Annex-B - a mismatch that is invisible from the host side and looks exactly
# like a dead network from the client side.
#
# Usage: tools/video-gate.sh [seconds] [host]

set -uo pipefail

SECS="${1:-12}"
HOST="${2:?Pass the gaming PC address as argument 2}"
HOST_SSH="${HOST_SSH:?Set HOST_SSH to user@pc}"
OUT="${OUT:-/tmp/inphase-video-gate}"
mkdir -p "$OUT"
STREAM="$OUT/received.bit"
ROOT=$(cd "$(dirname "$0")/.." && pwd)

fail() { echo "FAIL video-gate: $1" >&2; exit 1; }

command -v ffprobe >/dev/null 2>&1 || fail "ffprobe is not installed"

INVITE=$(ssh -o BatchMode=yes -o ConnectTimeout=20 "$HOST_SSH" \
  "curl.exe -s -X POST http://127.0.0.1:47800/api/v1/admin/pair-invite" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["url"])') \
  || fail "could not mint an invite"

echo "== video gate: ${SECS}s against $HOST"
cargo run -q -p inphase-host --example wt_probe -- \
  --host "$HOST" --invite "$INVITE" --secs "$SECS" --out "$STREAM" \
  > "$OUT/probe.json" 2> "$OUT/probe.log"
RC=$?
sed 's/^/  /' "$OUT/probe.log" | grep -v '^\s*$' || true
[ "$RC" -eq 0 ] || fail "the probe delivered no frames (exit $RC)"

FRAMES=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["frames"])' "$OUT/probe.json")
KEYS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["keyframes"])' "$OUT/probe.json")
echo "  delivered   : $FRAMES frames ($KEYS key), $(wc -c < "$STREAM") bytes"

# The assertion: what arrived is a decodable stream of the right shape.
INFO=$(ffprobe -v error -count_frames -select_streams v:0 \
  -show_entries stream=codec_name,profile,level,width,height,nb_read_frames \
  -of default=nw=1 "$STREAM" 2>/dev/null)
DECODED=$(printf '%s\n' "$INFO" | sed -n 's/^nb_read_frames=//p')
GEOM=$(printf '%s\n' "$INFO" | sed -n 's/^width=//p' | head -1)x$(printf '%s\n' "$INFO" | sed -n 's/^height=//p' | head -1)
PROFILE=$(printf '%s\n' "$INFO" | sed -n 's/^profile=//p')
LEVEL=$(printf '%s\n' "$INFO" | sed -n 's/^level=//p')

echo "  decoded     : ${DECODED:-0} frames, $GEOM, profile $PROFILE level $LEVEL"

[ -n "${DECODED:-}" ] && [ "$DECODED" != "N/A" ] && [ "$DECODED" -gt 0 ] 2>/dev/null \
  || fail "the delivered bitstream does not decode - the client config and the encoder output disagree"
# A level of -99 / unknown profile means ffprobe could not read the parameter
# sets: in-band VPS/SPS/PPS are what an Annex-B client config promises.
[ "$LEVEL" != "-99" ] || fail "no readable parameter sets in the stream (in-band VPS/SPS/PPS missing)"

# Most of what was sent should survive; a few frames at the edges are expected
# because the probe joins and leaves mid-stream.
python3 - "$FRAMES" "$DECODED" <<'PY' || exit 1
import sys
sent, dec = int(sys.argv[1]), int(sys.argv[2])
if dec < sent * 0.9:
    sys.exit(f"FAIL video-gate: only {dec} of {sent} delivered frames decode")
PY

echo "PASS video-gate: $DECODED frames decode, $GEOM $PROFILE"
