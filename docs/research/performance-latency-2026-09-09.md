# InPhase Performance and Latency Research

Repository audit and target architecture for stable, low latency remote play over cellular and wide area networks

Repository baseline bd71b5effe160948a7b82c8a8fce5891febd9091 | 9 September 2026

## Executive conclusion

InPhase has the right capture and decode fundamentals, but its current video carrier works against its latency goal on lossy remote links. The Windows path keeps GPU surfaces through conversion, disables encoder reordering and lookahead, bounds the raw capture queue to one frame, and presents decoded frames immediately. Those choices prevent most host and browser buffering. The remaining dominant risk is transport delivery: all video frames are written sequentially to one reliable QUIC stream. A lost packet delays every later byte on that stream until retransmission. The 120 ms write timeout then resets the stream and the client requests a large keyframe, producing a freeze-and-repair cycle precisely when cellular capacity is weakest. [1] [2]

The recommended design is deadline-aware QUIC DATAGRAM video with transport pacing, selective protection, and a freshness budget enforced at every queue. Audio remains datagram-based, input stays isolated, and a reliable control stream carries configuration and feedback. If Safari-specific defects still make datagrams unusable in one direction, use a two-carrier matrix rather than forcing every browser onto the single-stream compromise. The host should also replace its once-per-second decoder-FPS AIMD loop with a sender-side estimator that reacts to delivery rate, RTT inflation, congestion window pressure, and explicit frame outcomes. Device decode overload must be handled separately by changing codec, frame rate, or resolution.

|Priority|Change                                                                  |Expected effect                                                                     |Risk  |
|--------|------------------------------------------------------------------------|------------------------------------------------------------------------------------|------|
|P0      |Instrument a complete capture to present timeline and replay network traces|Makes every later change measurable; separates network, decode, and render delay |Low   |
|P0      |Replace the persistent reliable video stream with deadline-aware datagrams|Removes stream head-of-line blocking and prevents stale retransmissions           |Medium|
|P0      |Use a 20 to 35 ms video freshness budget and queue of one complete frame|Caps transport and decode backlog instead of recovering after 120 ms               |Low   |
|P1      |Adopt sender-side bandwidth and delay control at 100 to 200 ms cadence  |Faster response to cellular capacity drops; fewer bitrate oscillations             |Medium|
|P1      |Add unequal FEC and bounded retransmission only inside the frame deadline|Reduces visible loss without converting the path back to reliable delivery       |Medium|
|P1      |Split network adaptation from decoder adaptation                        |Avoids cutting bitrate for a slow device or raising it while decode is stalled     |Low   |
|P2      |Evaluate Quinn BBR against CUBIC under controlled traces                |May reduce loss-driven collapse and queue growth; must be benchmarked for fairness |Medium|

## Recommended performance targets

|Metric                          |Good cellular target                       |Failure threshold       |
|--------------------------------|-------------------------------------------|------------------------|
|Capture to present p50          |less than 80 ms                            |greater than 120 ms     |
|Capture to present p95          |less than 130 ms                           |greater than 200 ms     |
|Latency variation p95 minus p50 |less than 45 ms                            |greater than 90 ms      |
|Video freeze ratio              |less than 0.5 percent                      |greater than 2 percent  |
|Single freeze duration p95      |less than 250 ms                           |greater than 600 ms     |
|Input host arrival p95          |less than network one way delay plus 12 ms |greater than plus 30 ms |
|Recovery after handoff or outage|less than 500 ms                           |greater than 1500 ms    |
|Host raw queue                  |zero or one frame                          |more than one frame     |

## Repository findings

### Current media path

The current host path is capture, one-frame leaky queue, D3D11 conversion, hardware encode, an encoder pad tap, then WebTransport. Video is copied from the mapped encoder output into a Vec before transport. Audio uses 10 ms Opus packets sent as QUIC datagrams. The browser reads self-delimiting encoded frames, submits them to WebCodecs, draws them to a canvas, and closes every VideoFrame. The decoder uses optimizeForLatency and a nonstandard latencyMode hint; the WebCodecs standard describes optimizeForLatency as a request to minimize how many chunks must be decoded before output. [4]

