# Moonlight/Sunshine replacement readiness

InPhase can replace the host and player roles for supported Windows-to-browser
sessions. Complete replacement requires both sustained gaming performance on
real devices and the features each user relies on. The October 13 pipeline
fixes remove demonstrated sources of input duplication and recovery delay;
they do not establish latency or feature parity with Moonlight/Sunshine.

## Current path and remaining coverage

| Area | Implemented | Remaining replacement work |
| --- | --- | --- |
| Capture and encoding | Windows desktop capture, D3D11 NV12 conversion, hardware H.264/HEVC, low-latency encoder policies, one-frame raw queue | Validate NVIDIA/AMD/Intel drivers under game load; AV1 and HDR/10-bit formats are absent |
| Player | Browser WebTransport/WebCodecs, worker receive path, ordered decode, bounded pacing, canvas presentation | Validate actual hardware decode and display cadence at each target; native desktop/TV clients and GameStream compatibility are absent |
| Loss recovery | Interleaved fragments, XOR FEC, fair NACK rounds, adaptive deadlines, bounded host repair credit | Test real bandwidth limits, congestion and wireless bursts; fixed-size virtual datagrams cannot establish throughput or image quality |
| Keyboard/mouse/controller | Windows input injection and virtual Xbox 360 gamepad via ViGEmBus; duplicate copies apply once | Validate real games, controller mappings and reconnects; controller rumble feedback is absent |
| Audio | 48kHz stereo Opus path | Test audio underruns and A/V drift during long sessions; surround sound is absent |
| Connection | LAN pairing, session replacement, opt-in reachable IPv6 remote access | Test reconnect/suspend/resume across browsers; no hosted relay or Wake-on-LAN |
| Host platforms | Windows host | Linux/macOS capture and input use stubs |

Relevant sources: `crates/host/src/media/pipeline.rs`,
`crates/host/src/media/encoder_policy.rs`, `crates/protocol/src/signaling.rs`,
`crates/host/src/input/backends/virtual_hid.rs`, `web/src/wtdecoder.ts`,
and [remote-access requirements](ipv6-remote-access.md).

## Performance acceptance before claiming parity

Use the same gaming PC, player, display refresh rate, game scene, resolution,
FPS, codec and bitrate for InPhase and Moonlight/Sunshine. Keep pacing settings
in the result so a smoother picture's buffering cost is visible.

1. Verify hardware encode/decode and that the negotiated resolution/FPS stay
   constant. Measure presented FPS and p95/p99 frame intervals as well as
   decoded FPS; a decoder counter alone cannot prove smooth display.
2. Measure input-to-screen p50/p95/p99 with an external camera or optical rig.
   Browser timestamps and QUIC write completion do not measure physical latency.
3. Run clean LAN, Wi-Fi jitter, 1%/5% loss, burst loss, bandwidth caps and an
   outage, with game load active. Check repair rates, queue growth, recovery
   gaps, encoder/client load and image quality at the same settings.
4. Soak for at least an hour with stereo audio and controller input; record
   audio underruns, A/V drift, dropped frames and stalls. Exercise 100 reconnects
   plus sleep/resume, confirming no stuck inputs or stale-session actions.
5. Repeat on the supported host GPU and player/browser matrix. Treat a mode
   as supported only when its presentation and latency gates pass on hardware.

The existing [streaming harness](../tests/streaming/README.md),
[latency harness](../tests/latency/harness/), and
[host/GPU matrix](../tests/compatibility/host-gpu.md) provide the procedures.
The [pipeline follow-up](../tests/streaming/PIPELINE-2026-10-13.md) records the
automated evidence available from this Linux environment without a GPU.
