# ADR 0011 — WebTransport + WebCodecs video transport for internet mode

**Status:** accepted for implementation · **Report:** §8, §10, §27 · **Supersedes nothing; extends ADR-0006**

> **Progress (P0-P2 code complete; P3/P4 + hardware validation pending):**
> P0 spike verified on loopback (119.5 Mbps @ 0% datagram loss, RTT ~1.5 ms,
> H.264 software decode 1600+ fps; hardware-decode gate still needs a
> real-Chrome run). P1 landed: the wire format (`protocol/src/wtvideo.rs`,
> golden-vector tested), the host transport (`host/src/media/wt.rs`, token
> auth / one-active-session / leaky frame queue, Rust↔Rust loopback tested),
> the encoder tap, signaling advertisement (`WtVideoInfo`), and the UDP
> firewall rule — the Windows encoder call site is wired but not yet exercised
> on Windows hardware. P2 landed: `web/src/wtvideo.ts` (TS wire-format mirror
> pinned byte-for-byte to Rust via generated golden vectors,
> `dump_wtvideo_vectors.rs`), `web/src/wt.ts` (WebTransport dial with
> certificate-hash pinning, token auth, datagram reassembly, control stream,
> RTT ping), `web/src/wtdecoder.ts` (WebCodecs decode + canvas + adaptive
> anti-jitter-buffer playout: ≤1 frame pending, drop-don't-buffer, adaptive
> hold 0-40 ms), and player wiring that swaps canvas-over-<video> on the first
> WT frame and reverts on any failure. P3 (NACK/AIMD feedback) and P4
> (fallback matrix, dashboard parity) remain. P3 landed: NACK re-sends
> (bounded recent-fragment cache on the host; client asks at most twice per
> frame, ≥5 ms apart, only while the frame is under 30 ms old — verified e2e
> for a two-fragment loss beyond XOR recovery), and once-a-second WT
> `client_telemetry` that feeds the host's existing AIMD congestion controller
> identically to the WebRTC path. A stale-fragment gate (late parity recreating
> ghost pending frames) was caught by the TS mirror tests and fixed in both
> implementations. P4 landed: fallback-matrix integration tests (advertisement
> presence/absence per host config; advertised port/hash/token match the bound
> transport; tokens fresh per session and one-time against a real dial),
> dashboard parity (`video path` carrier cell + `wt_active` in the stats
> snapshot + the public `wt` offer in `/api/v1/status`), a `doctor` check for
> the WT UDP port, and the playout-delay control law extracted into a pure
> `PlayoutPolicy` (unit-tested: grow on underrun/eviction, shrink after a
> 30-frame streak, clamped). The firewall/installer surface is complete (UDP
> rule added to boot + tray paths; the cert needs no installation by design —
> the hash pin is the trust anchor). Code-complete for P0-P4; validation that
> still requires real hardware: a real-Chrome `isConfigSupported` decode bench,
> the Windows encoder tap, and an end-to-end internet latency measurement.

## Decision

Add a second video transport alongside WebRTC, for sessions where the path is
the internet rather than a quiet LAN:

- **Media**: encoded video frames travel as **WebTransport (HTTP/3 QUIC)
  datagrams** — unreliable, unordered, no browser jitter buffer in the path.
- **Decode + present**: the client decodes with **WebCodecs `VideoDecoder`**
  and presents to a canvas under its **own deadline scheduler** — an explicit
  implementation of the same leaky-queue-of-one discipline the host pipeline
  enforces (`queue leaky=downstream max-size-buffers=1`).
- **Everything else stays on WebRTC**: signaling, audio (Opus/NetEQ), the
  `input` data channel (unreliable/unordered, snapshot semantics), the
  `control` channel, and — critically — **video as the automatic fallback**
  whenever WebTransport is unavailable, blocked, or the host is unreachable.
- WebRTC remains the default transport in LAN mode. Internet mode is a host
  setting (ADR-0009 style mode), off by default.

## Why

The WebRTC receiver's jitter buffer is not application-controllable from
above: `jitterBufferTarget`/`playoutDelayHint` set a *floor*, and Chrome's
adaptive controller raises the effective buffer to ~50-130 ms whenever the
path shows arrival jitter and loss (measured 2026-09-05: 7 ms jitter, 13 ms
RTT, 277 lost packets → `jitter_buffer_target_ms ≈ 51`, per-frame wait p50
~110 ms, capture-to-glass p50 ~145 ms at 46/60 presented fps). No API caps
it. A custom playout path sizes the buffer from *measured* jitter instead:
at 7 ms jitter, 15-25 ms suffices.

Expected budget on the same measured path:

| Segment | WebRTC today | WT + WebCodecs |
|---|---|---|
| encode (host, p50) | ~5 ms | ~5 ms |
| transit (one-way) | ~9 ms | ~9 ms |
| playout buffer | ~110 ms | ~15-25 ms |
| decode | ~2.5 ms | ~2.5 ms |
| vsync wait | ≤ 16.7 ms | ≤ 16.7 ms |
| **glass-to-glass** | **~145 ms** | **~50-70 ms** |

On a wired LAN both transports converge (~25-35 ms); WT ships behind a flag
so the LAN path's proven behaviour is untouched.

## Design

### Transport

- **Endpoint**: HTTP/3 listener on a UDP port (configurable; installer adds
  the firewall rule alongside the existing TCP rule).
