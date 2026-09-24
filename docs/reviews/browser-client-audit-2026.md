# InPhase browser client — audit (read-only)

Scope: `web/src/**` (client) plus the host send paths that bound it. No file was modified
except this report. Line references are `path:line` as read at audit time.

---

## 0. Data flow, end to end

```
HOST   encoder -> FrameQueue -> v4 fragmenter (datagrams) + video channel (stream)
       crates/host/src/media/wt/transport.rs
         :427-475   delta => datagram fragments only; IDR => stream, and datagrams
                    only when a keyframe request was honoured (key_by_datagram)
         :533-537   datagram queue full => drop the REST of the frame, ok=false
         :615-630   keyframe also re-sent on the datagram carrier
         :49-51     KEY_STREAM_TIMEOUT 500 ms: an IDR write that cannot land resets
                    the stream
         session.rs:427-450  request_keyframe(): at most once per 1000 ms
WORKER wtworker.ts      owns WebTransport, datagram loop, v4 reassembly, FEC, NACK,
                        control I/O, telemetry; posts whole frames to the page
         :58        postMessage({t:"frame",f}, [f.payload.buffer])   (transfer)
         :86-91     page stats push => latestStats + core.telemetryNow()
PAGE   wt.ts:151-205    worker message demux; clock offset folded onto the page clock
       play.ts:995      onFrame -> WtDecoder.onFrame
       wtdecoder.ts     FrameOrderer -> submit -> VideoDecoder -> onDecodedFrame
                        -> presentNow (canvas draw) -> finishPresent
```

Both carriers converge on the same page-side gate:

* **datagram path** — `wtcore.ts:433-461` (`readDatagrams`) → `wtcore.ts:497-603`
  (`onVideoFragment`) → FEC `wtcore.ts:650-701` → `wtcore.ts:607-641` (`maybeDeliver`)
  → `h.onFrame` → `wtworker.ts:58`
* **stream path** — `wtcore.ts:383-429` (`videoChannelLoop`) → `wtcore.ts:725-781`
  (`readOneFrame`/`readExact`) → `h.onFrame` → `wtworker.ts:58`

---

## 1. Latency budget for one frame (nominal, steady state)

| # | Stage | Code that governs it | Bound enforced |
|---|---|---|---|
| 0 | Host fragment pacing | `transport.rs:486-539` | paced at ≤0.8× the client's measured drain (`pace_pps`); queue full ⇒ rest of frame dropped |
| 1 | Browser incoming-datagram queue | RFC 9221, no flow control (`wtcore.ts:1-16`) | **none** — drops from the head; the worker's own reader is the only mitigation (`wtworker.ts:1-13`) |
| 2 | Worker datagram read + demux | `wtcore.ts:433-461`; parse `wtcore.ts:497-506` | none per fragment; ~9 k datagrams/s required for 80 Mbps (comment `wtworker.ts:8-10`) |
| 3 | v4 reassembly | `wtcore.ts:466-603` | `v4` ≤8 concurrent frames (`:589-596`); 900 ms expiry (`:537-544`); `v4Done` tombstone ≤512 (`:541-543,617-619`) |
| 4 | FEC repair | `wtcore.ts:650-701` | 1 loss per 8-fragment group (row 1); 2 if even/odd-split (rows 1+2); never the final fragment; never the last fragment of a group (`:671,684`) |
| 5 | NACK repair | `wtcore.ts:545-562` | first round 150 ms after the first fragment, rounds every 180 ms, ≤4 rounds, ≤8 holes/round; useless after the 900 ms expiry |
| 6 | Worker → page hop | `wtworker.ts:58` | **unbounded** message queue; ordering is FIFO and preserved (single port, structured clone of the frame object) |
| 7 | Frame-order hold | `frameorder.ts:154-211` | reorder horizon = `max(MAX_REORDER=8, ceil(300 ms / cadence))` ⇒ ~18 frames at 60 fps (`:80-83`); fatal hole at 300 ms held (`:48,88-98`); memory backstop 64 (`:51`) |
| 8 | Decoder submission | `wtdecoder.ts:297-345` | backpressure drop only if `decodeQueueSize > 6` (`decoderpolicy.ts:108`) for >24 consecutive submissions (`:126`) |
| 9 | Decoder queue | WebCodecs; measured by `wtdecoder.ts:375-380` | ≤~6 chunks ≈ ≤100 ms before stage 8 acts |
| 10 | Decode | `wtdecoder.ts:374-419` | unbounded inside the codec; `decodeMs` is an EMA (`:380`) |
| 11 | Present | `wtdecoder.ts:424-496` | none — drawn inside the output callback, **no rAF wait**; bitmap path allows exactly one conversion in flight (`:442-463`, `PresentGate` `:65-88`) |
| 12 | Compositor → glass | not observable from the client | `e2eMs` = capture→present EMA (`:488-492`) |