The GPU-resident capture and conversion path, zero B-frames, disabled lookahead, hardware-only policy, one raw frame queue, decodeQueueSize guard, and immediate canvas presentation are all sound. They should remain. The main performance work belongs in the encoded-frame handoff, transport, adaptation, and measurement layers.

### The video protocol and implementation disagree

ADR 0011 still describes fragmented QUIC datagrams, XOR parity, bounded NACK, a custom playout scheduler, and WebRTC audio. The current protocol comments describe one frame per independent stream. The live sender instead places all frames back-to-back on one client-opened bidirectional stream, while audio is now datagram-only. The browser retains dead code and comments for server-initiated stream handling and frame reordering. These contradictions make tests and design reviews validate a protocol that is not running.

|Source                |Documented behavior                                    |Current behavior                                       |
|----------------------|-------------------------------------------------------|-------------------------------------------------------|
|ADR 0011              |Video datagrams with parity and bounded retransmission |Reliable persistent stream                             |
|wtvideo.rs            |One frame per unidirectional stream                    |Multiple frames on one bidirectional stream            |
|wt.ts comments        |Concurrent incoming unidirectional streams             |Primary path opens and reads one bidirectional channel |
|Architecture overview |WebRTC video and audio are authoritative               |WT video and audio are active on remote path           |
|Client telemetry      |Packet and fragment loss drive AIMD                    |Loss is always reported as zero for reliable video     |

### Persistent stream head of line blocking

QUIC removes loss coupling between different streams, but bytes within one stream remain ordered. RFC 9000 requires receivers to buffer out-of-order stream data and retransmit stream data until acknowledged or reset. [1] InPhase currently sends frame N plus every later frame on one ordered stream. Loss in frame N blocks N plus 1 even if the later frame is already available at the receiver. This is transport head-of-line blocking inside the application's only video stream.

The 120 ms write timeout does not enforce a 120 ms playout deadline. write_all can complete when bytes enter the local QUIC buffer, long before delivery. Conversely, a timeout resets the stream after part of a frame may already be in flight, discards all later framed data on that stream, waits 250 ms before the browser opens another channel, spends roughly one RTT delivering the marker, and then requests an IDR. The repair floor can therefore exceed 250 ms plus connection scheduling, RTT, keyframe encode, transfer, and decode.

### The queue is bounded in frames but not in bytes or age

The pipeline to sender channel holds four encoded frames. At 60 fps this is nominally 67 ms; at 120 fps it is 33 ms. Keyframes can be orders of magnitude larger than delta frames, and the QUIC implementation also buffers bytes below this channel. The 256 KiB datagram buffer does not limit stream buffering. A frame-count limit alone cannot establish a latency limit. Each frame needs an absolute expiry timestamp, and every queue must reject work past that timestamp.

### Congestion and adaptation findings

Two congestion controllers act on one flow

Quinn uses CUBIC by default and paces packets according to its congestion window. InPhase adds a second application controller that changes encoder bitrate once per second. QUIC congestion control protects the network; encoder control should keep media generation slightly below the capacity that QUIC can deliver before deadlines. Without direct delivery-rate and queue-delay measurements, the application loop observes the result late and can oscillate around the real capacity.

RFC 9002 recommends pacing all in-flight packets because bursts create short-term congestion and loss. [3] The current implementation correctly benefits from Quinn pacing, but the encoder can still emit a large IDR burst. An effectively infinite GOP avoids periodic bursts, yet every reference break forces an IDR. This couples recovery cost to congestion severity.

Decoded frame rate is not a congestion signal

The bitrate controller cuts only when decoded FPS collapses together with inferred loss, or when a frame write stalls. On the current reliable stream, client packet loss is always zero. Decoded FPS can fall because the browser lacks hardware decode, the main thread is busy, canvas conversion is slow, the device is thermal throttled, or a reference chain broke. Those cases require codec, frame-rate, resolution, or rendering changes. Lowering bitrate may reduce decode work indirectly, but it does not identify or solve the cause.