- **Auth**: QUIC dial carries no WebRTC-style handshake — a fresh WT session
  is untrusted. The signaling WebSocket (already authenticated: PIN pairing +
  per-browser Ed25519, ADR-0009) issues a **one-time session token**; the
  client's WT *control stream* presents it as its first message. The host
  drops datagrams and refuses media until the token validates, and binds the
  WT session to that signaling session (one active player, ADR-0007).
- **Certificates**: prefer the bundled local CA (already trusted on paired
  devices, §14) for normal TLS validation. `serverCertificateHashes`
  (SHA-256 pin, delivered over the authenticated WSS) is the fallback for
  devices that never installed the CA.
- **Reachability**: QUIC dials *toward the host* — no ICE. The host must be
  directly reachable (public IPv6/IPv4, port-forward, or UPnP). Unreachable
  → automatic WebRTC fallback. **No relay** (keeps ADR-0006's no-TURN
  property); relayed internet mode stays on WebRTC.

### Framing

- Encoder output is tapped **before** the RTP payloader (same encode chain,
  encoder policy, and bitrate AIMD). Frames are split into ≤ ~1300-byte
  fragments (PMTU-safe, no IP fragmentation): `{frame_no, frag_idx,
  frag_count, ts_us, flags:keyframe, payload}` plus a datagram seq for loss
  accounting.
- **FEC**: XOR parity fragment per frame group in v1 (one fragment loss per
  group survives without a round trip); pluggable for Reed-Solomon later.
- **Recovery**: the client sends per-frame fragment bitmaps on the WT control
  stream; the host retransmits only what is still before the client's
  playout deadline (client marks "still wanted"). Lost keyframes or oversized
  gaps trigger an existing keyframe request.
- **Congestion**: QUIC's CC is invisible to the app for datagrams, so the
  existing application-level AIMD (`media::bitrate`, client telemetry →
  bitrate before queue growth) keeps that job with reception stats from the
  bitmap reports.

### Client pipeline

- `wt.ts` — connect, control stream, datagram pump, reassembly, loss stats,
  clock sync (reuse ping/pong + host frame stamps from `diag.ts`).
- `scheduler.ts` — measures display refresh (reuse `measureRefresh`), decode
  EMA, arrival-jitter EMA; deadline = next rAF tick minus decode estimate;
  presents the newest complete frame, drops stale ones, requests a keyframe
  when the gap exceeds recovery threshold.
- `VideoDecoder` configured from `session_config` (hvc1/avc1, hardware-
  preferred, `isConfigSupported` gate — HEVC stays opportunistic per
  ADR-0005). `VideoFrame.close()` on every consumed frame to bound GC.
- Render: canvas2d `drawImage(VideoFrame)` in v1; WebGPU only if profiling
  demands it.
- Telemetry: the §20 metrics keep their field names — `jitter_buffer_delay_ms`
  reports the scheduler's measured wait — so the dashboard and HUD parity
  hold across transports.

### Fallback matrix

| Condition | Behaviour |
|---|---|
| No `WebTransport` / `VideoDecoder` (Safari < 26.4, old browsers) | WebRTC video, as today |
| Host not reachable over QUIC (hard NAT, UDP blocked) | WebRTC video, as today |
| WT connects then degrades (loss > threshold, freeze) | client-initiated revert to WebRTC mid-session |
| LAN mode, WT disabled | WebRTC video, as today |

Browser support (caniuse, 2026-09): WebTransport — Chrome/Edge 97+, Firefox
114+, **Safari 26.4+**; WebCodecs is much older (Safari 16.4+). The fallback
is load-bearing, not theoretical.

## Phases

| Phase | Scope | Effort |
|---|---|---|
| P0 — spike | Throwaway page: WT datagram throughput loopback; `isConfigSupported` for hvc1/avc1 on target GPUs; canvas present path. Gates: sustained ≥ 60 Mbps datagrams, hardware decode confirmed | 0.5-1 d |
| P1 — host sender | Encoder tap, framer + FEC + seq, control-stream auth + protocol messages, session_config advertisement, firewall/installer rule | 1-2 d |
| P2 — client pipeline | wt.ts + reassembly, decoder, deadline scheduler, canvas, HUD wiring | 1-2 d |
| P3 — recovery | Fragment bitmaps/retransmit, keyframe requests, AIMD feed, freeze/latency telemetry parity | 1-2 d |
| P4 — hardening | Fallback matrix, cert paths + rotation, dashboard parity, docs, ADR-0009 mode wiring | 1-2 d |

Total ≈ 5-8 working days for a flag-gated v1.

## Risks

- **Safari < 26.4 has no WebTransport** — fallback covers it; revisit when
  adoption is broad.
- **QUIC/UDP blocked** on hostile networks — fallback covers it; prefer a
  443/UDP port for the listener to maximise pass-through.
- **Datagram throughput / PMTU variance** — P0 gates it; fragments stay
  PMTU-safe by construction.
- **HEVC hardware decode variance in WebCodecs** — `isConfigSupported` gate
  + H.264 fallback (encoder policy already opportunistic).
- **Two transports in one session** — complexity is contained by keeping
  WebRTC authoritative for everything except video.
- **GC pressure from VideoFrames** — explicit `close()` discipline, verified
  in P2 profiling.

## Revisit trigger

If measured glass-to-glass over WT on a stable path exceeds the WebRTC LAN
result by more than 15 ms, or P0 fails a gate (throughput / hardware decode),
stop and reconsider: tunnelling only the playout decision (keep RTP, use
insertable streams) is not equivalent — frames still pass through the jitter
buffer upstream — so the alternative would be abandoning browser decode for
a WebAssembly decoder, which is a much larger cost for the same physics.