**Where the waits are unbounded or poorly bounded**

* Stage 1 — invisible to the client; the only protection is pacing (`wtcore.ts:958-965`
  reports `drain_pps`; `worker:true` raises the host's pace).
* Stage 6 — no acknowledgement, no depth counter, no age stamp travels with the frame.
  The page cannot tell "the worker held this frame 200 ms" from "the network was slow".
* Stage 7 — the **dominant long pole**: 0-300 ms of deliberate wait whose exit is not a
  bound but "whenever the next keyframe is honoured and delivered" (§2.1).
* Stage 9/10 — bounded in count, not in time; the codec's own latency is never measured
  against a deadline.
* `readExact` (`wtcore.ts:801-863`) — with `datagramVideoEnabled` true the 2 s wedge
  watchdog is disabled (`:831-837`), so a hostile/quiet host wedges the stream reader
  indefinitely. That is the recovery carrier for IDRs, so "forever" is the worst shape.

Steady state measured (58-61 fps, p95 fragment age 27-36 ms) means stages 1-6 + 9-12 cost
tens of ms; **the whole smoothness problem lives in stage 7 and in what it waits for.**

---

## 2. Stall / jitter mechanisms (code-cited)

### 2.1 The reorder buffer gives up on exactly the frames it was holding for the IDR — code-level diagnosis, this is the measured stall

`frameorder.ts:169-174` buffers deltas while `nextSeq === null` (no anchor yet).
`frameorder.ts:177-182,209` then calls `budgetSpent`, which on a **pure timer** does
`pending.clear(); nextSeq = null; resyncNeeded = true` (`frameorder.ts:88-98`). The spend
condition is `heldSpanMs(newestUs) >= REORDER_BUDGET_MS` = 300 ms (`frameorder.ts:48,91`)
— i.e. ~18 frames at 60 fps. Consequences:

* Every ~300 ms during an IDR wait the client **destroys the run it was holding** and
  restarts the anchor search. `held` therefore oscillates 0→18 in that state; a snapshot
  showing `held 16` is exactly this sawtooth (`frameorder.ts:30-47` documents an earlier
  version of the same sawtooth, `held` 0→8→0 with `keys` climbing).
* Nothing in the client can distinguish "the IDR is in flight and will land in 20 ms"
  from "the IDR is lost": the spend is purely `heldSpanMs`.
* The re-request that the spend triggers is throttled in three uncoordinated places:
  page `KeyframeThrottle(500)` (`wtdecoder.ts:120,277-281`), core channel-reopen 1000 ms
  (`wtcore.ts:1103-1111`), host 1000 ms (`session.rs:427-450`). The client's give-up
  cadence (300 ms) is faster than its own ask cadence (500 ms) and faster than the
  host's (1000 ms), so asks are structurally wasted — the measured "three requests, two
  refused" is this, not an anomaly.
* Recovery time is therefore **the first honoured-and-delivered keyframe**, i.e. 1-4 s,
  matching the observation precisely, and matching "recovery was instant when a
  13,328-byte IDR arrived" (the keyframe branch `frameorder.ts:160-168` anchors and the
  while-loop `:200-206` drains the whole held run in one go).

### 2.2 A page-driven keyframe request never reaches the core's channel-reopen logic