The current sender stall statistic records the worst write_all duration in a one-second window. It is a useful alarm but a weak estimator: it has no baseline, no distribution, and no direct relationship to bytes buffered in QUIC. A single 101 ms write triggers a 15 percent cut while a growing 90 ms tail does not. The metric should be replaced or supplemented with smoothed RTT, minimum RTT, RTT variance, congestion window, bytes in flight, lost bytes, delivered bytes, pacing rate, and application queue age.

Startup rate is too aggressive for unknown remote paths

The preset table starts HEVC 1440p60 at 22 Mbps and 1440p120 at 35 Mbps; H.264 starts still higher. Those are plausible quality ceilings on strong networks, not safe probes for an unmeasured cellular route. A first-second overshoot fills carrier, home-router, modem, and QUIC buffers before the one-second feedback loop can act. Remote mode should begin from a conservative cached or probed rate, then ramp using delivery evidence. A practical uncached starting range is 4 to 8 Mbps for 1080p60 and 8 to 12 Mbps for 1440p60, with resolution or FPS negotiation if that cannot sustain the requested mode.

Audio and video compete without an explicit deadline policy

Audio datagrams and reliable video share one QUIC connection and one congestion controller. RFC 9221 confirms that QUIC datagrams are congestion controlled and may be delayed or dropped when the controller does not allow sending. [2] The Rust select loop usually services ready work fairly, but it does not establish a byte or time reservation for audio. A keyframe burst can occupy the congestion window while 10 ms audio packets wait or drop. Audio, input acknowledgments, and control repair messages need explicit priority ahead of delta video.

## Target transport design

### Media plane

Use one WebTransport session with four logical classes: reliable control, unreliable input snapshots where supported, unreliable audio, and deadline-aware video datagrams. RFC 9221 explicitly identifies audio, video, and gaming as uses for QUIC datagrams and states that datagrams share QUIC congestion control without retransmission. [2] A video datagram that misses its opportunity can be discarded while a newer datagram proceeds. This property matches remote play better than reliable ordered delivery.

|Class           |Delivery                              |Deadline        |Priority                        |Recovery                            |
|----------------|--------------------------------------|----------------|--------------------------------|------------------------------------|
|Control         |Reliable stream                       |Session lifetime|Highest for keyframe and config |Retransmit                          |
|Input           |Datagram plus isolated fallback stream|20 to 40 ms     |Highest                         |Newest state replaces old state     |
|Audio           |Datagram                              |40 to 80 ms     |High                            |Optional small parity; conceal loss |
|Video key data  |Datagram fragments                    |35 to 80 ms     |High                            |Unequal FEC and one bounded resend  |
|Video delta data|Datagram fragments                    |20 to 50 ms     |Normal                          |FEC only; drop after deadline       |

### Frame fragmentation and protection

- Read the negotiated maximum datagram size and cap application payload below it. RFC 9221 notes that QUIC datagrams cannot be fragmented and the effective limit may be lower than the advertised QUIC value because of path MTU. [2]
- Give every frame a capture time, frame number, dependency epoch, fragment index, fragment count, and deadline class. Reject fragments from older epochs immediately.
- Group fragments into small FEC blocks rather than one XOR fragment for an entire frame. Start with 8 data plus 2 parity for keyframes and 10 plus 1 for delta frames, then tune from measured burst-loss traces.
- Allow at most one retransmission only when predicted arrival remains before the deadline. Never retransmit a stale delta frame. A missing reference should trigger a recovery frame instead.
- Reserve pacing budget for control and audio. Avoid sending an entire keyframe as one burst; interleave its fragments with audio and the newest delta data when dependency rules permit.

### Reference structure and recovery

The current effectively infinite GOP makes any unrecovered reference loss fatal until a full IDR. Keep B-frames and lookahead disabled, but add regular low-cost recovery points. Test intra-refresh if every target decoder handles it correctly; otherwise use short closed recovery epochs or periodic IDRs whose interval adapts to loss. A starting experiment is an IDR every 1 to 2 seconds on remote links, combined with a capped keyframe size and pacing. This raises steady bandwidth but bounds worst-case recovery. The correct interval must come from trace replay, not a universal constant.

