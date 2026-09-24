# InPhase performance measurement — 2026-09-01

Host: test PC, RTX 3070, capture = SudoMaker virtual display 2560×1440@144.
Stream config: H.264, 2560×1440@60, 40 Mbps cap, `webrtcbin` path, preset
`low_latency`. Client: Chrome 152. Every number below is measured per-frame
(host stamps on the `control` channel + `requestVideoFrameCallback` on the
client), keyed by RTP timestamp — the completed frametrace feature.

## Headline

| leg | p50 | p95 | note |
|---|---:|---:|---|
| capture → encoder output | **3.1 ms** | ~5 ms | occasional 10–13 ms under load |
| RTP payloader | 0.1 ms | 0.2 ms | |
| network (loopback / same-host LAN IP) | **~1 ms** | ~2 ms | |
| **receiver jitter buffer** | **~110 ms** | ~122 ms | **dominates the budget; see finding 1** |
| decode (RTX 3070 HW) | 0.4 ms | 0.6 ms | |
| compositor → glass (`present`) | 6.9 ms | — | 145 Hz panel |
| **capture → glass total** | **~124 ms** | **~127 ms** | |

Frame pacing (client, 60 fps content on a 145 Hz display):
- presented-interval p50 13.9 ms / p95 20.9 ms — this is the normal 2‑vs‑3
  refresh beat, **not** stutter.
- pacing jitter (stddev of interval): **4–5 ms**.
- skipped frames: **2–6 per 90–150 s run** (≈0.02–0.04/s).
- packet loss 0, freezes 0, decoder-missing frames 0, decoded fps a solid 60.

Host send cadence (gap between marker packets): p50 16.7 ms, p95 18 ms,
max ~23 ms. Clean — the uneven NVENC pacing noted earlier is no longer visible.

## Finding 1 — the jitter buffer is the whole latency story, and the client control does nothing

Every run parks at jitter-buffer delay ≈ **100 ms** (getStats
`jitterBufferDelay`), target ≈ **109 ms**, regardless of the "Jitter buffer (ms)"
setting:

| setting | measured jbuf p50 | getStats target |
|---|---:|---:|
| buffer = 120 ms (idle) | 115 ms | 109.7 |
| buffer = 0 ms (idle) | 126 ms | 109.8 |
| buffer = 120 ms (under load) | 118 ms | 108.9 |
| buffer = 0 ms (under load) | 118 ms | 108.8 |

`web/src/webrtc.ts:113` sets `receiver.jitterBufferTarget = targetMs` and
`playoutDelayHint = targetMs/1000`. With the setting at 0 both are set to 0, yet
Chrome still holds ~100–110 ms. Either the assignment is silently rejected (it's
wrapped in `try/catch {}`) or Chrome is clamping to a floor. **~110 ms of the
~124 ms glass-to-glass is buffer that the current knob cannot move.** The
pipeline underneath it is ~15 ms (3 encode + 1 net + 0.4 decode + 7 present + a
few ms of frame quantisation).

Worth doing next: log the actual `jitterBufferTarget` read-back after setting it;
try `playoutDelayHint` alone; check whether the host is emitting a
`playout-delay` RTP header extension with a high minimum; and confirm the
`encoder_policy` low-latency target (40 ms) actually reaches the client
(`session_config.jitter_buffer_target_ms`).

## Finding 2 — encode latency is ~3 ms, not the ~6–15 ms recorded before

Earlier notes had encode p50 ~4 ms / p95 ~15 ms with 40–53 ms spikes. Clean
runs here show **encode p50 3.1 ms, p95 ~5 ms**. Two things changed:

- The 6.5 ms seen in the first "idle" runs was a **test artdefact**: the client
  window was showing its own capture (infinite hall-of-mirrors), which is a
  pathological encoder input. Once real content covers it (Heaven, or a
  foregrounded full-bleed video) encode drops to 3 ms.
- The 40–53 ms encode spikes did not recur in ~40 min of streaming. p90-of-windows
  reached ~10–13 ms under load; max 13 ms.

## Finding 3 — GPU load does not perturb the stream

Unigine Heaven 4.0 (720p, High, Moderate tessellation) alongside the stream:

