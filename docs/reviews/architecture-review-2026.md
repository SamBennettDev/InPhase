# InPhase Architecture Review

The plan is to keep bespoke WebTransport/WebCodecs video, remove intentional presentation buffering, fix IPv6 connectivity, and simplify the code around one session owner. Video does not move back to WebRTC.

The revised plan replaces the earlier WebRTC-video recommendation. Its performance targets still need validation on the Windows host and actual remote clients.

## 1. Define the latency contract and make the defaults match it.

The production video path is GPU capture → hardware encoder → bespoke QUIC datagrams → WebCodecs → immediate canvas drawing.

There is no adaptive presentation delay, no waiting for the next frame, and no video hold to synchronize with audio. Incomplete packets and codec dependencies still need bounded pending state; already-decodable frames should not wait for a presentation timer.

Remove the user-facing `wt_enabled` choice after migrating existing configuration. A transport startup failure must report an error, rather than claim a WebRTC-video fallback that does not exist.

## 2. Fix the confirmed correctness defects before optimizing.

| Defect | Required change |
|---|---|
| Decoder reset leaves it unconfigured | Reset, reconfigure with the last validated configuration, then await a keyframe |
| Keyframe throttle mixes clocks | Use one monotonic clock and one throttle owner |
| Host keyframe event uses the wrong direction | Correct the event/pad pairing and verify an actual IDR |
| Bitmap rendering skips accounting | Share completion accounting and cleanup across rendering paths |
| Parity cannot recover separate groups | Repair each eligible group before assembling the complete frame |
| Control parser discards subsequent messages | Retain unread bytes across reads |
| NACK requests become permanent "loss" | Separate delayed, repaired, expired, and uniquely received packets |
| Raw queue retains older frames | Use one raw buffer with leaky=downstream |

These become regression tests. The five reproduced client failures matter more than adding more tests around isolated policy formulas.

## 3. Replace adaptive presentation with immediate drawing.

Delete `PlayoutPolicy`, its delay settings, successor-frame waiting, and the scheduling loop that intentionally holds decoded frames.

On every decoder output: validate its session/configuration generation, draw it immediately, record completion, and close the VideoFrame.

If a browser needs asynchronous bitmap conversion, permit one conversion in flight and at most one newest pending decoded frame. Close superseded frames and prevent an older conversion from overwriting newer output.

Keep HUD animation independent of video. Browser compositing and display scanout still take time; canvas draw completion is not proof that pixels have appeared.

## 4. Bound every queue without breaking encoded references.

| Stage | Queue policy |
|---|---|
| Raw capture | Keep only the newest frame |
| Encoder | No B-frames or lookahead; measure in-flight delay |
| Encoded output | Bound bytes and age; preserve reference dependencies |
| Packet scheduler | Pace within capacity; expire obsolete work |
| Reassembly | Bound fragments, bytes, frames, and age |
| Decoder | Submit complete frames promptly in decode order |
| Presentation | Draw immediately; no intentional hold |

"Newest wins" is safe before encoding and after decoding. It is not generally safe between interdependent encoded frames.

If frame N+1 completes before reference N, allow only a bounded opportunity to repair N. If recovery is no longer useful, abandon that chain, request an IDR, and resume at the live edge. Do not accumulate seconds of old gameplay.

## 5. Keep the bespoke protocol small and explicit.

Retain existing framing where it works. Add a protocol revision only where semantics require it.

Reliable control should carry authentication, capabilities, configuration changes, acknowledgments, keyframe requests, stop, and compact feedback. Datagrams carry video fragments and eventually audio/input.

Define or verify session generation, codec epoch, frame sequence, packet sequence, capture timestamp, fragment identity, declared lengths, and parity-group identity. Validate limits before allocating memory.

Send the actual encoder configuration and dimensions. Probe the exact configuration with `VideoDecoder.isConfigSupported`; WebRTC codec capabilities are not the right test for this video path. Acknowledge configuration changes before sending the new epoch's keyframe. [WebCodecs specification](https://www.w3.org/TR/webcodecs/).

## 6. Repair frames only while the repair remains useful.

First fix grouped XOR recovery. Keep it as the comparison baseline, then measure parity off and a small number of fixed parity policies. The current ~25% overhead should earn its bandwidth through timely recovered frames.