Client repair should be one state machine driven by frame age and dependency state. If a delta frame is incomplete after its deadline, discard its epoch until the next recovery point. If a keyframe cannot complete, request one replacement after a cooldown based on RTT, not a fixed 500 or 1000 ms. Do not reset the whole QUIC connection for a video dependency failure.

### Safari compatibility matrix

The repository records inconsistent iOS WebKit behavior for client-to-host datagrams and server-initiated streams. Treat those as capability results, not reasons to force all clients onto one reliable stream. Probe each session: host-to-client datagrams, client-to-host datagrams, and server-initiated streams. Select the narrowest compatible carrier. If host-to-client datagrams work, use them for video even when input requires its own client-opened stream. If they fail, use rotating client-opened video streams with a small number of lanes and explicit cancellation, then measure the residual head-of-line delay. WebRTC remains the last fallback.

### Target congestion controller

#### Control objectives

The media controller should minimize capture-to-present age subject to a minimum delivered frame rate and acceptable visual quality. It should not maximize raw throughput. Maintain a small safety margin below estimated deliverable capacity, react quickly to rising queue delay, and recover cautiously after a capacity drop. Cellular schedulers can change capacity within hundreds of milliseconds, so a one-second control interval is too coarse for the fast path.

#### Signals and cadence

|Signal                                |Source                    |Cadence                  |Use                                              |
|--------------------------------------|--------------------------|-------------------------|-------------------------------------------------|
|Smoothed and minimum RTT              |Quinn connection stats    |50 to 100 ms             |Estimate queue delay as RTT minus minimum RTT    |
|Bytes in flight and congestion window |Quinn                     |50 to 100 ms             |Detect transport pressure before a write blocks  |
|Delivered and lost bytes              |Quinn ACK processing      |100 to 200 ms            |Estimate available delivery rate and loss regime |
|Oldest media age                      |Application queues        |Every frame              |Hard freshness cutoff                            |
|Complete and late frames              |Browser feedback          |100 to 200 ms batch      |Validate deadline success                        |
|Decode time and queue depth           |WebCodecs                 |Every output and dequeue |Device adaptation only                           |
|Present age                           |Clock synchronized client |100 to 200 ms batch      |End objective and regression gate                |

#### Recommended control law

1. Set target media rate to 80 to 90 percent of a robust short-window delivered-rate estimate. Use a lower margin when RTT variance is high.
2. Cut immediately when queue delay exceeds 15 to 25 ms above minimum RTT, bytes in flight approaches the congestion window while application age rises, or late-frame ratio exceeds the target.
3. After a cut, hold for at least one smoothed RTT and require two clean observation windows before increasing.
4. Increase by a small proportional step, about 3 to 8 percent, rather than doubling once per second. Use faster startup only while queue delay is flat.
5. Reset the path model on migration, NAT rebinding, or an RTT step that persists for several samples. QUIC supports client connection migration, but capacity and RTT estimates from the old path do not apply to the new one. [1]
6. Cache the last stable rate by host, client class, codec, resolution, network type, and coarse route identity. Start the next session below the cached value and invalidate it after a handoff.

### CUBIC and BBR experiment

The vendored Quinn version contains CUBIC and BBR controllers, with CUBIC selected by default. BBR estimates bottleneck bandwidth and propagation delay rather than treating loss alone as congestion; its original paper reports benefits when random loss and bufferbloat make loss-based control misleading. [6] This makes BBR a credible cellular experiment, not an automatic production choice. Benchmark CUBIC and Quinn BBR with the same application deadline controller, competing flows, random loss, policers, and rapidly varying capacity. Keep CUBIC unless BBR improves p95 latency and freeze time without unacceptable fairness or burst behavior.

### L4S

L4S can maintain shallow queues and low delay variation when both endpoint congestion control and the network support scalable ECN. RFC 9330 describes roughly 1 ms marking thresholds and rapid rate tracking under stable conditions. [5] Treat L4S as an optional later path. It requires compatible congestion control and network treatment and cannot replace deadline enforcement or fallback behavior on ordinary networks.