| | GPU util | NVENC util | power |
|---|---:|---:|---:|
| stream only (idle desktop) | 17 % | 29 % | 25 W |
| stream + Heaven | 65 % | 12 % | 120 W |

Under that load: encode still 3.1 ms p50, send cadence still 16.7 ms p50,
**0 dropped / lost / missing frames**, capture→glass unchanged at ~124 ms.
(Heaven's shader cache can't be written — it lives in Program Files — so it
recompiles on every launch and only reached ~65 % GPU at 720p; a fullscreen
1440p run would push higher, but the pipeline headroom is already clear.)

## Finding 4 — the LAN network leg is free

- Chrome → `127.0.0.1`: net p50 0.5 ms
- Chrome → host's own LAN IP `192.168.1.100` (real NIC path): net p50 1.1 ms
- Linux box → `192.168.1.100` over the wire: one-way **0.3 ms**, RTT 1–3 ms

The network contributes ~1 ms. It is not a factor in the latency budget on LAN.

## Caveats

- `present` (compositor→glass) is only captured when the client window is
  unoccluded; under Heaven it reads 0, so those `capture→glass` figures are
  ~7 ms low (capture→"frame decoded & ready" rather than →photons).
- Client-side clock alignment uses ½ of the control-channel ping RTT; on
  loopback that leaves ±1–2 ms of noise on `net` (visible as the occasional
  negative sample). Fine at the 100 ms scale of the total; not good enough to
  trust sub-ms `net` differences.
- The one browser-over-the-wire run from the Linux box used software decode
  (no GPU in this box's headless Chrome) and choked — its decode/pacing/bitrate
  numbers are invalid; only its `net` and the host-side stamps are usable. A
  real remote-client latency number still needs a GPU browser on another machine.
- Glass-to-glass here is browser-instrumented, not an optical fixture
  (`tests/latency/glass-to-glass.md`). It decomposes the pipeline; it is not
  photon-to-photon truth.

## Bottom line

The InPhase pipeline is fast and rock-steady — ~15 ms of real work,
imperceptible jitter, zero loss, immune to a 65 %-GPU game running next to it.
The entire user-visible latency is one thing: a ~110 ms receiver jitter buffer
that currently ignores its own setting. Fixing/plumbing that knob is the whole
game for latency.

---

# Follow-up (same day): buffer set to 0

Root cause of "the setting does nothing": `settings.ts` loaded `bufferMs` with
`Number(s.bufferMs) || DEFAULTS.bufferMs` — a stored **0 became 120**, so 0 was
never actually applied. Fixed (`numOr` helper), default changed to 0, and
`webrtc.ts` now logs the receiver read-back.

With the fix, `receiver.jitterBufferTarget = 0` **is** honoured
(read-back `{jitterBufferTarget: 0, playoutDelayHint: 0}`):

| | buf = 120 | buf = 0 | buf = 0 + Heaven load |
|---|---:|---:|---:|
| getStats jitterBufferTarget | 109.7 ms | **13.4 ms** | 15.5 ms |
| getStats jitterBufferDelay | 101.8 ms | **8.4 ms** | 9.2 ms |
| measured jbuf wait p50 | 123.7 ms | **24.8 ms** | 21.8 ms |
| **capture → glass p50** | 132 ms | **38.6 ms** | **32.5 ms** |
| capture → glass p95 | 135 ms | 42 ms | 37 ms |
| skipped frames | 4 / 100 s | 3 / 100 s | 2 / 130 s |
| packet loss / freezes | 0 / 0 | 0 / 0 | 0 / 0 |
| pacing jitter | 4.1 ms | 4.9 ms | 5.2 ms |

**~93 ms cut, no measurable smoothness cost on the wired LAN / loopback path.**
The only visible tradeoff: an occasional single late frame (one c2g outlier of
~140 ms in 130 s under load) that a 120 ms buffer would have hidden. Clients on a
jittery link (wifi) can raise the buffer in settings; the default is now 0.

`jbuf wait p50` (24.8 ms) reads higher than getStats `jitterBufferDelay`
(8.4 ms) because it also includes the wait for the next compositor vsync
(≤ ~14 ms on the 145 Hz panel) + the rVFC callback — both agree the buffer
itself collapsed to single-digit ms.
