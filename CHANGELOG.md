# Changelog

## Unreleased

- Pace each frame to its own interval instead of at a fixed rate. At 1440p120
  a fixed 18000/s with a 32-datagram burst needed 4 ms for an average frame and
  up to 30 ms for a busy one, against 8.3 ms: the send queue filled and evicted
  frames every few seconds (a Mac session: 32 evictions, 34 re-keys in 5.5 min,
  network clean). Each frame's datagrams now go out over 90 % of the frame
  interval - faster when frames are queued - never slower than the
  connection's pace. Same mode, 150 s: evictions 0, re-keys 15 -> 6 (vs the
  60 % spread first tried), freeze time 5.1 s -> 1.4 s.

- Lip sync. Audio used to chain packets from a fixed 60 ms lead, so it played
  60 ms plus the output device's latency behind the picture, and every late
  burst or host/client clock drift pushed it further behind until the 250 ms
  cap. Audio is now stamped on the video's capture clock (the encoder's
  measured PTS offset, pipeline.rs) and each packet is scheduled to be heard
  when its frame is seen: the video's capture->present age plus scanout, less
  the output device's latency - corrected continuously by dropping a 10 ms
  packet when behind or leaving a short gap when ahead. `window.__inphaseAudio`
  publishes heard vs seen age.

- Step the stream down when this device's decoder cannot keep up, frame rate
  first: 1440p120 -> 1440p60 -> 1080p60 -> 720p60. A decoder that was
  producing pictures and then fails twice within a minute is overloaded, not
  incompatible, so the player reconnects at the next mode, saves it, and says
  so - instead of rebuilding the decoder (and trying software decode of the
  same mode) until iOS killed the tab. Measured on an iPhone with Safari 27
  and a bare test page: Safari's own HEVC decoder fails after 306-613 frames
  at 1440p120 and runs clean at 1440p60 and 1080p120. A decoder that fails
  before its first picture still falls back to software (a driver problem).
- The ImageBitmap renderer is chosen only when it shows pixels a direct draw
  did not, not whenever the first frame happens to be dark.

- Step the stream down when this device's decoder cannot keep up, frame rate
  first: a hardware decoder that was producing pictures and then fails twice
  within a minute reconnects the session at the next mode - 120 -> 60 fps at the
  same resolution, then one resolution rung at a time (floor 720p60) - with a
  notice, and the mode is remembered for this browser. iPhone Safari's own HEVC
  decoder fails at 1440p120 (306-613 frames into a bare decode loop with no
  InPhase code; 90 s clean at 1440p60 and 1080p120), and the player's rebuild /
  software-fallback reaction to that got the tab killed ~25 s in. A decoder
  that fails before its first picture still falls back to software decode.
- Pick the canvas renderer by comparison, not by a dark first frame: the
  per-frame ImageBitmap path is only chosen when a direct VideoFrame draw
  shows nothing that the bitmap route does.

- Step the stream down when this device's decoder cannot keep up, instead of
  rebuilding it until the tab dies. A decoder that had been producing pictures
  and fails twice within a minute is overloaded, not incompatible: the player
  switches to the next mode - frame rate first (120 -> 60 at the same
  resolution), then resolution one rung at a time down to 720p - saves it, and
  says so. Safari's own HEVC decoder on an iPhone (Safari 27) fails after
  300-600 frames at 1440p120 with no InPhase code involved (a bare decode test
  page), and runs clean at 1440p60 and 1080p120; the player's rebuild loop
  turned that into iOS killing the tab ~25 s in. A decoder that fails before
  its first picture still falls back to software decode as before.
- The render-path probe no longer mistakes a dark first frame for a broken
  direct draw: it compares against the bitmap route before switching. The
  bitmap route costs a full-size RGBA allocation per frame.