`wt.ts:227-229` posts a raw `{type:"keyframe_request"}` → `wtworker.ts:82-83` →
`wtcore.ts:1081-1088` (`send`). The core's own `requestKeyframe()`/`reopenVideoChannel()`
(`wtcore.ts:1090-1111`) is therefore **dead code in worker mode** — grep confirms no
caller: only `wtcore.ts:414` (inside `videoChannelLoop`) ever uses that path, and the page
never invokes `WtVideoClient.requestKeyframe` on the core. The host's whole
fresh-channel guarantee (`transport.rs:640-660`, "keyframes always prefer a FRESH sink";
the comment at `wtcore.ts:396-407` about a cached channel being indistinguishable from a
live one) is unused for page-driven recovery. This is the mechanism that matches
"an IDR of 29,639 bytes did not assemble": the host writes the IDR onto the **cached**
channel with a 500 ms budget (`transport.rs:49-51`), the write times out and the stream
is reset mid-frame, `readOneFrame`'s catch abandons the frame (`wtcore.ts:771-779`), and
the datagram copy is best-effort (`transport.rs:533-537`). Both copies can fail.

### 2.3 A big IDR can be evicted from the worker's assembler by newer deltas

`wtcore.ts:589-596` keeps at most 8 concurrent v4 assemblies and evicts the numeric
oldest with a **tombstone** (`v4Done`), which is precisely the state that makes a
retransmitted fragment of that frame unrecoverable (`:566` returns early; `:598` rejects
the fragment for the fresh partial). A repair IDR is 20-600 fragments (`transport.rs:49`)
and drains over ~50-210 ms at the paced rate; at 60 fps a 16 KB frame is ~1 fragment, so
a 25-fragment IDR spans 25+ newer frame numbers. If more than 8 frames interleave, the
partial **keyed** assembly is the numeric oldest and gets tombstoned. The surviving
partial has no parity (parity for the evicted state was merged only on state creation,
`:583-588`) and its fragments are never re-requested (`v4Done` `:507,566`). Inference:
this is the most likely way for "a 29,639-byte IDR did not assemble" to happen even when
its own fragments were delivered late but complete.

### 2.4 postMessage ordering and the two dedup domains

Ordering across the worker/page boundary is *not* a hazard: one `MessagePort`, FIFO, and
the frame object is structured-cloned whole (`wtworker.ts:58`, `wt.ts:155-157`). What is
a hazard is the **reverse** ordering inside the worker: the datagram path delivers as
soon as `got === cnt` (`wtcore.ts:613`), the stream path as soon as `readExact` completes
(`wtcore.ts:739-780`) — so a *delta* can reach the page before a *keyframe* with a lower
frame number, exactly the case `frameorder.ts:20-36` was written for. That part is
handled.

What is **not** handled is a frame delivered on **both** carriers. The two paths have
disjoint dedup: `v4Done` covers datagrams only (`wtcore.ts:480,507`); the stream path has
none (`wtcore.ts:725-781`). The host deliberately sends a repair IDR on both
(`transport.rs:468-471,615-630`). The page therefore receives the same keyframe twice, and
the second copy takes the `!isPlausibleNext` branch (`frameorder.ts:183-193`), which after
`horizon()` further such arrivals does `pending.clear(); nextSeq=null; resyncNeeded=true`
— a *spurious* resync caused by correct-by-design duplication. Inference (not measured):
this can manufacture extra resync cycles on sessions that are otherwise fine.

### 2.5 `hudTick` is the only watchdog clock, and it is a 2 s pong

`WtRecovery.observe` is called from `play.ts:1257-1288` inside `hudTickInner`, which is
invoked from: first frame (`play.ts:932`), `onClosed` (`:1003`), `onRtt` (`:1006`), and
`onConnected` (`:1161`). In worker mode `onRtt` is driven by the **2 s** ping/pong
(`wtcore.ts:343-351`, re-emitted at `wtworker.ts:60-67`). So:

* the 3 s reset and 10 s redial thresholds (`wtrecovery.ts:41-44`) are sampled at ~2 s
  granularity: a decoder reset fires at the first tick past 3 s, i.e. 3-5 s into a stall;
* the 8 s silent-connection teardown (`play.ts:1253-1256`) is likewise sampled at 2 s;
* a 1-4 s stall usually heals before the watchdog fires, so the watchdog does not rescue
  §2.1 — and when it does fire, `WtDecoder.reset()` (`wtdecoder.ts:507-518`) calls
  `decoder.reset()`, discarding queued chunks, plus `order.reset()` — **it cancels the
  in-flight IDR it is waiting for** and starts another 500 ms-throttled request cycle.

### 2.6 Render throttling / backgrounding

