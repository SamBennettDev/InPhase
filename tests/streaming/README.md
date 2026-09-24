# Streaming target-validation harness

Proves whether a stream actually held a target — resolution, frame rate and
bitrate — consistently, with no drops, freezes or errors. It drives a **real
browser** as the client and asserts against explicit tolerances, rather than
printing numbers for someone to eyeball.

`tests/latency/harness/` measures *where latency goes*. This harness answers a
different question: **did it hit the numbers, and did it stay there.**

## Cross-browser matrix (`xbrowser.mjs`, `matrix.sh`, `netem.sh`)

```sh
cd tests && npm install          # playwright pinned to the installed browsers
streaming/matrix.sh "chromium firefox" "clean jitter loss1 burst wifi cap40" 60
```

`xbrowser.mjs` drives one Playwright engine through the real UI (pair, Connect)
and samples both ends each second - the page's decode counters and the host's
admin status (encoder target, the client telemetry the host received). It fails
a run on any second with no decoded frame, a WT redial, a page error, a sagging
decode rate, or (clean profile) the encoder not reaching and holding the set
bitrate. `netem.sh` impairs only UDP from the host (ingress via ifb, needs
sudo); `matrix.sh` owns the SSH forward to the admin API and clears netem on
exit. Motion on the host matters: a static desktop sends <1 Mbps and exercises
nothing - run Heaven *windowed* (exclusive fullscreen captures black).

Engine notes (this Linux box): Chromium and Firefox carry the full WT path;
Playwright's WebKit has no `WebTransport` and can only check the refusal
message. Chromium runs with GPU canvas but software decode (VA-API fails this
stream on RADV). Headless Firefox has no WebGL, so its decode runs are
client-limited, and the harness says so instead of passing silently. Results
are only meaningful on an idle client: a loaded laptop reads as decoder stalls.

### Real client: the Mac (`mac.sh`)

`mac.sh <label> [duration] [settings]` drives Chrome on the owner's M4 MacBook
Air (Wi-Fi, hardware HEVC decode) over CDP through an SSH tunnel. Start that
Chrome once on the Mac, in its own profile:

```sh
ssh "$INPHASE_MAC" 'open -na "Google Chrome" --args \
  --remote-debugging-port=9223 --user-data-dir=/tmp/inphase-cdp --no-first-run'
```

The first navigation after launch can fail `ERR_ADDRESS_UNREACHABLE` while
macOS's Local Network permission settles; retry. 2026-09-24, 1440p120 /
80 Mbps, 3 min: 118 fps p50, 80 Mbps held 100 %, capture->canvas 18.6 ms,
2 brief freezes.

## Run

```sh
cd tests/streaming

# can this client decode the target at all? (no host needed)
./sweep.sh 9555 out

# end-to-end against a real host
INPHASE_TEST_PIN=123456 ./run.sh 4k120-lan https://192.168.1.100:47800 120 \
  '{"width":3840,"height":2160,"fps":120,"maxBitrateKbps":80000,"preset":"low_latency"}'
```

`run.sh` exits non-zero unless every check passes. `INPHASE_MODE=ozone` switches
the client mode (see "The client-side wall"). Deps: `npm install` in `tests/`.

| file | what it does |
|---|---|
| `run.sh` | end-to-end: display → client → probe → assert (+ optional host slice over SSH) |
| `verify.mjs` | asserts a `cdp-probe` summary against a target; writes `<label>.verify.json` |
| `sweep.sh` | decode ceiling across resolutions/frame rates; writes `out/ceiling.json` |
| `wccap.mjs` | WebCodecs decode benchmark — the client's real decode ceiling |
| `gpucheck.mjs` | proves hardware (not software) video decode, over CDP |
| `client.sh` | launches the browser under the virtual display with a GPU path |
| `xvfb.sh` | brings the sized virtual display up/down |

## Measured: this client (`cinix-hp-lap`, Ryzen 3 4300U / Radeon Vega 6)

Hardware **H.264** decode via WebCodecs, 240-frame clips, steady state:

| mode | decoded | fps | cores | vs realtime |
|---|---|---:|---:|---:|
| **3840×2160@120** | 240/240 | **128–139** | 1.07 | **1.07–1.16×** |
| 2560×1440@120 | 240/240 | 237 | 1.04 | 1.97× |
| 1920×1080@120 | 240/240 | 447 | 1.08 | 3.72× |

**4K120 is decodable, but only just** — 7–16% headroom. The decoder keeps up;
there is very little margin left for network ingest, canvas draw and compositing
on top. Expect 4K120 to be marginal on this box and 1440p120 / 1080p120 to be
comfortable. That is a finding about the hardware, not about InPhase's code.