## Capture encode decode and presentation

### Host pipeline

Keep D3D11 capture and conversion, the one-buffer leaky raw queue, hardware encode, zero B-frames, zero lookahead, and the ultra-low-latency preset. Measure the encoder pad-probe copy: mapping the encoded buffer and allocating a new Vec for every access unit may add CPU work and allocator variance at 120 fps. Replace it with pooled buffers or reference-counted mapped memory only if profiling shows a meaningful tail. Do not disturb the GPU capture path without evidence.

Give the encoded handoff a latest-value semantic. A bounded mpsc channel that rejects the newest item when full preserves older queued frames. For live video, when the queue is full, remove the oldest non-key frame and insert the newest eligible frame. Preserve a keyframe only while it remains inside its deadline. Track both queue length and oldest age.

### Encoder policy

- Expose capped frame size or VBV settings where each vendor supports them. Rate control that meets an average bitrate but emits a multi-megabyte IDR can still destroy latency.
- Keep HEVC opportunistic. Run a real decode benchmark after isConfigSupported because the repository already observed browsers that advertise HEVC and then fail in use.
- Add an explicit 1080p60 cellular-safe mode. Lowering bitrate alone at 1440p120 can produce poor quality and maintain excessive capture, encode, decode, and render work.
- When the decoder is behind, first lower FPS, then resolution, then switch codec if hardware acceleration is unavailable. Network bitrate remains controlled by network signals.

### Browser decode and canvas

The decodeQueueSize guard is valuable but its threshold of six frames permits 100 ms of queued work at 60 fps and 50 ms at 120 fps before reset. Use an age-based limit and target zero or one submitted frame beyond the decoder's active work. The WebCodecs dequeue event can drive admission instead of polling. [4] If a fresh keyframe arrives while old chunks remain queued, reset and configure once, then feed only the new epoch.

Immediate Canvas2D drawing avoids a deliberate playout buffer, but presentation still occurs through browser compositing and display scanout. Measure decoded output time, draw completion proxy, requestAnimationFrame timing, and where supported requestVideoFrameCallback on the fallback video element. Keep only the newest decoded VideoFrame if bitmap conversion is already in flight. Test OffscreenCanvas in a dedicated worker on Chrome and Edge; retain the main-thread path on Safari unless measurements show a stable benefit.

### Audio synchronization

The audio path starts 60 ms ahead and only drops queued audio after it exceeds 250 ms. That can make audio lead fresh video or retain latency after a network recovery. Use a bounded target that follows measured jitter, for example 30 to 80 ms, and hard-reset the schedule when buffered lead exceeds the target plus 30 ms. Report audio playout lead and underruns. For gaming, do not hold video to match delayed audio; let audio conceal or resynchronize.

## Measurement architecture

### One frame timeline

Every frame should carry one trace identifier and timestamps for capture acquired, conversion submitted, encoder input, encoder output, application enqueue, QUIC handoff, client complete, decode submitted, decode output, draw called, and presentation estimate. Use monotonic clocks and the existing ping based clock mapping. Report raw samples in a ring buffer and aggregate p50, p95, and p99. An exponential moving average alone hides the spikes that define remote-play quality.

|Segment    |Metric                                        |Interpretation                                   |
|-----------|----------------------------------------------|-------------------------------------------------|
|Capture    |inter-capture interval and source age         |Capture API cadence and duplicate-frame behavior |
|Encode     |encoder output minus input                    |GPU encode tail and IDR cost                     |
|Host queue |handoff minus encoder output                  |Application backlog                              |
|Network    |client complete minus QUIC handoff            |Transit, loss recovery, and congestion delay     |
|Decode     |output minus submit                           |Hardware availability and device pressure        |
|Render     |presentation estimate minus decode output     |Canvas, compositor, and scanout                  |
|End to end |presentation minus capture                    |Product latency objective                        |

### Network trace suite