`presentNow` has no rAF dependency (a strength), but the canvas still has to be composited
and the `VideoDecoder` still has to be serviced by the page's task loop. In an occluded or
backgrounded tab Chrome throttles rAF and rendering: `presentedFrames` stops changing
(`wtdecoder.ts:495`), so `WtRecovery.observe` sees a freeze and fires `reset` at the next
tick (4-5 s) even though the decoder is fine — and `performance.now()`-based thresholds
(`wtrecovery.ts:45`) do not stop while hidden. `FrameProbe.measureRefresh` keeps a rAF loop
running the whole session (`diag.ts:152-153,176-184`), which is backgrounded too, so its
`displayHz` (`diag.ts:186-190`) is the readout for this.

### 2.7 Main-thread contention

Per second on the main thread while playing: `stats()` — which allocates and assigns
`window.__inphaseWt`, recomputes rounded fps and resets the sampling window — is called by
the 1 s stats push (`play.ts:1023-1037` → `wt.ts:101-103`) **and** by every `hudTick`
(`play.ts:1238`), whose DOM is rebuilt each time (`hud.ts:148-197`, `renderLine`
`:200-217`); the input snapshot timer at 40 Hz (`input/manager.ts:21,148`) and a gamepad
rAF (`manager.ts:149`, `touch.ts:62`); `wtAudio.drain()` allocates an `AudioBuffer` and a
`Float32Array` per 10 ms Opus frame (`wtaudio.ts:100-119`); `FrameProbe.measureRefresh` per
frame (`diag.ts:176-184`). None of these is large, but they are all on the same thread as
the canvas draw, and `submitTimes`-based decode latency (`wtdecoder.ts:326-330`) shares it.

### 2.8 Reconfiguration discards work

`configure()` (`wtdecoder.ts:185-260`) does `order.reset()`, `keyThrottle.reset()`, a
`decoder.reset()` and then `requestKeyframe()`. `play.ts:971-985` mitigates the
duplicate-config case by comparing codec/size/description — **but it calls
`decoder.configure(cfg)` unconditionally regardless of `same`** (`play.ts:978`), so a
duplicate `video_config` still resets the orderer, the decoder queue and the throttle, and
issues a keyframe request. The "kept the running decoder" log line at `:980-985` is
therefore misleading; the comment at `:965-970` describes behaviour the code does not
implement.

### 2.9 Everything else that discards decoded frames

* backpressure drop path (`wtdecoder.ts:303-325`) — `decoder.reset()` on a keyed frame,
  `framesDropped++` on deltas plus `referenceGap()`;
* decode error handler (`wtdecoder.ts:220-244`) — bounded rebuilds
  (`decoderpolicy.ts:60-79`), and `referenceGap()` on every error;
* bitmap gate (`wtdecoder.ts:442-446`) and the failure branch `:458-462` — both
  `framesDropped++`;
* `stop()` (`wtdecoder.ts:562-567`) and `reset()` (`:507-518`).

---

## 3. Correctness concerns in the worker/page split

1. **Keyframe throttle exists twice, in two threads, with different quotas.**
   `KeyframeThrottle(500)` on the page (`wtdecoder.ts:120`) and `reopenVideoChannel`'s
   1000 ms gate in the worker (`wtcore.ts:1105`) — and neither is informed by the host's
   own 1000 ms refusal (`session.rs:432-450`). The page's count of "requests sent" is not
   the worker's, and neither is the host's honour count. Only the host knows an IDR was
   requested; the client never counts IDRs it asked for.
2. **The worker never learns that a page-side request should reopen the channel** (§2.2).
   Page state (`wt.ts`) and worker state (`wtcore.ts`) disagree about what a keyframe
   request means.
3. **`stats()` is both a getter and a mutator of the fps window** (`wtdecoder.ts:520-530`:
   it resets `statsSnap` on every call). Because `hudTick` (2 s) and the stats push (1 s)
   both call it, the "per-window rate" windows are set by whichever caller ran last; the
   telemetry deltas in `sendTelemetry` (`wtcore.ts:930-932`) use a *separate* window
   (`winStartedMs`, `:894`) so rates are not double-counted, but they are not the same
   interval as the ones `stats()` believes it computed.
