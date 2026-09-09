# InPhase architecture overview

A map into the code. The design report in `../research/` has the full rationale;
section references in the code (`§5.1`, …) point back to it.

## Data path

```
                         INPHASE HOST — WINDOWS USER SESSION
 Browser HTTP/HTTPS        Rust host runtime (crates/host)
 ───────────────────►  ┌──────────────────────────────────────────┐
                       │ Axum web server / pairing / sessions      │  http/  pairing/  session/
                       │ WebRTC signaller adapter                  │  media/webrtc/
                       │ Stats + policy                            │  stats/  media/encoder_policy.rs
                       │ Input state + Windows backend             │  input/
                       └───────────────┬──────────────────────────┘
                                       │ build/own while a player is connected
                                       ▼
 Desktop GPU  ──DXGI──► D3D11 capture ─► D3D11 convert ─► HW encoder ┐   media/pipeline.rs
                                                                    │
 Windows audio ─WASAPI loopback─► Opus ─────────────────────────────┤   (Phase 2)
                                                                    ▼
                                                         GStreamer webrtcbin
                                                         DTLS/SRTP
                                                              ↕  LAN / direct ICE host candidate only
                                                        Browser RTCPeerConnection
                                                         ├─ video + audio
                                                         ├─ input datachannel   (unreliable/unordered)
                                                         └─ control datachannel (reliable)
```

## Module responsibilities (report §3.1)

| Module | Owns | Must not own |
|--------|------|--------------|
| `app::HostRuntime` | boot, config, lifecycle, shutdown | codec logic, capture details |
| `http` | static assets, pair/status APIs, authenticated signaling | media packets |
| `pairing::PairingManager` | PIN generation, expiry, rate limit, session cookie | WebRTC negotiation internals |
| `session::SessionManager` | exactly one active `PlayerSession`; state transitions | the media framework |
| `media::MediaSession` | one GStreamer pipeline + its `webrtcbin` consumer | authentication policy |
| `media::encoder_policy` | codec selection + low-latency encoder properties | vendor SDK implementations |
| `input::InputSession` | decode packets, sequence/state/watchdog | browser DOM handling |
| `input::backends` | SendInput / virtual-HID adapter | transport |
| `stats::StatsCollector` | normalise host + WebRTC + client telemetry | UI rendering |
| `platform::windows` | monitor/GPU enumeration, tray/startup/firewall | product/session policy |

The discipline (§3.1): these stay **adapters around real primitives**, never a
generic streaming framework.

## Session state machine (report §15) — `session/mod.rs`

```
IDLE ─pair─► PAIRED ─caps─► NEGOTIATING ─ICE/DTLS─► PLAYING
                                                     ├─ transient ICE loss ─► RECONNECTING ─► PLAYING
                                                     └─ stop/timeout/failure ─► STOPPING ─► IDLE
```

Rules enforced: one `PlayerSession` owns the one `MediaSession`; a second play
request gets `BUSY`; entering `STOPPING` releases all input immediately; the
pipeline is destroyed before returning to `IDLE`.

## Latency discipline (report §2.1, §5.1, §8.2, §10)

- At most **one** raw frame queued; stale frames dropped (`queue leaky=downstream
  max-size-buffers=1`).
- Frames stay in `memory:D3D11Memory` until encoder input — no avoidable CPU copy.
- Hardware encoder only; **no** silent x264 fallback.
- No B-frames, no lookahead, zero-latency rate control (`media/encoder_policy.rs`).
- `media::bitrate` (application-level AIMD, from client telemetry) varies
  **bitrate** before queues grow; resolution/FPS preserved (ADR-010).
- No receiver jitter buffer — `RTCRtpReceiver.jitterBufferTarget = 0` on the
  **audio and video** receivers (the browser syncs A/V by holding video to
  audio's playout point, so audio must target 0 too). Not a setting. Browsers
  may still enforce their own floor (Safari ~2 frames; Chrome honours 0);
  true presentation cadence is measured with `requestVideoFrameCallback`.
- Latency + queue depth are first-class metrics (`stats/`, `/api/v1/admin/status`).

## Codec policy (report §7) — `http/signal.rs::choose_config`

1. probe host hardware encoders (`platform::enumerate_gpus`)
2. read `RTCRtpReceiver.getCapabilities("video")` — never invent codec names
3. `MediaCapabilities.decodingInfo` for smoothness/power
4. HEVC only when host encode + RTP receive + decode are all positive; else H.264
5. `setCodecPreferences()` before answering
6. verify actual `inbound-rtp` codec after connect

## Security posture (report §14, ADR-009)

- HTTPS from a **bundled local CA**: the installer trusts the root on the host
  PC; other devices install it once from `GET /ca.crt`. Whole-origin HTTPS/WSS,
  so the browser gets a secure context (WebCrypto, Keyboard Lock, gamepad).
- Pairing: 6-digit PIN (rotates on pair) **plus** a non-extractable per-browser
  Ed25519 key enrolled in the host's controller ACL. HttpOnly/SameSite cookie
  authenticates the signaling WebSocket; global + per-/64 rate limiting; strict
  CSP; admin API loopback-only.
- Remote access is **off by default** (`remote_access_gate`). When on, pairing
  stays LAN-only unless `allow_remote_pairing` is also set; a remote device
  authenticates by signing a per-connection challenge with its enrolled key.
  See [`../lan-security-model.md`](../lan-security-model.md).