Build repeatable profiles with Linux netem or a hardware network emulator. Each run should last at least ten minutes and include a controller-input script so motion and bitrate demand are comparable. Record packet capture, host telemetry, browser trace, encoder stats, frame timeline, and device temperature.

|Profile        |RTT         |Jitter  |Loss             |Capacity behavior                      |
|---------------|------------|--------|-----------------|---------------------------------------|
|Strong 5G      |35 ms       |5 ms    |0.2 percent      |40 Mbps stable                         |
|Typical LTE    |70 ms       |15 ms   |1 percent burst  |8 to 20 Mbps every 5 seconds           |
|Weak LTE       |110 ms      |35 ms   |3 percent burst  |2 to 8 Mbps every 2 seconds            |
|Bufferbloat    |45 ms base  |variable|0.2 percent      |uplink cross traffic adds 200 ms queue |
|Handoff        |50 to 120 ms|40 ms   |500 ms outage    |path and address change                |
|Tunnel MTU     |60 ms       |10 ms   |0.5 percent      |1280 byte path MTU                     |
|Random erasure |60 ms       |10 ms   |10 percent random|20 Mbps stable                         |

### Acceptance tests

- No queue at any layer may contain media older than its configured freshness budget.
- A lost delta packet must not delay a newer complete recovery frame.
- A 500 ms outage must not require a new WebTransport connection unless path validation fails.
- A decoder overload test must not lower network bitrate unless sender-side congestion evidence also exists.
- An abrupt capacity reduction from 20 Mbps to 5 Mbps must bring queue delay below 30 ms within 500 ms and must not freeze longer than 600 ms.
- Audio packets and input repair messages must continue during a paced video keyframe burst.
- All claimed modes must be tested on real Chrome, Edge, and current iOS Safari hardware; API support checks alone do not pass.

## Implementation plan

|Phase                   |Repository changes                                                                               |Exit gate                                                       |
|------------------------|-------------------------------------------------------------------------------------------------|----------------------------------------------------------------|
|Phase 0 Measurement     |Extend frametrace, Quinn stats export, client frame outcomes, JSONL trace capture, netem harness |Baseline report for all seven network profiles with p50 p95 p99 |
|Phase 1 Freshness       |Age-based queues, latest-frame replacement, decode admission of one, audio lead cap              |No application queue exceeds its budget                         |
|Phase 2 Datagram video  |New v4 fragment format, dynamic MTU budget, deadline drop, receiver reassembly, capability matrix|Loss of one packet never blocks newer frames                    |
|Phase 3 Recovery        |Small-block FEC, one deadline-bounded repair, dependency epochs, paced recovery frames           |Weak LTE freeze ratio below 2 percent                           |
|Phase 4 Rate control    |100 to 200 ms delivery and delay estimator, conservative startup, cached stable rate             |20 to 5 Mbps step settles within 500 ms                         |
|Phase 5 Device adaptation|Decode classification, FPS and resolution ladder, verified hardware codec choice                |Thermal and software-decode tests stay fresh                    |
|Phase 6 CCA trials      |CUBIC versus BBR trace matrix; optional ECN and L4S spike                                        |Ship only if p95 and fairness gates pass                        |

## Concrete file map

|File                                         |Change                                                                                                                       |
|---------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------|
|crates/host/src/media/wt/transport.rs        |Replace persistent frame stream sender; add deadline scheduler, priority queues, datagram fragment sender, and Quinn metrics.|
|crates/host/src/media/wt/session.rs          |Batch frame acknowledgments and outcomes; retain reliable control; expose migration and liveness events.                     |
|crates/host/src/media/bitrate.rs             |Replace decoder-FPS AIMD with delivery-rate and RTT-inflation controller; split device policy.                               |
|crates/host/src/media/pipeline.rs            |Use latest-frame replacement at encoder handoff; record encoder timestamps; apply rate and recovery-frame limits.            |
|crates/protocol/src/wtvideo.rs               |Create v4 datagram framing, dependency epochs, deadlines, FEC groups, and golden vectors. Remove stale v3 claims.            |
|web/src/wt.ts                                |Reassembly with expiration, batched outcomes, capability probes, datagram age limits, and carrier matrix.                    |
|web/src/wtdecoder.ts                         |Age-based admission, dequeue-driven feed, single recovery state machine, and separate device telemetry.                      |
|web/src/frameorder.ts                        |Delete from datagram path or reduce to dependency-epoch validation; datagrams should not wait for missing sequence numbers.  |
|web/src/wtaudio.ts                           |Adaptive bounded jitter target, hard lead reset, underrun and lead telemetry.                                                |
|docs/adr/0011-webtransport-webcodecs-video.md|Replace historical progress text with the actual v4 decision and explicit browser fallback matrix.                           |