4. **The last-500-ms keyframe-request state does not travel with the stats push**:
   `wt.parsed push` (`play.ts:1023-1037`) sends only counters (`WtClientStats`,
   `wtcore.ts:61-79`). The worker cannot see page-side throttling, hold depth, or the
   orderer's resync state except through `held`/`queueSize`/`behindEvents`.
5. **Double delivery of one keyframe** (§2.4) — two dedup domains for two carriers.
6. **A keyframe can anchor backwards.** `frameorder.ts:160-168` sets `nextSeq = f.frame_no`
   unconditionally. If a stale keyframe arrives after the cursor has already passed it
   (`isBefore(f.frame_no, nextSeq)` is checked only in the `delta` branch at `:175`), the
   keyframe is submitted to the decoder (`:197-206`) even though everything ahead of it
   was already decoded — with an infinite GOP that poisons the reference chain. The
   `farAhead` counter (`:183-193`) is the only protection and it only sees the mirror
   case.
7. **Over-release on a late keyframe.** The keyframe branch prunes `pending` with
   `isPlausibleNext` (`:161-165`) but the release loop at `:200-206` emits *whatever
   `nextSeq` walks into*, without re-checking plausibility. A previous pipeline's frames
   numbered far ahead (the case `:132-147` documents) that were already buffered while
   `nextSeq === null` are therefore released and fed to the decoder.
8. **Counters that can be double-counted on the page:** a frame refused by the bitmap gate
   is `framesDropped++` (`:443`) and could also be counted by the reactive backpressure
   path in a later submission; `framesDropped` mixes three different causes (backpressure,
   bitmap gate, bitmap failure) with no breakdown, so the telemetry field
   `frames_dropped` (`wtcore.ts:938`, `play.ts:1032`) cannot separate "we skipped forward"
   from "the renderer refused".
9. **Nothing on the page knows a frame's worker-side age.** `WtFrame` carries
   `capture_us` (`wtvideo.ts:25-30`) but no worker receive/emit stamp, and the page has
   only the offset from the *worker's* pong (`wt.ts:181-190`). `decodeMs`
   (`wtdecoder.ts:380`) therefore measures submit→output, excluding the postMessage wait
   entirely — a 100 ms worker stall is invisible in every page counter.

---

## 4. Ranked fixes

### 4.1 (highest) `frameorder.ts` — do not discard the run that is waiting for an IDR

`budgetSpent` should only be fatal for a hole in an **anchored** sequence. When
`nextSeq === null` (no anchor) the pending run is not "held behind a hole" — it is held
behind a keyframe the client has already asked for, and destroying it (`:93`) guarantees
the IDR that finally arrives anchors nothing. Add an `awaitingKeyMs` stamp set when
`resyncNeeded` is first raised; while `pending.size < MAX_HELD_FRAMES` do not clear the
buffer, and re-request at most once per budget *after* the IDR has had a chance to arrive
(e.g. 2 budget windows). Keep the 64-frame backstop as the escape hatch.

*Why*: it turns the 0→18→reset sawtooth into "hold until the IDR lands", which is the
first keyframe the client is already waiting for; it also removes one keyframe request per
~300 ms of stall, i.e. most of the host's refusals.
*Cost/risk*: up to 64 frames (~1 s, ~10-15 MB at 1440p) held instead of ~18; if the IDR
never comes the escape hatch reproduces today's behaviour. No new timers.
*Test* (`frameorder.test.ts`): feed a keyframe, then 25 deltas with **no** anchor and
assert `needsResync === false` after 300 ms of video and `held > 18`; then deliver the
keyframe and assert the whole run drains in order. Second case: with a genuine anchored
hole, the existing 300 ms give-up behaviour must be unchanged (`:67-81` must still pass).

### 4.2 `wtcore.ts:589-596` — never evict a partial **keyed** assembly, and re-request its missing fragments

The eviction loop picks the numeric oldest; make it skip states whose `key` is true (choose
the oldest non-key partial instead, mirroring the host's own rule at
`transport.rs:105-118`), and raise the concurrent-assembly limit while a partial is keyed.
Additionally, when a key partial is about to be dropped, count it
(`keyPartialsEvicted`) and trigger the recovery ladder rather than dropping it silently.

