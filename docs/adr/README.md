# Architecture Decision Records

One file per decision from the InPhase Deep Research Architecture Report §27.
Each records the decision, the rationale, and the **revisit trigger** — the
specific measured result that would justify reopening it.

| ADR | Decision | Revisit trigger |
|-----|----------|-----------------|
| [0001](0001-rust-host-ts-browser.md) | Rust host, TypeScript browser | A required Windows/media API is materially blocked by bindings |
| [0002](0002-gstreamer-1.28.md) | GStreamer 1.28.6 stable | Upgrade only after the compatibility regression suite passes |
| [0003](0003-dxgi-desktop-duplication.md) | DXGI Desktop Duplication primary | WGC benchmark shows better correctness/latency for target cases |
| [0005](0005-h264-baseline-hevc-optional.md) | H.264 baseline; HEVC opportunistic | Browser/GPU matrix makes another codec broadly superior |
| [0006](0006-no-stun-turn-lan.md) | No STUN/TURN in LAN mode | Scope expands beyond direct LAN/VPN reachability |
| [0007](0007-one-active-player.md) | One active player | Multi-viewer becomes a real product requirement |
| [0008](0008-input-injection.md) | SendInput + optional Virtual HID | Windows adds a supported user-mode gamepad API, or the chosen backend fails product gates |
| [0009](0009-security-modes.md) | Trusted-LAN HTTP MVP, secure-origin production mode | Security/offline requirements are explicitly reprioritised |
| [0010](0010-no-auto-resolution-mitigation.md) | No automatic resolution/FPS mitigation in the first WebRTC policy | Measured Wi-Fi results show app-level step-down is needed |
| [0011](0011-webtransport-webcodecs-video.md) | WebTransport + WebCodecs video transport for internet mode (WebRTC fallback) | WT glass-to-glass exceeds the WebRTC LAN result by > 15 ms, or a P0 gate fails |