- Serve the Let's Encrypt certificate within seconds of a host restart, not
  ~30 s. Until the ACME resolver is installed the public (IPv6) listener falls
  back to the local-CA leaf, and the check for the router mapping ran on a
  30 s interval with its first tick skipped - so every restart gave devices
  that trust only the public chain a half-minute of ERR_CERT_AUTHORITY_INVALID.

- Give up on an unrepairable hole on the RTT's schedule: ~220 ms on a 5 ms LAN
  instead of a fixed 300 ms, still inside the RTT-paced repair rounds. Each
  unrepairable burst froze the picture ~390 ms before re-keying (Mac,
  1440p120); the fixed budget waited out ~100 ms past the last useful repair.

- Two parity rows on large delta frames (>= 16 fragments), as keyframes already
  had. Browser-side datagram drops at 1440p120 came in bursts dense enough to
  put two holes in one 8-fragment group, which row 1 alone cannot rebuild; the
  rare frame whose re-send then missed its window cost a re-key. About 12.5 %
  more parity on those frames, zero added latency.
- Burst 32 datagrams at the worker pace instead of 90: 90 still overran
  Chrome's internal datagram queue on a Mac (3-10 % lost on some seconds with
  zero network loss). An ~80-fragment frame now spreads over ~3 ms.

- Stop losing datagrams inside the browser. With QUIC reporting zero packet
  loss, a Mac Chrome session at 1440p120 still lost 1-4 % of datagrams on some
  seconds - after arrival, in the browser's incoming-datagram queue, which has
  no flow control. The client now raises `incomingHighWaterMark` to 4096 and
  the host bursts 5 ms of pace (90 datagrams) instead of a whole frame
  interval (300); with the 1 ms timer a small burst costs nothing.
- Keep a second of re-sendable fragments at 120 fps (150 frames, was 60 = 0.5 s,
  shorter than the repair schedule: `nack_missed` 7-48/s).
- Ask for missing fragments on the RTT's schedule, not a fixed 150 ms: on a
  5 ms LAN the first request goes out at ~40 ms and four rounds finish inside
  the orderer's 300 ms budget, instead of freezing the frame for 150 ms first.

- Raise the worker datagram pace from 11000 to 18000/s. A Mac Chrome session
  at 1440p120 / 80 Mbps put 95-114 Mbps on the wire with zero network loss,
  but that is 9-10.4k datagrams/s against an 11k pace: every large frame
  backed the send queue up, the host evicted frames, and the client re-keyed
  every 0.5-2 s ("wt reference gap ... re-keying").

- Cut the host's own send latency from 8.6 ms (p50) / 28 ms (p95) to under
  0.1 ms, measured per frame on Cin-PC with motion:
  - Raise the Windows timer resolution to 1 ms at startup, and opt the
    windowless tray process out of timer throttling (Windows 11 ignores
    `timeBeginPeriod` for invisible processes otherwise). The pacer's "1 ms"
    waits were 15.6 ms ticks.
  - Size the pacer's burst to one frame interval of the pace (183 datagrams
    at the worker pace, 64 in-page), so a delta frame leaves at once and only
    keyframe-sized frames are spread out.
- Remove `videorate` from the capture chain. It could not emit a frame until
  the next one arrived, so every frame waited up to a frame interval inside it.
  `d3d11screencapturesrc` already repeats the last frame at the negotiated
  rate (60.0 fps on a static desktop, measured), which is what it was added for.
- Measure latency honestly. Clock-sync pongs now answer from the pipeline
  clock plus the encoder's measured PTS offset (1000 h - 30 ms on NVENC)
  instead of an anchor refreshed at send time, which had equated capture with
  send and hidden capture, encode, queueing and pacing from every client-side
  age. Capture-to-canvas now reads 16 ms at 1080p60 on the LAN test client.
- Default to 120 fps on first visit when the display refreshes at >= 110 Hz
  and the browser reports hardware-grade (smooth, power-efficient) 1080p120
  decode. Software decoders stay at 60: a laptop decoding 1080p120 in software
  queued 40 ms in the decoder (50 ms capture-to-canvas vs 16 ms at 60 fps).