*Why*: a keyed partial is unrecoverable once tombstoned — its parity arrived on a state
that no longer exists and its fragments are refused (`wtcore.ts:507,566,598`). This is a
direct candidate for "a 29,639-byte IDR did not assemble" (§2.3).
*Cost/risk*: memory for one extra partial frame (~700 KB worst case); tiny branch in a hot
loop (`onVideoFragment`).
*Test* (`wt.parity.test.ts` / `wt.chunks.test.ts` style, both already drive
`onVideoFragment` directly): build `cnt = 24`, `key = true`; feed a partial of frame 100,
then ≥9 complete delta frames so the old eviction would pick 100; deliver the remaining
fragments of 100 and assert `onFrame` fired with `frame_no === 100`. Also assert a
non-key partial *is* evicted (unchanged behaviour).

### 4.3 `wt.ts:227-229` + `wtworker.ts:82-83` — make page requests use the core's request path

Add a worker message (`{t:"keyframe-req"}`) that calls `core.requestKeyframe()` instead of
`core.send({type:"keyframe_request"})`, so `reopenVideoChannel` (`wtcore.ts:1103-1111`) runs
and the IDR gets the fresh channel + the datagram copy the host guarantees. Align the
client's cadence with the host's: page throttle ~750-900 ms so the host's 1000 ms gate is
almost never the thing that refuses.

*Why*: the host only sets `key_by_datagram` and prefers a fresh sink when it *honours* a
request (`transport.rs:468-471,640-660`); today a page request is a bare control message
that gets the cached channel and no datagram copy.
*Cost/risk*: reopening a channel drops whatever was queued on it — acceptable, a request
only goes out when the client has nothing decodable. Reopening is already rate-limited at
1000 ms (`wtcore.ts:1105`); keep that.
*Test*: assert `wt.ts`'s request path reaches the worker with the new tag (small unit test
over a fake `Worker`, following `wt.test.ts`), and that `core.requestKeyframe()` still only
reopens once per second (pure-logic extraction, `decoderpolicy.test.ts` style).

### 4.4 `frameorder.ts` — reject a keyframe that is behind the decode cursor

Track the highest `frame_no` ever released (`accept` return path `:199-206`) and treat a
later keyframe with `isBefore(keyNo, released)` (`:54-56`) as stale: do not submit it, ask
for a fresh IDR instead. This closes §3.6/§3.7 (a backwards anchor poisoning the reference
chain, and over-release from the keyframe branch).

*Cost/risk*: one field plus one branch; a genuinely mis-numbered host would ask for an IDR
(recoverable, and visible in `keys_received`).
*Test* (`frameorder.test.ts`): anchor at 1, release 2..5, then deliver `key` frame 4 →
assert it is not released, assert a fresh keyframe (frame 90, key) is required and decodes
afterwards.

### 4.5 `play.ts:1253-1291` + `wtdecoder.ts` — a watchdog on its own clock, informed by the cause

Move the recovery observation off `hudTick` onto its own 250 ms timer reading a cheap
counter (`framesPresented`); have `WtDecoder` expose "waiting for a keyframe since T" so
`WtRecovery` can suppress `reset` when the freeze's cause is a known reorder hole
(otherwise the reset cancels the IDR it is waiting for, §2.5).

*Cost/risk*: one timer; thresholds unchanged.
*Test* (`wtrecovery.test.ts`): a freeze with an outstanding keyframe request must not
produce `reset` before the request deadline; the existing ladder tests must still pass.

### 4.6 `play.ts:978` — honour the duplicate-config verdict

Move `decoder.configure(cfg)` behind the `!same` check (`:971-978`) and route the duplicate
to the existing "kept the running decoder" branch. Today a duplicate epoch resets the
orderer, the codec queue and the keyframe throttle and forces an IDR (`:978-985`).

*Cost/risk*: none beyond the already-existing comparison.
*Test*: `play.ts` is not unit-tested; assert at the `WtDecoder` level that a no-op
reconfigure is never issued (extract the comparison into a pure helper next to
`decoderpolicy.ts` and test it there).

### 4.7 `wtcore.ts:750-780` — carry a worker-side and a page-side stamp with each frame

Add `workerRxMs` (datagram/stream completion) and `postedMs` to `WtFrame`
(`wtvideo.ts:25-30`) and expose `postMessage age` and `orderer hold` in the decoder stats.
Without this, §2.1-2.3 can only be inferred from `held` snapshots.