For reference, at the driver level `ffmpeg` VA-API does the same 4K120 clip at
185 fps using 0.20 s of user CPU, versus 3.11 s software — the ~15× CPU gap is
the hardware/software discriminator `wccap.mjs` reproduces in the browser.

## Measured end to end, over WebTransport (2026-09-20)

```
run.sh wt-1440p120 https://<host>:443 90 \
  '{"width":2560,"height":1440,"fps":120,"maxBitrateKbps":80000,"preset":"low_latency"}'
```

| check | wanted | measured |
|---|---|---|
| negotiated resolution | 2560×1440 | **2560×1440** ✓ |
| stream dimensions stable | 0 changes | 0 ✓ |
| decoded / presented fps | 120 | **32.4** ✗ |
| decode time p50 | — | **407 ms** ✗ |
| wire latency p50 | — | **2.4 ms** ✓ |
| frames dropped | 0 | 89–132 ✗ |
| freezes | 0 | 0–1 |

**The synthetic ceiling below is not the end-to-end ceiling.** 237 fps at 1440p
was a bare decoder loop — no rendering, no compositing, easy content. Driving the
real pipeline (decode *plus* canvas present at 2560×1440) costs **407–530 ms per
frame** and lands at ~32 fps. A decoder microbenchmark predicts capability, not
throughput; only this harness measures the number the user actually gets.

**4K120 @ 80 Mbps is not reachable on this box, and neither is 1440p120.** The
wall is software H.264 decode: Xvfb provides no DRI3, so there is no hardware
decode path, and `ozone` cannot be substituted (see the next section for why the
two cannot both be had).

### Two things the harness had to be taught to see

1. **It measured the wrong transport.** Every telemetry global is WebRTC-shaped
   (`pc.getStats()`, `<video>`). The WebTransport path is the primary transport
   and renders to `<canvas class="wt-video">`, keeping its own counters on
   `__inphaseWt` / `__inphaseWtWire`. Measuring only the WebRTC surface reported
   a fully working WT stream as **0 frames** — worse than no harness, because it
   reads as a broken pipeline. `cdp-probe.mjs` now reads both.
2. **It never pressed Connect.** Pairing only reaches the home screen; the stream
   starts on the `#connect` click. Without it the probe paired, then idled and
   reported 0 frames for the whole run.

### Known stall (fixed 2026-09-23)

The `sent=0 … evicted=121` stall was the WT redial being dropped: the client
asks for a fresh dial over the signaling socket after its WT connection
closes, and the host answered that request only on the retired WebRTC data
channel. Any close was therefore permanent. It is answered on the socket now
(`crates/host/tests/wt_fallback.rs` pins it), and the close reason is logged.

## The client-side wall (important)

On this box the two things a test needs **cannot currently be had at once**:

| client mode | hardware decode | frame telemetry (rVFC) |
|---|---|---|
| `x11` (Xvfb) | ✗ software, ~23 fps at 4K120 | ✓ |
| `ozone` (`--ozone-platform=headless`) | ✓ 128–139 fps at 4K120 | ✗ no compositor |

Under Xvfb, Chrome logs `dri3 extension not supported` (`gbm_support_x11.cc`) and
falls back to software decode; a `<video>` element then plays 4K120 at **22.9 fps
while dropping 913 frames and burning 2.37 cores**. Chrome's Linux VA-API import
needs DRI3/GBM for zero-copy surfaces. With `--ozone-platform=headless` there is
no X11 layer, the warning disappears and hardware decode works — but with no
compositor, `requestVideoFrameCallback` never fires, so the client's own frame
telemetry is empty.

Getting both needs a **real GPU-backed desktop session** on this machine. GDM
holds the DRM master (a Wayland `gnome-shell` session), so a second X server
cannot take the GPU, and the greeter's display needs Xauth we do not have. Log
into the desktop on the box (or run the harness inside that session) to measure
presentation and 4K120 together.

Also note: `Xvfb` reports refresh `0.00`, so *presentation* timing is not
physical here regardless — resampled present-to-glass figures from this display
should not be quoted.

## Finding: the client is not always the bottleneck — verify the tool first

An early version of `wccap.mjs` reported 4K120 as undecodable ("Decoding error"
after 3 frames), which looked like a hard hardware wall. The cause was the test
clip, not the GPU: `-c:v copy` from an MP4 leaves SPS/PPS in the `avcC` box, so
the raw stream carried no parameter sets and the decoder had no configuration.
`h264_mp4toannexb` fixes it. **If a decode test fails, check the bitstream before
concluding anything about the hardware.**

## Host-side prerequisites

`run.sh` needs either a live pairing PIN, or `HOST_SSH` for the host-side
frametrace slice. The existing harness conventions apply
(`tests/latency/harness/README.md`): host-side steps are PowerShell `.ps1` files
run with `powershell -File`, because quoting over SSH breaks otherwise.