Replace immediate NACKs and fixed retry timing with evidence of a gap, measured reordering allowance, and a deadline decision. A requested repair needs the client-to-host-to-client round trip to fit within its remaining useful lifetime.

Cap retransmission bytes per time window, not merely fragments per request. Count repair traffic against available bandwidth.

Give one recovery state machine responsibility for keyframe requests. It should request recovery promptly, coalesce duplicates, and avoid flooding a congested route with large keyframes.

## 7. Connect encoder output to transport capacity.

Keep QUIC's congestion control. Add one bounded packet scheduler and one encoder-budget policy, rather than another competing congestion controller.

Feed that policy transport statistics, queue age, queue occupancy, unique delivery feedback, and RTT trends. Distinguish application-queued bytes from delivered bytes: Quinn can discard older queued datagrams even after accepting a send. [Quinn API](https://docs.rs/quinn/latest/quinn/).

Pace bursts over the shortest interval the path can support. Do not spread a frame over its entire frame interval when the network can carry it sooner.

If the current frame cannot fit its deadline, reduce subsequent encoder output or recover at a fresh keyframe. Do not compensate with deeper queues.

Adapt bitrate first, resolution after sustained shortage, and fps when decoding/presentation capacity or minimum usable quality requires it. Remove the assumption that an RTT below 40 ms identifies a LAN.

## 8. Make IPv6 address, URL, certificate, and pinhole agree.

Introduce one small **EndpointPlan** containing:

- Selected interface and usable IPv6 address.
- Canonical HTTPS origin.
- TCP page port and UDP transport port.
- Certificate identities.
- Endpoint generation.

Every subsystem consumes this value. Stop independently selecting addresses for URLs, certificates, and PCP.

Inspect Windows interface state, temporary-address classification, preferred/deprecated state, address lifetime, and route. Monitor network changes.

Initially retain TCP 443 and UDP 4433. Changing UDP to 443 is an optional tested improvement, not a connectivity guarantee.

Use the same canonical origin during LAN enrollment and remote play. A stable IPv6 literal works; changing ISP prefixes require a stable hostname option if bookmarks and browser identity must persist.

## 9. Make router and Windows firewall state reliable.

Fix explicit UDP rules and propagate firewall failures. The application toggle and installed Windows rules must not silently disagree.

Establish manual IPv6 pinholes as the reference setup, then repair PCP automation:

- Retain each mapping's nonce.
- Track TCP and UDP leases independently.
- Renew using each lease's actual expiry.
- Handle router reboot and address changes.
- Remove mappings on disable/shutdown where possible.
- Report errors and actual endpoint state.

Remove NAT-PMP from IPv6 readiness. Consider UPnP IPv6 firewall control only if your target routers justify it.

A successful mapping response is "configured," not "verified remotely." IPv4-only clients and networks blocking the required UDP cannot be promised direct IPv6 connectivity under the no-relay requirement.

## 10. Preserve authentication while fixing certificate handling.

Keep local device enrollment, controller signatures, ACLs, rate limits, and loopback-only administration.

Make certificate SANs match the canonical origin. Rotate the WT certificate and advertised hash coherently before expiry. A hash distributed by an untrusted page does not solve HTTPS trust.

Replace the current origin scheme-prefix check with actual origin validation.

Public IPv6 certificates can be a later onboarding improvement, with automatic renewal and reload. They should not delay the streaming fixes or introduce a mandatory account service.

## 11. Give the whole session one owner.

Every connection receives a generation ID. Every callback, input packet, token, decoder configuration, telemetry sample, and disconnect belongs to that generation.

An old connection's cleanup must be unable to stop its replacement.

One coordinator handles cancellation, input release, queue/cache clearing, transport closure, and encoder teardown. Remove synchronous polling/sleep loops from async signaling.

The essential regression is: A connects, B replaces A, A's delayed close/input arrives, and B remains unaffected.

## 12. Tune encoding for quality within the frame deadline.

Preserve GPU memory from capture through encoder input and verify adapter selection.

Compare NVENC P1/P3/P4 at identical bitrate, content, resolution, and game load. Choose the best quality that meets encode deadlines; P1 is not automatically the best-looking option. Evaluate single-frame VBV with correct property units. [NVENC tuning guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/12.0/nvenc-preset-migration-guide/index.html).

Make GOP duration intentional. Sixty frames means one second at 60 fps and half a second at 120 fps. Verify forced-IDR recovery before experimenting with longer intervals.

Establish H.264 1080p60 first, then HEVC 1440p60/120. Validate actual hardware decoding, codec configuration, color range, resolution changes, and loaded-GPU performance.

## 13. Consolidate audio and input after video is stable.

Keep existing WebRTC audio/input temporarily so their replacement does not block immediate video improvements.

Move input onto WT using the existing binary events, sequence rules, authoritative snapshots, and release watchdog. Test short taps and lost releases. Use bounded reliable delivery for discrete transitions if necessary; keep replaceable pointer/gamepad state on datagrams.

Evaluate Opus datagrams → browser audio decoder → AudioWorklet for audio. Require actual support on the target clients before committing to removal of the old path.

Audio needs a small bounded continuity buffer and output scheduling. That buffer must never hold video back. Measure audio drift and latency independently.

Delete WebRTC audio/input, SDP, ICE, and their exclusive dependencies only after these replacements pass.

## 14. Repair measurements before claiming lower latency.

Keep the original capture clock. Stop anchoring a capture timestamp to "now" at send time, which can hide capture/encode/queue delay.

Record capture, encode completion, send admission, assembly, decode submission/output, and draw completion.

Separate timestamped video/audio/input/transport samples. Missing measurements are unknown, not zero. Remove hardcoded zero freezes and telemetry overwrites.

Measure p50/p95/p99, freeze durations, queue age, useful delivery, and wire overhead. Use an input-to-photon/high-speed-camera test for the headline number; software timestamps explain the stages.

## 15. Delete the complexity that conflicts with this design.

| Delete or replace | Keep |
|---|---|
| Adaptive presentation policy and successor waiting | Immediate WebCodecs/canvas path |
| Nonexistent WebRTC-video fallback branches | Bespoke WT video |
| Fragment-probability resolution model | Measured capacity/headroom policy |
| Duplicate throttles and reset/redial/reload cascades | One recovery state machine |
| Independent session ownership and telemetry writers | One generation and timestamped samples |
| Independent IPv6 address heuristics | One endpoint description |
| Shipping spike crate and obsolete settings/docs | Production protocol and useful regression vectors |
| WebRTC audio/input after replacement passes | Existing capture, Opus encoding, and input reconciliation |

Keep authentication, emergency release, build identity, and useful deployment/test tooling. Game artwork is not a demonstrated streaming bottleneck; defer unrelated feature work rather than claiming its deletion improves packet latency.

## 16. Ship the work in bounded change sets.

| Order | Change set | Exit gate |
|---|---|---|
| 1 | Correct recovery, parity, parsing, loss counting, defaults | Reproduced failures fixed; real IDR; default video |
| 2 | Immediate presentation and raw queue | No intentional hold or growing frame backlog |
| 3 | IPv6 endpoint, enrollment, firewall, PCP | Genuine remote connection and lease renewal |
| 4 | Session ownership and cancellation | No stale-session interference |
| 5 | Accurate timing, codec epochs, receiver ordering | Trustworthy baseline |
| 6 | Packet scheduling, deadline repair, encoder budget | Bounded latency under loss/capacity changes |
| 7 | Encoder quality and 1440p120 tuning | Better equal-budget quality within deadlines |
| 8 | WT audio/input | Required-client behavior and capability gates |
| 9 | Remove superseded components | Clean install, upgrade, reconnect, and soak pass |
| 10 | Optional worker/router/certificate improvements | Each solves a measured problem |

Test real Windows, Mac, and phone clients across LAN, Wi-Fi, and external IPv6. Include 20/50/100 ms RTT, loss, reordering, bursts, capacity drops, path changes, reconnects, router restarts, and GPU-heavy gameplay.

Required outcomes are zero intentional video presentation hold, bounded memory and queue age, no permanent black session after recoverable faults, no stuck inputs, and correct renewal. Compare every optimization against the corrected bespoke baseline using latency tails and freezes — not just the median.