- Repair frames that left no trace. A small frame is one data fragment plus
  its parity, and QUIC packs both into one UDP packet, so a single lost packet
  erased the frame entirely: no assembly, so no NACK and no FEC, and the only
  repair was the orderer's 300 ms give-up and an IDR. The client now asks for
  a skipped frame number whole (`nack` with `idx = NACK_WHOLE_FRAME`) the
  moment a later frame arrives, and the host re-sends every cached fragment.
  Under 1 % random loss: 42 -> 59 fps median, 13.6 s -> 0 s of freezes a
  minute.
- Let parity rebuild a frame's final fragment. Parity payloads now end with
  the frame's total length (`PARITY_TRAILER_LEN`), so the client can size the
  shorter final fragment; before, it could never be rebuilt, and on a
  one-fragment frame that meant parity repaired nothing at all. Parity that
  arrives before any data also starts the assembly now, instead of waiting
  for a data fragment that may never come.
- Stop killing Firefox's keyframe channels. The host dropped the read half of
  each client-opened video channel, which sends STOP_SENDING; Firefox answers
  that by erroring the whole bidirectional stream, so every IDR died on
  arrival and Firefox decoded nothing (four redials a minute). The read half
  is now kept open and drained.
- Answer a WT redial over the signaling socket. The client's
  `wt_video_info_request` was handled only on the retired WebRTC data channel,
  so any WT close left the session frameless until a reload (host
  `sent=0 ... evicted=121`, client "waiting for a keyframe"). The connection
  close reason is now logged too.
- Run QUIC with a fixed, loss-blind congestion window (`[media]
  wt_congestion = "realtime"`, the default; `"cubic"` restores quinn's
  default). Cubic halved its window on ordinary Wi-Fi erasure and silently
  dropped window-blocked datagrams, a second rate authority that could hold
  the stream far below the configured bitrate. The host logs QUIC's path
  view (cwnd, lost packets, congestion events) every second.
- Measure datagram loss over a four-second window. The per-second sent/seen
  difference swung +/-2 % on sampling skew alone, and the climb gate blocked
  at 1 %, so noise vetoed most climbs. The climb gate is now 2 %, a
  loss-marked ceiling is forgotten after 30 clean seconds, and the first
  telemetry of a session no longer reads as 100 % loss.
- Reset the datagram pace per session, and recover it when there is too
  little traffic to measure. A pace one session had backed off carried into
  the next and pinned the encoder ceiling at 28 Mbps against an 80 Mbps
  setting.
- Reconfigure the decoder after every codec reset. WebCodecs' `reset()` leaves
  the decoder unconfigured; the backlog path and the watchdog reset both left
  it there, so every frame after was dropped until a redial (12 s frozen at
  60 fps arriving).
- Client recovery no longer turns one lost frame into a freeze: frames held
  for a requested IDR are kept, duplicate and stale keyframes are ignored, a
  keyframe partial is never evicted, the watchdog waits for an outstanding
  keyframe, a duplicate `video_config` keeps the decoder, hidden tabs do not
  count as frozen, and keyframe requests are paced to the host's gate.
- Fall back to software decode when the hardware decoder fails, instead of
  giving up on the session.
- Refuse clearly, before claiming the host, in a browser without
  WebTransport; and stop blaming pairing for every connection dropped before
  the first frame.

- Size the NACK re-send cache in frames, not fragments. 1024 fragments is 2.4 s
  of video at 4 Mbps but **0.28 s at 40 Mbps**, and the client cannot ask for a
  re-send until 150 ms after a frame starts assembling plus a round trip — so at
  high bitrate the fragment was always gone before the request arrived. The live
  80 Mbps session logs **11,252 NACKs that hit an empty cache against 1,431
  refused by the rate budget**: damaged frames expired at 900 ms, were abandoned
  (2–5/s, with bursts of 100), and pulled 140 recovery IDRs in eleven minutes,
  each IDR being a large flow-controlled burst on the stream carrier. A 60-frame
  window scales with both the bitrate and the repair latency.