*Cost/risk*: two numbers per frame; the wire format is untouched (this is the in-process
frame object, not `parseFrame`).
*Test*: pure type-level change; extend the existing `wt.parity.test.ts` frame-shape
assertions.

### 4.8 Visibility handling

In `WtDecoder`/`Session`, ignore freeze accounting and suspend the reset ladder while
`document.hidden` (`wtdecoder.ts:475-485`, `play.ts:1253-1291`), and stop feeding the
throttle/give-up timers from paused frames.

*Why*: a hidden tab legitimately stops presenting; §2.6 shows it currently looks like a
freeze and costs a decoder reset.
*Test*: extract a `shouldCountFreeze(visibility, gapMs)` predicate next to
`decoderpolicy.ts` and test it.

---

## 5. Verification per fix

| Fix | What proves it |
|---|---|
| 4.1 orderer hold | `decode_held` (`wtcore.ts:974`) no longer returns to 0 every 300 ms during a stall (it should sit ~18-64 and then drop to 0 exactly once, at recovery); host log count of `wt: client requested a keyframe` (`session.rs:238-240`) per minute should fall; `freezeCount/totalFreezeMs` (`wtdecoder.ts:475-485`, HUD via `play.ts:1307-1327`) should stop incrementing for 1-4 s episodes |
| 4.2 key partials | new counter `keyPartialsEvicted` (surface beside `framesAbandoned`, `wtcore.ts:544,949`) stays 0; `keys_received` (`:981`, `:630,758`) becomes ≥1 in the seconds after each request; host `wt: keyframe sent on a freshly installed channel` vs `transport.rs` `stalled` counter |
| 4.3 request path | host log: `wt: client requested a keyframe` count ≈ IDRs sent (refusals ~0); host `wt: keyframe sent on a freshly installed channel` (`transport.rs:648-654`) should be the common case, not the exception; client `nacks_sent` vs `keys_received` ratio improves (`wtcore.ts:980-981`) |
| 4.4 stale anchors | new `staleKeyframes` counter next to `framesAbandoned`; `decode_behind_events` (`wtcore.ts:975`) should stop rising during recovery; `framesDropped` (`wtdecoder.ts:320`) should fall |
| 4.5 watchdog | `freezeCount`/`totalFreezeMs` on the HUD plus the watchdog log lines (`play.ts:1260-1267`) — resets should no longer appear for stalls that self-heal in <3 s |
| 4.6 duplicate config | page console: the existing `wt: duplicate video_config (epoch …)` line (`play.ts:981-985`) must be the *only* effect — no keyframe request right after it (host log) |
| 4.7 per-frame stamps | `window.__inphaseWt` (`wtdecoder.ts:558`) gains worker-hop and hold ages; `lat_p95_ms` (`wtcore.ts:971`) vs `presentEmaMs` (`:549`) should stop disagreeing during stalls |
| 4.8 visibility | `displayHz` in `window.__inphaseFrameStats` (`diag.ts:277`) drops when hidden; `freezeCount` must not move while it is 0 |

Host-side confirmation lines to watch: `wt: repair keyframe sent as datagram fragments too`
(`transport.rs:623-626`), `KEYFRAME DROPPED - no client video channel` (`transport.rs:691-697`),
and the `write_stall_ms`/`stalled` counters in the same file.

---

## 6. Inferences vs. observations

Observed in code (directly read): everything cited above; the exact bounds in §1; the
dead-code claim in §2.2 (grep for callers of `WtCore.requestKeyframe`); the disjoint dedup
domains in §2.4.

Inferences (reasoning, not measured): the fragment-count arithmetic for a 29,639-byte IDR
at the host's datagram budget (`transport.rs:478-483`, `WT_FRAGMENT_HEADER_LEN = 18`,
`crates/protocol/src/wtvideo.rs:555`); that §2.3's eviction, rather than pure fragment loss,
is the likeliest reason that specific IDR never assembled; the exact 2 s sampling offset of
the watchdog (the ping interval is 2000 ms at `wtcore.ts:343-351`, but pong arrival jitter
was not measured); and the magnitude of main-thread contention in §2.7 (no profile was
taken — `diag.ts` has no decoding trace in WT mode because there is no `<video>` element,
`diag.ts:154-161`).
