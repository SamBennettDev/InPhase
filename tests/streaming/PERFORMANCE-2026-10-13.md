# Stutter and recovery validation — 2026-10-13

The receiver had avoidable stutters under packet loss: late repairs distorted
its cadence estimate, and incomplete NACK rounds ran past its recovery budget.
The fixes improve these simulations substantially. They do not certify a real
Windows gaming session or guarantee smooth playback on every player.

Baseline: `20a807d0ef2900e0f35c4f887357c70a557b2c89`. Candidate: the changes
accompanying this report. Machine: Linux x86_64, four-vCPU quota, no GPU device.
Rust 1.98.1, Node 24.19.0, Playwright Chromium 153.0.8010.12. Aggregate data is
in [performance-2026-10-13.json](performance-2026-10-13.json).

## Receiver simulations

Seven network profiles × 60/120 fps × seeds 1/7/42 × 60 virtual seconds:
42 runs for each revision. The same harness exercises the production v4 FEC,
NACK planner and frame-order gate. Payloads are checked byte-for-byte, delta
chains must be contiguous, and memory/progress assertions run throughout.

Minimum admission and maximum gap below are the worst of six runs per profile.
Admission means ready for decode; these numbers do not measure encoded image
quality, actual FPS, QUIC throughput or capture-to-display latency.

| Profile | RTT / jitter / loss | Admission before → after | Maximum admission gap before → after |
| --- | --- | --- | --- |
| LAN | 4ms / 0 / 0 | 100% → 100% | 16.7ms → 16.7ms |
| Wi-Fi jitter | 20ms / 0–30ms / 0 | 100% → 100% | 34.2ms → 34.2ms |
| 1% erasure | 20ms / 0–10ms / 1% | 100% → 100% | 215.4ms → 154.7ms |
| 5% erasure | 40ms / 0–20ms / 5% | 19.76% → 99.88% | 1185ms → 305.5ms |
| WAN | 120ms / 0–50ms / 1% | 86.96% → 99.88% | 761.9ms → 299.4ms |
| Burst | 20ms / 0–10ms / 20ms erased every 2s | 99.89% → 99.89% | 76ms → 76ms |
| Outage | 20ms / 0–10ms / 1s outage | 97.75% → 97.75% | 1371.8ms → 1371.8ms |

The worst 5%-loss run previously resynced 60 times per minute; all six candidate
runs resynced zero times. The WAN profile previously reached 16 resyncs per
minute; the candidate had zero. Candidate peaks were 34 held frames of the
64-frame cap and nine partial assemblies of the 24-assembly cap.

The model uses synthetic v4 keyframes as well as deltas; the live host sends
keyframes reliably. Control-message transit and a 150-frame resend cache are
modelled, but real host resend-rate limits, bandwidth/congestion, encoder bursts,
browser event-loop stalls and audio are not. The byte pattern and these profile
settings are intentionally fixed: this is a reproducible receiver regression
test, not a prediction of a user's network.

## Actual QUIC through an impaired UDP path

`cargo test --locked -p inphase-host --test quic_impairment -- --nocapture`
uses the production transport, authentication, reliable video channel and
dedicated input stream through a userspace UDP proxy. Both directions receive
1% packet erasure, 0–12ms jitter and either 5ms or 50ms added one-way delay.

Both runs delivered all 120 synthetic frames with intact bytes and increasing
frame numbers. Dedicated input reached the host while video was active (21ms
and 67ms in the initial run). These are individual transport timings, not input
latency percentiles or Windows injection measurements. The test also exposed an
accept-loop shutdown race; prioritizing the published shutdown signal prevents
polling wtransport's closed-endpoint accept future, which otherwise panics.

## Actual browser decode and presentation

The production `WtDecoder` decodes and draws moving H.264, with 5ms base transit
and deterministic 0–25ms arrival jitter. Each run lasts ten seconds. Encoding
the fixture finishes before measurement. Browser frametime statistics cover
the most recent 300 displayed samples, using the measured page refresh rate to
count hitches; canvas/animation timestamps do not measure physical scanout.

| Mode | Pacing | Decoded / 10s | Canvas draws / 10s | Hitches / recent 300 samples | p99 frametime | Added pacing delay at end |
| --- | --- | --- | --- | --- | --- | --- |
| 640×360 / 60 | off | 598 | 593 | 56 | 49.9ms | 0 |
| 640×360 / 60 | on | 598 | 591 | 26 | 32.2ms | 25.0ms |
| 1080p / 60 | off | 598 | 572 | 38 | 37.0ms | 0 |
| 1080p / 60 | on | 598 | 556 | 21 | 45.0ms | 36.4ms |
| 1440p / 120 | off | 398 | 398 | 97 | 55.8ms | 0 |
| 1440p / 120 | on | 261 | 261 | 114 | 149.7ms | 40.0ms |

All runs produced visible pixels and no decoder/page errors. Pacing reduced
moderate-mode hitch counts, but did not improve every tail metric, and cannot
rescue the overloaded 1440p120 software path. This machine did not sustain
1440p120. A repeat under concurrent compilation also performed substantially
worse at 1080p60; run these diagnostics on an idle player.

For a jittery route, Frame pacing is worth trying at a mode the player can
sustain. Watch p99 frametime and displayed FPS as well as decoded FPS. A flat
60 decoded FPS can coexist with uneven presentation. An overloaded client needs
less decode/render work; extra buffering alone cannot solve that limit.

## Checks and remaining hardware work

- 143 web tests pass, including 14 combined impairment cases and focused cadence/
  NACK regressions. The four focused regressions fail against the original code.
- 201 Rust tests pass; formatting and Clippy with warnings denied pass.
- Web TypeScript/production build pass. Browser fixture and spec also pass a
  strict standalone TypeScript check.
- 30 existing browser UI cases pass; three added real-WebCodecs cases pass,
  including reordered delivery and recovery after a missing reference run.
- All 42 extended candidate receiver runs pass their profile admission,
  progress and memory gates.

No Windows GPU/driver, HEVC hardware decoder, actual game, ViGEm controller,
host audio, bandwidth-capped end-to-end session or optical latency was tested on
this machine. Use the existing host/GPU, queue, reconnect, audio and network
matrix on the user's actual devices before claiming the reported stutter is
resolved. Record device/browser, host GPU/driver, resolution/FPS, codec, network
and Frame pacing setting; that information was not available for this run.

Reproduction commands and scope are in [README.md](README.md#automated-tests-without-a-windows-host).
