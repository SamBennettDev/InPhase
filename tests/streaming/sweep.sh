#!/bin/bash
# Find the client's real hardware-decode ceiling: generate a clip at each mode,
# then measure what the browser's WebCodecs decoder can actually sustain.
#
#   tests/streaming/sweep.sh [port] [outdir]
#
# Writes <outdir>/ceiling.json and prints a table. A mode whose hardware decode
# errors out is reported as such rather than silently falling back — the point
# is to find where the wall is, not to find a number that looks good.
set -u
cd "$(dirname "$0")"
PORT="${1:-9555}"
OUT="${2:-out}"
mkdir -p "$OUT"
WORK=/tmp/inphase-sweep
mkdir -p "$WORK"

# mode:label:WxH:fps:bitrate
MODES=(
  "2160p120:3840x2160:120:80M"
  "2160p60:3840x2160:60:80M"
  "1440p120:2560x1440:120:60M"
  "1440p60:2560x1440:60:40M"
  "1080p120:1920x1080:120:30M"
  "1080p60:1920x1080:60:20M"
)

echo "[" >"$OUT/ceiling.json"
FIRST=1

for entry in "${MODES[@]}"; do
  IFS=: read -r label size fps br <<<"$entry"
  w="${size%x*}"; h="${size#*x}"
  mp4="$WORK/$label.mp4"; raw="$WORK/$label.264"

  echo "=== $label  ${w}x${h}@${fps}  $br ==="

  if [ ! -f "$mp4" ]; then
    ffmpeg -hide_banner -loglevel error -f lavfi \
      -i "testsrc2=size=${w}x${h}:rate=${fps}:duration=2" \
      -c:v libx264 -preset ultrafast -b:v "$br" -pix_fmt yuv420p -y "$mp4" || {
        echo "  encode failed"; continue; }
  fi
  # h264_mp4toannexb is required: -c:v copy alone leaves SPS/PPS in the MP4 avcC
  # box and the decoder then has no configuration.
  ffmpeg -hide_banner -loglevel error -i "$mp4" -c:v copy \
    -bsf:v h264_mp4toannexb,h264_metadata=aud=insert -f h264 "$raw" -y || {
    echo "  extract failed"; continue; }

  prof=$(ffprobe -v error -select_streams v:0 -show_entries stream=profile \
    -of default=nw=1:nk=1 "$mp4")
  lvl=$(ffprobe -v error -select_streams v:0 -show_entries stream=level \
    -of default=nw=1:nk=1 "$mp4")
  # avc1.PPCCLL — Constrained Baseline is 0x42 with constraint flags 0xC0.
  case "$prof" in
    "Constrained Baseline") ppcc="42C0" ;;
    "Baseline")             ppcc="4200" ;;
    "Main")                 ppcc="4D40" ;;
    "High")                 ppcc="6400" ;;
    *)                      ppcc="42C0" ;;
  esac
  codec=$(printf 'avc1.%s%02X' "$ppcc" "$lvl")
  echo "  bitstream: profile='$prof' level=$lvl -> $codec"

  res=$(node wccap.mjs --port "$PORT" --file "$raw" --codec "$codec" \
    --expect "$fps" --mode max 2>&1 | sed -n '/^{/,$p')

  if [ -z "$res" ]; then echo "  no result"; continue; fi
  echo "$res" | node -e '
    let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
      const o=JSON.parse(s);
      console.log(`  decoded=${o.decodedFrames}/${o.accessUnits} fps=${o.achievedFps} cpu=${o.cpuCoresUsed} cores kind=${o.decodeKind}${o.errored?"\n  ERROR: "+o.errored:""}`);
    });' 2>/dev/null || echo "$res" | head -5

  [ $FIRST -eq 1 ] || echo "," >>"$OUT/ceiling.json"
  FIRST=0
  echo "{\"label\":\"$label\",\"width\":$w,\"height\":$h,\"fps\":$fps,\"codec\":\"$codec\",\"result\":$res}" >>"$OUT/ceiling.json"
done

echo "]" >>"$OUT/ceiling.json"
echo
echo "wrote $OUT/ceiling.json"