## Changes to avoid

- Do not increase the QUIC or application send buffers to hide stalls. Larger buffers convert capacity variation into latency.
- Do not use reliable retransmission for every video byte. Reliability without deadlines recreates the current head-of-line problem.
- Do not request a keyframe for every dropped frame. Recovery traffic is largest when the route has the least spare capacity.
- Do not make bitrate the only adaptation dimension. A weak decoder and an undersized network are different failures.
- Do not claim latency from an EMA alone. Publish percentiles, variation, freeze time, and the full segment breakdown.

## Decision summary

InPhase should keep its bespoke browser media path. WebTransport plus WebCodecs is still the best fit for the stated requirement that frames be displayed as soon as useful rather than held by a browser-managed WebRTC jitter buffer. The present persistent reliable stream, however, gives up the most important property of that architecture. Restoring unreliable, congestion-controlled datagrams with explicit deadlines is the highest-impact change.

The next engineering milestone should not be a broad refactor. Build the measurement harness, then implement a v4 datagram carrier behind the existing fallback switch. Prove it against recorded cellular traces before deleting the stream path. Once freshness is structurally bounded, replace the rate controller and tune protection. This sequence makes each result attributable and gives the project a working fallback throughout the transition.

## Sources

[1] IETF. RFC 9000 QUIC A UDP Based Multiplexed and Secure Transport. May 2021. https://www.rfc-editor.org/rfc/rfc9000.html

[2] IETF. RFC 9221 An Unreliable Datagram Extension to QUIC. March 2022. https://www.rfc-editor.org/rfc/rfc9221.html

[3] IETF. RFC 9002 QUIC Loss Detection and Congestion Control. May 2021. https://www.rfc-editor.org/rfc/rfc9002.html

[4] W3C. WebCodecs. current working specification accessed September 2026. https://www.w3.org/TR/webcodecs/

[5] IETF. RFC 9330 Low Latency Low Loss and Scalable Throughput Architecture. January 2023. https://www.rfc-editor.org/rfc/rfc9330.html

[6] Cardwell et al. BBR Congestion Based Congestion Control. ACM Queue 2016. https://research.google.com/pubs/pub45646.html

[7] W3C. WebTransport. current working specification accessed September 2026. https://www.w3.org/TR/webtransport/

[8] SamBennettDev InPhase. Repository snapshot bd71b5e. 9 September 2026. https://github.com/SamBennettDev/InPhase/commit/bd71b5effe160948a7b82c8a8fce5891febd9091

[9] SamBennettDev InPhase. WebTransport transport implementation. 9 September 2026. https://github.com/SamBennettDev/InPhase/blob/bd71b5effe160948a7b82c8a8fce5891febd9091/crates/host/src/media/wt/transport.rs

[10] SamBennettDev InPhase. Browser WebTransport client. 9 September 2026. https://github.com/SamBennettDev/InPhase/blob/bd71b5effe160948a7b82c8a8fce5891febd9091/web/src/wt.ts

[11] SamBennettDev InPhase. Bitrate controller. 9 September 2026. https://github.com/SamBennettDev/InPhase/blob/bd71b5effe160948a7b82c8a8fce5891febd9091/crates/host/src/media/bitrate.rs

[12] SamBennettDev InPhase. WebCodecs decoder. 9 September 2026. https://github.com/SamBennettDev/InPhase/blob/bd71b5effe160948a7b82c8a8fce5891febd9091/web/src/wtdecoder.ts