- Do not halve the rate on one lossy second. A single Wi-Fi burst was enough to
  trigger the severe branch through the new measured-loss input: the session cut
  63 times in eleven minutes, almost every one a 50 % halving
  (`31446->15723`, `35117->17558`), each taking ~10 s to climb back. The first
  lossy second now backs off a quarter; only a persisted loss halves.

- Lift the hard ceiling that made `max_kbps=80000` unreachable. The controller
  clamps its target to `pace x WT_PACE_TO_CEILING`, and the worker pace was
  6500 pps - so the encoder could never be asked for more than **47.9 Mbps**
  however the client was configured. An 80 Mbps target needs ~11000 pps
  (11000 x 1118 B = 98 Mbps of datagrams, covering the target plus ~12.5 % FEC
  parity and NVIDIA's overshoot), and that is now the worker pace. In-page
  clients stay at the measured-safe 3800.
- Release a learned rate ceiling at 2 %/s instead of 0.2 %/s, and forget it
  after forty-five consecutive clean seconds. The controller remembers the rate
  that queued the route; at 0.2 %/s that memory was effectively permanent - a
  session cut to 22 Mbps needed ~11 minutes of *continuously* sub-50 ms tail to
  be allowed anywhere near 80 Mbps, and one tick above 50 ms reset the clock.
  The ceiling, not the configured cap, is what was binding.
- Measure what the transport actually destroyed, instead of modelling it. QUIC
  datagrams are unacknowledged, so a datagram the browser's incoming queue drops
  leaves no sender-side trace: the send returns `Ok`, the stream carrier is
  fine, and the client's own loss counter is hardcoded to zero. The host now
  diffs its cumulative datagram send count against the client's reported
  receive count each telemetry interval - the only loss signal that exists on
  this side - logs it, and feeds it to the controller, which uses it to refuse
  climbs into a lossy path (parity and NACKs still deliver frames through
  erasure, so loss holds the rate rather than cutting it).
- Bound the injection pace by evidence rather than assumption. Over-pacing looks
  like loss and nothing else, so the pace now retreats 25 % every two
  consecutive lossy seconds, down to the measured-safe 3800, and creeps back at
  5 % of the ceiling per clean second. This is what makes a raised pace safe to
  ship: a client that cannot drain it lowers it instead of producing a broken
  picture.

- Interleave a frame's fragments across its FEC groups instead of sending them
  front to back. Wi-Fi loses A-MPDU aggregates, not individual datagrams, so a
  run of eight consecutive fragments used to take three or more out of ONE
  8-fragment group — and row-1 parity repairs exactly one hole per group. The
  frame was therefore FEC-dead and could only be finished by a NACK re-send.
  That is the shape of the 80 Mbps session: the client received 18–27 Mbps
  while the host dropped nothing at all (0 stalled, 0 expired, 1 eviction), yet
  it decoded **1–17 fps** with its NACK count climbing by up to 506/s. Sending
  `0, G, 2G, …, 1, G+1, …` means a burst of ≤ G datagrams puts one fragment in
  each group, so parity rebuilds all of them locally with no round trip and no
  re-send traffic. The client reassembles by `frag_idx` and parity is computed
  over group membership, so this is a pure scheduling change on the wire.
- Serve NACK repair requests at 300/s instead of 60/s, and count what the
  budget refuses. The client's demand is bounded by its own round policy but at
  high bitrate a frame is 30–40 fragments: the measured session asked up to
  506/s while the host handed out 60/s, so ~88 % of the repair requests were
  discarded **silently** and the frames never assembled. A refused request and
  an aged-out cache entry are now logged per second — from telemetry alone they
  were indistinguishable from a repair that worked.

- Serve a throttled keyframe request instead of dropping it. The host coalesced
  forced IDRs to one per 2.5 s and `continue`d past a request that arrived
  early — so the request was consumed and lost. Measured over one log: requests
  arriving ≥3.0 s after the previous grant were granted 395/395 times within
  34 ms, but requests arriving any earlier were granted only **4.2 %** of the
  time, with a median 1079 ms wait and a grant-to-grant cadence of 3079 ms. The
  client gives up on a frame after 300 ms and asks again straight away, so its
  requests always landed inside the window and evaporated: a hole the client
  could see in 300 ms stayed black until the gate expired on its own. The
  throttle is now a coalescing bound (one IDR per second, buffered till the
  window opens) rather than a filter, which is the whole recovery budget —
  `ForceKeyUnit` to IDR is 15 ms at the median.
- Request the holes that can actually be repaired. The client's v4 NACK scan
  walked missing indices from 0 and stopped at eight, so a frame with more than
  eight holes **never requested the higher ones in any round** — it could not
  assemble however much the host re-sent, and it was discarded at the 900 ms
  expiry. That is the shape of a 30-fragment IDR whose leading fragments the
  sender's queue destroyed (k..29 arrived, 0..k-1 never did). It now asks for at
  most one hole per 8-fragment FEC group per round, rotating which group leads
  and which hole inside the group gets its turn: a group with two holes is
  FEC-dead until one is restored, so asking for the second in the same round is
  a wasted datagram, while the freed budget covers groups two onwards.
- Stop discarding the one frame the decoder is waiting for. When the client held
  more than eight partial assemblies it evicted the oldest and tombstoned it —
  chosen by frame number, so the victim could be the keyframe that `FrameOrderer`
  was blocked on, and tombstones refuse the host's re-sends. Nothing counted it
  either, which is why the live logs show `aband` flat, `held` climbing and the
  decoder idle with no evidence anywhere. The cap is 24 (900 ms of a 60 fps
  stream is 54 partials, so this covers any burst the expiry scan has not yet
  collected), a delta is evicted before a keyframe, and the eviction is counted.
- Report which carrier each forced IDR took, once a second. A "cached stream"
  delivery cannot be distinguished from success — the write goes into the void
  without error when the client has stopped reading that channel — and 306 of
  337 IDRs went that way, which is the single reason the repair path was
  unreliable.

- Give the forced IDR a carrier it can actually arrive on. The host writes an
  IDR to the video channel it has *cached*, and it cannot tell a channel a
  client is still reading from one whose reader stopped — the write succeeds
  into the void either way — so the datagram copy was doing the work, and that
  copy loses fragments in proportion to its size. Measured over 373 repair IDRs
  in one log: **under 8 KB recovered the client 100 % of the time, 8–30 KB 39 %,
  30–100 KB 15 %**. A 29,639-byte IDR never assembled, three requests were sent,
  the host's one-per-second throttle refused two, and the picture stayed black
  for ~4 s while deltas kept arriving at 60 fps. Asking for a keyframe now opens
  an extra video channel first, so the host's own "freshly installed channel"
  path fires and the IDR rides a stream the client is definitely reading — and
  the datagram copy is kept only for IDRs small enough to survive (≤12 KB).
  (An earlier version of this fix cancelled the working channel to force a
  reopen; that replaced a live path with a dead one, so the spare is opened
  *alongside* it and nothing is cancelled.)
- Stop letting one Wi-Fi spike empty the stream. The tail-delay cut respected
  its cooldown in the 120–250 ms band but exempted anything above it, so a
  single 267–481 ms excursion — exactly what airtime contention throws — cut
  0.6x on every tick: 4000 → 2400 → 1440 → 864 → 600 kbps inside five seconds,
  with the client decoding 59–60 fps throughout (live 04:38). A severe spike
  still cuts harder and holds longer; it can no longer cascade to the floor on
  its own.
- Treat an unmeasured tail as unknown, not perfect. `lat_p95_ms` is negative
  until the client's clock sync settles, and the host clamped it to 0.0 — which
  passed every delay guard and the climb gate, ramping the rate on a route
  nobody had measured.
- Size datagram fragments with a margin. `max_datagram_size` moves with the
  path-MTU estimate, so a fragment sized against the old value can come back
  `TooLarge` mid-frame — and the NACK re-send replays those same oversized bytes
  from the cache and fails identically, leaving a hole the client can never
  repair. One log had 373 such warnings, each a frame delivered with a hole in
  it.

- Reopen the video channel when we ask for a keyframe. A forced IDR is written
  to the channel the host has *cached*, and the host cannot tell a channel a
  client is still reading from one whose reader stopped — a write into the void
  succeeds either way. So when the datagram copy of an IDR lost a fragment, the
  client had no reliable second chance: it asked again, the host's one-per-second
  throttle refused every other request, and the picture stayed black for
  seconds at a time (2026-09-20 04:39: a 29639-byte IDR, three requests, ~4 s of
  nothing while deltas kept arriving at 60 fps, then instant recovery on a
  smaller 13328-byte IDR that survived the datagrams). Asking for a keyframe now
  also reopens the channel, so the IDR that answers it rides a stream the host
  knows is live — its own "freshly installed channel" path, which never fired
  for a whole session.
- Stop cutting the bitrate on stale evidence. `client_lat_p95_ms` is a rolling
  percentile over the client's sample buffer, and that buffer was 512 frames —
  ~8.5 s of video at 60 fps — so a single tail spike held p95 above the
  controller's 120 ms threshold for seconds after the route had cleared. The
  delay branch cut ×0.7 on *every* tick off that number (it never consulted the
  cooldown), and because a gated tick fell through to the climb branch, each cut
  also looked like a fresh episode and re-marked the remembered ceiling lower —
  50000 → 5882 kbps in fourteen cuts. The live log is unambiguous: every cut
  line read `decoded_fps=60 rtt_ms=7 client_p95_ms=135 held=0`, i.e. a client
  decoding 60 fps on a 7 ms RTT while the encoder was driven to the floor. The
  percentile window is now 2 s, a cut holds the rate for its cooldown window
  instead of falling through as a clean tick, and the ceiling only moves on a
  genuinely fresh episode.
- Stop turning every lost fragment into a keyframe. The client had two answers
  to a hole and they were racing: the frame-order gate gave up after 8 frames —
  **133 ms** at 60 fps — while the NACK ladder's first repair round only goes
  out 150 ms after the hole's first fragment, with the re-send landing an RTT
  later. So the gate declared a hole fatal about 17 ms *before* its own repair
  attempt, asked the host for an IDR and reset the sequence, ~once a second and
  forever: `held` cycling 0 → 8 → 0, `keys` climbing one per second, and a
  forced IDR (the largest thing on the wire) whose burst inflated the client's
  tail delay until the bitrate controller cut. The gate now holds a
  cadence-independent repair budget (300 ms of video) so a re-send that lands
  inside it costs nothing, and the decoder's backpressure policy requires the
  queue to stay over the limit instead of tripping on the first sample — a
  repaired hole releases a whole held run at once, which is not a decoder that
  has fallen behind.
- Stop the bitrate sawtooth on a route that queues. The controller cuts on
  rising tail delay, but that was the one cut that left no memory of the rate
  that caused it, so it ramped straight back into the same wall: a 1440p60
  session on a ~6 Mbps uplink ran 600 → 6354 kbps and crashed back to 600 every
  ~17 s for the whole session — ten ramp steps up, seven cuts down, `lost_delta`
  0 throughout, the client's p95 fragment age (25 → 155 ms) the only signal. A
  queue-marked ceiling now holds the rate below the wall, creeps up at 0.2 %/s
  only while the tail is quiet, and — unlike loss — is recorded once per episode
  instead of ratcheting down with each successive cut of the same bad stretch
  (which used to leave the probe cap below the rate the route had already been
  carrying, so it could never be re-tried).
- Fix `decoded_fps` lying to the host on worker-drain clients. The decoder
  counters are pushed from the page once a second while the worker's own
  telemetry timer ran independently, so a tick could difference a stale
  snapshot: 0 fps, then double, on a stream decoding perfectly (live:
  `decoded_fps` 0 / 120 / 0 / 110 at 60 fps, plus spurious `permanent-black
  signature` errors in the host log during healthy sessions, and the host's loss
  and health laws acting on both). In worker mode the page's push is now the
  telemetry tick, so the counters and the window they are divided by always
  describe the same interval.
- Fix the picture-freezing stall: a starved client could never get a keyframe.
  Deltas ride the datagram carrier while forced IDRs ride the reliable stream,
  and a write into a stream the client has stopped reading succeeds silently —
  so a client that lost its anchor asked for a keyframe once a second while the
  host reported perfect delivery: `held=8 decoded=0`, deltas still arriving at
  60 fps and tens of Mbps, no loss, no stream error, no dropped frame, for
  minutes (reported ~30 s into a 1440p60 session over the internet, after the
  bitrate controller ramped into the route's ceiling and one loss burst cost the
  anchor). A keyframe that answers a client request now rides the datagram
  carrier as well as the stream, so the anchor no longer depends on the one
  carrier that can fail this way. A healthy session never pays for it: clients
  only request a keyframe when they have no anchor. The log now also records the
  encoder half of that conversation (`wt: encoder produced a keyframe`, and
  `wt: KEYFRAME DROPPED` when a keyframe has nowhere to go, which was silent).
- Fix pairing from a browser. The same-origin guard compared the `Origin` header
  against a `host` header that HTTP/2 does not send — the authority rides in
  `:authority` — so every browser POST to the API was refused as cross-origin,
  silently and before the PIN was compared. Browsers negotiate HTTP/2 with the
  host, so pairing from Chrome or Safari failed with "pair on the same local
  network using the secure play address", while the same request over HTTP/1.1
  and every native client worked. The request URI's authority is now accepted as
  the request's own target, falling back to the header and to the browser's
  Fetch Metadata verdict.
- Stop a frame with no video channel from panicking the video sender. A forced
  keyframe racing the client's channel install hit `expect("stream just
  ensured")` and killed the sender task for the life of the process: the
  transport kept accepting dials and completing the handshake, so every session
  after it connected and stayed black until the host was restarted. The frame is
  now dropped (the client re-requests a keyframe), the sender's exit is logged
  loudly, `/api/v1/status` reports `wt.sender_alive`, and the transport is
  rebuilt automatically if it ever does exit.
- Rotate the WebTransport certificate on a timer. Browsers refuse a pinned
  certificate that is not valid at dial time and cap its validity at two weeks,
  so a host left running served an expired pin: the page loaded, pairing and
  signaling succeeded, and the picture stayed black until the host restarted.
  `/api/v1/status` now reports the certificate's remaining life.
- Fix IPv4 LAN WebTransport connections by binding both IPv4 and IPv6.
- Redesigned dashboard: play address, PIN, working QR invitations, device access,
  certificate help and live stream health.
- Searchable player library, launcher filters, desktop selection, saved quality
  settings, accessible controls and responsive layouts.
- Clear busy, offline, empty and mutation-failure states; PIN-free diagnostics.
- Loopback Host and browser Origin checks, stricter CSP and no-store API responses.
- Safe Unicode identity parsing, fail-closed DPAPI writes and no plaintext PIN logs.
- Per-user CA storage and original-user setup. Upgrades retire the shared CA;
  other devices must trust the new certificate.
- Support DLLs beside the EXE; AMD, Intel and Media Foundation encoder plugins.
- Strict build/package checks, correct GPL metadata and distribution notices.
- Working web test discovery, browser regression tests and isolated Windows checks.
- Installation, troubleshooting, contribution and release documentation.

These changes do not certify hardware compatibility or a public release.
