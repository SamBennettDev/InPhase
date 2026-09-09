# ADR 0010 — No automatic resolution/FPS mitigation in the first WebRTC policy

**Status:** accepted · **Report:** §8.2, §9, §22, §27

## Decision
Congestion control (`crate::media::bitrate`): application-level AIMD from client
telemetry; retransmission on, FEC off
on wired (auto on lossy Wi-Fi after validation), `enable-mitigation-modes=none`.
GCC varies **encoder bitrate** first, preserving requested resolution/FPS. If the
link cannot sustain the floor, prompt or step quality per preset policy rather
than accumulating latency. Never let a raw/video queue grow to "smooth" over
congestion — drop stale frames.

## Revisit trigger
Measured Wi-Fi results show app-level resolution/FPS step-down is needed for a
good experience.
