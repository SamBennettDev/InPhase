# Release metric: glass-to-glass latency (report §20, §21)

Browser `getStats()` + `requestVideoFrameCallback` decompose the *pipeline*;
they are **not** photon-to-photon truth. The release number comes from a
physical fixture.

## Fixture
- High-speed camera (≥240 fps) framing both the host monitor and the client
  display, **or** a photodiode on each screen + a 2-channel scope.
- Host shows a full-screen flashing rectangle driven by vsync.
- Measure host-flash → client-flash delta over ≥100 events; report median + p95.

## Input-to-photon (track separately, §21)
- Instrumented input device (or a relay on a mouse button) toggles a light and
  sends the click; measure light → on-screen response. Game
  simulation/render/display latency is additional to InPhase and must be noted.

## Working target (hypothesis, not a promise — §21)
Well-tuned 120 Hz wired path: ~20–35 ms median capture-to-display. Validate; do
not publish until the fixture exists.
